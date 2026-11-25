//! Glue between config, drives and the engine: registering drives, recognising
//! them when plugged in, and running a backup with locking + history.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

use crate::config::{data_dir, Config, DriveReg, SourceKind};
use crate::engine::{self, Progress, Report, VerifyReport, META_DIR};
use crate::win::{self, NamedLock};

const MARKER: &str = "drive.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Marker {
    pub drive_id: String,
    pub name: String,
}

pub fn marker_path(root: &Path) -> PathBuf {
    root.join(META_DIR).join(MARKER)
}

pub fn read_marker(root: &Path) -> Option<Marker> {
    let bytes = fs::read(marker_path(root)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub enum Identity {
    /// Not a SyncApp drive (or registered on another PC). Ignored.
    Unknown,
    /// Marker present and matches a registered drive.
    Registered(DriveReg),
    /// Marker id matches but the volume serial doesn't: a copied marker or a
    /// reformatted drive. Refused for safety; re-register to fix.
    SerialMismatch(DriveReg),
}

pub fn identify(cfg: &Config, root: &Path) -> Identity {
    let Some(m) = read_marker(root) else { return Identity::Unknown };
    let Some(reg) = cfg.drive(&m.drive_id) else { return Identity::Unknown };
    if let Some(expected) = reg.volume_serial {
        let actual = win::volume_info(root).map(|v| v.serial);
        if actual != Some(expected) {
            return Identity::SerialMismatch(reg.clone());
        }
    }
    Identity::Registered(reg.clone())
}

/// Mark a drive as a backup target. Re-registering an already-marked drive keeps its id.
pub fn register_drive(cfg: &mut Config, root: &str, name: Option<&str>) -> Result<DriveReg, String> {
    let root = win::normalize_root(root)?;
    if root == win::system_root() {
        return Err("Refusing to use the Windows system drive as a backup target".into());
    }
    let rp = Path::new(&root);
    let vi = win::volume_info(rp).ok_or_else(|| format!("Drive {root} is not ready"))?;
    let drive_id = read_marker(rp).map(|m| m.drive_id).unwrap_or_else(win::random_id);
    let name = name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| if vi.label.is_empty() { format!("Drive {}", &root[..2]) } else { vi.label.clone() });

    let marker = Marker { drive_id: drive_id.clone(), name: name.clone() };
    let meta = rp.join(META_DIR);
    fs::create_dir_all(&meta).map_err(|e| format!("Cannot write to {root}: {e}"))?;
    win::set_hidden(&meta);
    engine::atomic_write(&marker_path(rp), &serde_json::to_vec_pretty(&marker).unwrap())
        .map_err(|e| format!("Cannot write to {root}: {e}"))?;

    let reg = match cfg.drive_mut(&drive_id) {
        Some(existing) => {
            existing.name = name;
            existing.volume_serial = Some(vi.serial);
            existing.clone()
        }
        None => {
            let reg = DriveReg {
                drive_id,
                name,
                volume_serial: Some(vi.serial),
                auto_backup: true,
                sources: None,
                schedule: Default::default(),
                repeat_minutes: 0,
                registered_at: win::now_unix(),
            };
            cfg.drives.push(reg.clone());
            reg
        }
    };
    cfg.save()?;
    crate::info!("Registered drive {} ({}) as '{}'", root, reg.drive_id, reg.name);
    Ok(reg)
}

/// Stop backing up to a drive. Its existing backup files are left untouched.
pub fn unregister_drive(cfg: &mut Config, drive_id: &str) -> Result<(), String> {
    let before = cfg.drives.len();
    cfg.drives.retain(|d| d.drive_id != drive_id);
    if cfg.drives.len() == before {
        return Err("No such drive".into());
    }
    // If it's connected, remove its marker so it's clearly no longer a target.
    for root in win::candidate_drives() {
        let rp = Path::new(&root);
        if read_marker(rp).is_some_and(|m| m.drive_id == drive_id) {
            let _ = fs::remove_file(marker_path(rp));
        }
    }
    cfg.save()
}

/// Registered drives currently plugged in: (root, registration).
pub fn connected_drives(cfg: &Config) -> Vec<(String, DriveReg)> {
    win::candidate_drives()
        .into_iter()
        .filter_map(|root| match identify(cfg, Path::new(&root)) {
            Identity::Registered(r) => Some((root, r)),
            _ => None,
        })
        .collect()
}

pub fn find_connected(cfg: &Config, drive_id: &str) -> Option<String> {
    connected_drives(cfg).into_iter().find(|(_, r)| r.drive_id == drive_id).map(|(root, _)| root)
}

fn drive_lock_name(id: &str) -> String {
    format!("Local\\SyncApp-Drive-{id}")
}

pub fn is_busy(drive_id: &str) -> bool {
    match NamedLock::try_acquire(&drive_lock_name(drive_id)) {
        Ok(Some(_l)) => false,
        _ => true,
    }
}

/// Run a backup with the per-drive lock held, and record it in history.
pub fn backup_drive(cfg: &Config, root: &str, reg: &DriveReg, cancel: &AtomicBool, progress: Option<Progress>) -> Result<Report, String> {
    let _lock = NamedLock::try_acquire(&drive_lock_name(&reg.drive_id))
        .map_err(|e| e.to_string())?
        .ok_or("A backup to this drive is already running")?;
    let sources = cfg.sources_for(reg);
    if sources.is_empty() {
        return Err("No sources selected for this drive".into());
    }
    crate::info!("Backup to '{}' ({root}) started: {} source(s)", reg.name, sources.len());
    let report = engine::run_backup(&sources, Path::new(root), &cfg.pc(), cancel, progress);
    crate::info!("Backup to '{}' finished: {}", reg.name, report.summary());
    for (k, e) in report.failed.iter().take(20) {
        crate::warn!("  failed: {k}: {e}");
    }
    history_append(HistoryRecord::from_backup(reg, &report));
    Ok(report)
}

pub fn verify(root: &str, reg: &DriveReg, cancel: &AtomicBool, progress: Option<Progress>) -> Result<VerifyReport, String> {
    let _lock = NamedLock::try_acquire(&drive_lock_name(&reg.drive_id))
        .map_err(|e| e.to_string())?
        .ok_or("This drive is busy with a backup")?;
    let r = engine::verify_drive(Path::new(root), cancel, progress);
    crate::info!(
        "Verify of '{}': {} ok, {} corrupted, {} missing",
        reg.name, r.ok, r.corrupted.len(), r.missing.len()
    );
    history_append(HistoryRecord::from_verify(reg, &r));
    let saved = SavedVerify { finished_at: win::now_unix(), drive_name: reg.name.clone(), report: r.clone() };
    if let Ok(b) = serde_json::to_vec(&saved) {
        let _ = engine::atomic_write(&verify_report_path(&reg.drive_id), &b);
    }
    Ok(r)
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SavedVerify {
    pub finished_at: u64,
    pub drive_name: String,
    pub report: VerifyReport,
}

fn verify_report_path(drive_id: &str) -> PathBuf {
    data_dir().join(format!("verify-{drive_id}.json"))
}

/// The most recent full verification report for a drive.
pub fn last_verify(drive_id: &str) -> Option<SavedVerify> {
    fs::read(verify_report_path(drive_id)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// Restore selected files (key -> destination) from a drive, with the drive lock held.
pub fn restore_files(
    root: &str,
    reg: &DriveReg,
    files: &[(String, PathBuf)],
    overwrite: bool,
    description: &str,
    cancel: &AtomicBool,
    progress: Option<Progress>,
) -> Result<Report, String> {
    let _lock = NamedLock::try_acquire(&drive_lock_name(&reg.drive_id))
        .map_err(|e| e.to_string())?
        .ok_or("This drive is busy with a backup")?;
    crate::info!("Restore of {} file(s) from '{}': {description}", files.len(), reg.name);
    let r = engine::restore_files(Path::new(root), files, overwrite, cancel, progress);
    crate::info!("Restore finished: {}", r.summary());
    let mut h = HistoryRecord::from_backup(reg, &r);
    h.kind = "restore".into();
    h.detail = description.to_string();
    history_append(h);
    Ok(r)
}

/// Where a backed-up file originally lived on this PC, if its source is still configured.
///
/// Works even when the source has since been deleted from the PC (the usual reason to
/// restore): the source's recorded kind decides; for old configs without one, a key whose
/// remainder is exactly the source's own file name means a single-file source, anything
/// else means a folder.
pub fn original_path(cfg: &Config, key: &str) -> Option<PathBuf> {
    let (name, rel) = key.split_once('/')?;
    let src = cfg.sources.iter().find(|s| s.name.eq_ignore_ascii_case(name))?;
    let file_name = src.path.file_name()?.to_string_lossy().into_owned();
    let is_file = match src.kind() {
        Some(SourceKind::File) => true,
        Some(SourceKind::Folder) => false,
        None => rel.eq_ignore_ascii_case(&file_name),
    };
    if is_file {
        // Single-file source: backed up as "<name>/<file name>".
        if rel.eq_ignore_ascii_case(&file_name) {
            Some(src.path.clone())
        } else {
            src.path.parent().map(|p| engine::dest_path(p, rel))
        }
    } else {
        Some(engine::dest_path(&src.path, rel))
    }
}

/// Text for a tray notification after a backup: (title, body, is_problem).
pub fn describe(reg: &DriveReg, root: &str, r: &Report) -> (String, String, bool) {
    let letter = &root[..2];
    if let Some(a) = &r.aborted {
        return (
            format!("Backup to {} stopped", reg.name),
            format!(
                "{a}. {} file(s) were updated first. Files already on the drive are intact.",
                r.copied.len()
            ),
            true,
        );
    }
    let mut body = if r.copied.is_empty() {
        "No changes since the last backup. Nothing was written.".to_string()
    } else {
        format!("{} file(s) updated ({}).", r.copied.len(), engine::human_bytes(r.bytes_copied))
    };
    if !r.failed.is_empty() {
        body += &format!(" {} file(s) couldn't be copied (open in another app?). See History.", r.failed.len());
    }
    if !r.missing_sources.is_empty() {
        body += &format!(" {} source folder(s) not found.", r.missing_sources.len());
    }
    if !r.newer_on_drive.is_empty() {
        let from = &r.newer_on_drive[0].1;
        body += &format!(
            " {} file(s) weren't updated because the drive already has newer versions from {from}.",
            r.newer_on_drive.len()
        );
    }
    if r.ok() {
        body += &format!(" Safe to remove {letter}.");
    }
    (format!("Backup to {} complete", reg.name), body, !r.ok())
}

// ------------------------------------------------------------------ history

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct HistoryRecord {
    pub kind: String, // "backup" | "verify" | "restore"
    pub drive_id: String,
    pub drive_name: String,
    pub finished_at: u64,
    pub ok: bool,
    pub copied: u64,
    pub unchanged: u64,
    pub bytes: u64,
    pub duration_ms: u64,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub errors: Vec<(String, String)>,
}

impl HistoryRecord {
    fn from_backup(reg: &DriveReg, r: &Report) -> Self {
        let mut errors = r.failed.clone();
        errors.extend(r.missing_sources.iter().map(|s| (s.clone(), "source not found".to_string())));
        errors.truncate(HISTORY_ERRORS_MAX);
        HistoryRecord {
            kind: "backup".into(),
            drive_id: reg.drive_id.clone(),
            drive_name: reg.name.clone(),
            finished_at: win::now_unix(),
            ok: r.ok(),
            copied: r.copied.len() as u64,
            unchanged: r.unchanged + r.refreshed,
            bytes: r.bytes_copied,
            duration_ms: r.duration_ms,
            detail: r.aborted.clone().map(|a| format!("Stopped: {a}")).unwrap_or_default(),
            errors,
        }
    }

    fn from_verify(reg: &DriveReg, r: &VerifyReport) -> Self {
        let mut errors: Vec<(String, String)> = r.corrupted.iter().map(|k| (k.clone(), "corrupted".into())).collect();
        errors.extend(r.missing.iter().map(|k| (k.clone(), "missing from drive".into())));
        errors.truncate(HISTORY_ERRORS_MAX);
        HistoryRecord {
            kind: "verify".into(),
            drive_id: reg.drive_id.clone(),
            drive_name: reg.name.clone(),
            finished_at: win::now_unix(),
            ok: errors.is_empty() && r.aborted.is_none(),
            copied: 0,
            unchanged: r.ok,
            bytes: 0,
            duration_ms: r.duration_ms,
            detail: r.aborted.clone().unwrap_or_default(),
            errors,
        }
    }
}

const HISTORY_MAX: usize = 500;
/// Per-entry cap on stored error lines; the log file has the full list.
const HISTORY_ERRORS_MAX: usize = 50;

fn history_path() -> PathBuf {
    data_dir().join("history.json")
}

pub fn history_load() -> Vec<HistoryRecord> {
    fs::read(history_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn history_append(rec: HistoryRecord) {
    let _lock = NamedLock::acquire_wait("Local\\SyncApp-History", 5000).ok().flatten();
    let mut h = history_load();
    h.push(rec);
    if h.len() > HISTORY_MAX {
        let excess = h.len() - HISTORY_MAX;
        h.drain(..excess);
    }
    if let Ok(b) = serde_json::to_vec(&h) {
        let _ = engine::atomic_write(&history_path(), &b);
    }
}

pub fn history_path_pub() -> PathBuf {
    history_path()
}

/// Finish time of the latest backup for every drive, from one read of the history.
pub fn last_backups() -> std::collections::HashMap<String, u64> {
    let mut m = std::collections::HashMap::new();
    for r in history_load() {
        if r.kind == "backup" {
            m.insert(r.drive_id, r.finished_at);
        }
    }
    m
}

pub fn last_backup(drive_id: &str) -> Option<HistoryRecord> {
    history_load().into_iter().rev().find(|r| r.drive_id == drive_id && r.kind == "backup")
}

// ------------------------------------------------------------------ clean-up

/// Backed-up files on this drive that no PC needs any more.
pub fn find_orphans(cfg: &Config, root: &str) -> Vec<engine::Orphan> {
    let names: Vec<String> = cfg.sources.iter().map(|s| s.name.clone()).collect();
    engine::find_orphans(Path::new(root), &names, &cfg.pc().id)
}

/// Permanently delete the given backed-up files, with the drive lock held, and log it.
pub fn clean_up(
    root: &str,
    reg: &DriveReg,
    keys: &[String],
    cancel: &AtomicBool,
    progress: Option<Progress>,
) -> Result<engine::CleanupReport, String> {
    let _lock = NamedLock::try_acquire(&drive_lock_name(&reg.drive_id))
        .map_err(|e| e.to_string())?
        .ok_or("This drive is busy with a backup")?;
    crate::info!("Clean-up of {} file(s) on '{}'", keys.len(), reg.name);
    let r = engine::remove_backed_up(Path::new(root), keys, cancel, progress);
    crate::info!("Clean-up finished: {} removed ({}), {} failed", r.removed, engine::human_bytes(r.bytes), r.failed.len());
    let mut errors = r.failed.clone();
    errors.truncate(HISTORY_ERRORS_MAX);
    history_append(HistoryRecord {
        kind: "cleanup".into(),
        drive_id: reg.drive_id.clone(),
        drive_name: reg.name.clone(),
        finished_at: win::now_unix(),
        ok: r.failed.is_empty() && r.aborted.is_none(),
        copied: r.removed,
        unchanged: 0,
        bytes: r.bytes,
        duration_ms: 0,
        detail: format!("Removed {} file(s) no longer needed", r.removed),
        errors,
    });
    Ok(r)
}
