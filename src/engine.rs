//! Backup engine: decides what changed and copies it safely.
//!
//! Guarantees
//! * Files on the PC are only ever *read* during a backup.
//! * Nothing on the drive is deleted, except SyncApp's own temp files.
//! * A backup copy is only replaced by a new copy that has been fully written,
//!   flushed to the device, read back and checksum-verified. Until that moment the
//!   previous copy stays in place, so unplugging mid-copy, running out of space or
//!   a crash can never leave a broken or missing backup of a file.
//! * If nothing changed since the last backup, nothing is written to the drive.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::Source;

pub const META_DIR: &str = ".syncapp";
pub const BACKUP_DIR: &str = "SyncApp Backup";
pub const MANIFEST: &str = "manifest.json";
pub const TMP_SUFFIX: &str = ".syncapp-tmp";
const CHUNK: usize = 4 << 20; // 4 MiB: large sequential I/O is fastest on flash media
const FREE_MARGIN: u64 = 32 << 20;
const FAT32_MAX_FILE: u64 = (4u64 << 30) - 1;
const COPY_ATTEMPTS: u32 = 3;
const MANIFEST_SAVE_EVERY: Duration = Duration::from_secs(20);
pub const DEFAULT_EXCLUDES: &[&str] = &["~$*", "*.tmp", "thumbs.db", "desktop.ini", "*.syncapp-tmp"];

// ------------------------------------------------------------------ results & progress

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Report {
    pub copied: Vec<String>,
    pub unchanged: u64,
    /// Timestamp changed but content identical: recorded, not copied.
    pub refreshed: u64,
    pub failed: Vec<(String, String)>,
    pub missing_sources: Vec<String>,
    pub bytes_copied: u64,
    pub duration_ms: u64,
    pub aborted: Option<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.failed.is_empty() && self.aborted.is_none() && self.missing_sources.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} updated ({}), {} unchanged",
            self.copied.len(),
            human_bytes(self.bytes_copied),
            self.unchanged + self.refreshed
        );
        if !self.failed.is_empty() {
            s += &format!(", {} failed", self.failed.len());
        }
        if !self.missing_sources.is_empty() {
            s += &format!(", {} source(s) missing", self.missing_sources.len());
        }
        s += &format!(" in {:.1}s", self.duration_ms as f64 / 1000.0);
        if let Some(a) = &self.aborted {
            s = format!("STOPPED ({a}): {s}");
        }
        s
    }
}

pub fn human_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

pub enum Event<'a> {
    /// Scan finished: this many files (and bytes) need work.
    Planned { files: usize, bytes: u64 },
    /// Starting work on a file.
    File { index: usize, total: usize, key: &'a str },
    /// Bytes processed so far (for progress bars).
    Bytes { done: u64, total: u64 },
    /// Verify: a file was checked.
    Checked { ok: bool },
}

pub type Progress<'a> = &'a mut dyn FnMut(Event);

fn noop(_: Event) {}

// ------------------------------------------------------------------ small fs helpers

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub size: u64,
    pub mtime: u64, // FILETIME ticks
}

impl Stamp {
    fn of(md: &fs::Metadata) -> Stamp {
        Stamp { size: md.len(), mtime: md.last_write_time() }
    }
}

fn stamp(path: &Path) -> io::Result<Stamp> {
    fs::metadata(path).map(|m| Stamp::of(&m))
}

fn stamp_opt(path: &Path) -> Option<Stamp> {
    fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| Stamp::of(&m))
}

fn clear_readonly(path: &Path) {
    if let Ok(md) = fs::metadata(path) {
        let mut p = md.permissions();
        if p.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            p.set_readonly(false);
            let _ = fs::set_permissions(path, p);
        }
    }
}

fn remove_quietly(path: &Path) {
    clear_readonly(path);
    let _ = fs::remove_file(path);
}

/// Write a small file so it is either the old version or the new one, never half-written.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let res = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        clear_readonly(path);
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        remove_quietly(&tmp);
    }
    res
}

pub fn hash_file(path: &Path, cancel: &AtomicBool) -> Result<String, Fail> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Fail::Fatal("cancelled".into()));
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_hex().to_string())
}

/// Page-aligned buffer, required for unbuffered (FILE_FLAG_NO_BUFFERING) reads.
struct AlignedBuf {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "out of memory");
        AlignedBuf { ptr, layout }
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}

/// Hash a file reading it straight from the device, bypassing the Windows file
/// cache, so a match proves the bytes actually stored on the drive are correct.
/// Returns (hash, true) when the read was unbuffered; falls back to a normal
/// read (hash, false) on the rare file systems that refuse unbuffered I/O.
pub fn hash_file_direct(path: &Path, cancel: &AtomicBool) -> Result<(String, bool), Fail> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
    let Ok(mut f) = fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_NO_BUFFERING).open(path) else {
        return hash_file(path, cancel).map(|h| (h, false));
    };
    let mut buf = AlignedBuf::new(CHUNK);
    let mut h = blake3::Hasher::new();
    let mut first = true;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Fail::Fatal("cancelled".into()));
        }
        let n = match f.read(buf.as_mut_slice()) {
            Ok(n) => n,
            // ERROR_INVALID_PARAMETER: unusual sector size; use a normal read instead.
            Err(e) if first && e.raw_os_error() == Some(87) => return hash_file(path, cancel).map(|h| (h, false)),
            Err(e) => return Err(e.into()),
        };
        first = false;
        if n == 0 {
            break;
        }
        h.update(&buf.as_mut_slice()[..n]);
    }
    Ok((h.finalize().to_hex().to_string(), true))
}

/// Case-insensitive wildcard match supporting * and ?.
pub fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ------------------------------------------------------------------ errors

#[derive(Debug)]
pub enum Fail {
    /// This file failed; carry on with the others.
    File(String),
    /// Stop the whole run (drive removed, drive full, cancelled).
    Fatal(String),
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        // ERROR_DISK_FULL (112) / ERROR_HANDLE_DISK_FULL (39)
        match e.raw_os_error() {
            Some(112) | Some(39) => Fail::Fatal("drive is full".into()),
            _ => Fail::File(e.to_string()),
        }
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::File(s) | Fail::Fatal(s) => f.write_str(s),
        }
    }
}

// ------------------------------------------------------------------ manifest

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub size: u64,
    pub mtime: u64,
    pub hash: String,
    pub dest_size: u64,
    pub dest_mtime: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct ManifestFile {
    version: u32,
    files: BTreeMap<String, Entry>,
}

/// Per-drive record of what was backed up. Lets unchanged files be skipped with
/// a single metadata lookup, without re-reading them.
///
/// It is purely a cache: if it's lost or corrupted, the engine falls back to
/// comparing checksums of the PC file and the drive copy, so it can never cause
/// data loss or needless copies of identical files.
pub struct Manifest {
    path: PathBuf,
    pub files: BTreeMap<String, Entry>,
    dirty: bool,
}

impl Manifest {
    pub fn path_for(drive_root: &Path) -> PathBuf {
        drive_root.join(META_DIR).join(MANIFEST)
    }

    pub fn load(drive_root: &Path) -> Manifest {
        let path = Self::path_for(drive_root);
        let files = match fs::read(&path) {
            Ok(b) => match serde_json::from_slice::<ManifestFile>(&b) {
                Ok(m) => m.files,
                Err(e) => {
                    crate::warn!("Manifest on {} unreadable ({e}); using checksum comparison", drive_root.display());
                    BTreeMap::new()
                }
            },
            Err(_) => BTreeMap::new(),
        };
        Manifest { path, files, dirty: false }
    }

    fn record(&mut self, key: &str, src: Stamp, hash: String, dest: Stamp) {
        self.files.insert(
            key.to_string(),
            Entry { size: src.size, mtime: src.mtime, hash, dest_size: dest.size, dest_mtime: dest.mtime },
        );
        self.dirty = true;
    }

    pub fn save(&mut self) -> io::Result<()> {
        let mf = ManifestFile { version: 1, files: std::mem::take(&mut self.files) };
        let res = serde_json::to_vec(&mf).map_err(io::Error::other).and_then(|b| atomic_write(&self.path, &b));
        self.files = mf.files;
        if res.is_ok() {
            self.dirty = false;
        }
        res
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.values().map(|e| e.size).sum()
    }
}

// ------------------------------------------------------------------ scanning

struct Item {
    key: String,
    path: PathBuf,
    stamp: Stamp,
}

/// List every file under `root`, keyed "prefix/rel/path". Symlinks and junctions
/// are skipped so a link can't pull in files from outside the chosen folder or loop.
fn walk(root: &Path, prefix: &str, excludes: &[String], report: &mut Report, out: &mut Vec<Item>) {
    let md = match fs::metadata(root) {
        Ok(m) => m,
        Err(_) => {
            report.missing_sources.push(root.display().to_string());
            crate::warn!("Source not found, skipped: {}", root.display());
            return;
        }
    };
    if md.is_file() {
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push(Item { key: format!("{prefix}/{name}"), path: root.to_path_buf(), stamp: Stamp::of(&md) });
        return;
    }
    let excluded = |rel: &str, name: &str| {
        DEFAULT_EXCLUDES.iter().map(|s| *s).chain(excludes.iter().map(|s| s.as_str()))
            .any(|p| wildcard(p, name) || wildcard(p, rel))
    };
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel_dir)) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                report.failed.push((format!("{prefix}/{rel_dir}"), format!("cannot list folder: {e}")));
                continue;
            }
        };
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    report.failed.push((format!("{prefix}/{rel_dir}"), e.to_string()));
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if rel_dir.is_empty() { name.clone() } else { format!("{rel_dir}/{name}") };
            if excluded(&rel, &name) {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(e) => {
                    report.failed.push((format!("{prefix}/{rel}"), e.to_string()));
                    continue;
                }
            };
            if ft.is_symlink() {
                continue; // includes junctions (name-surrogate reparse points)
            } else if ft.is_dir() {
                stack.push((entry.path(), rel));
            } else if ft.is_file() {
                match entry.metadata() {
                    Ok(md) => out.push(Item { key: format!("{prefix}/{rel}"), path: entry.path(), stamp: Stamp::of(&md) }),
                    Err(e) => report.failed.push((format!("{prefix}/{rel}"), e.to_string())),
                }
            }
        }
    }
}

// ------------------------------------------------------------------ decisions

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Copy,
    /// Same size, new timestamp: compare the PC file's hash with the recorded one.
    CheckEntry,
    /// No trustworthy record: compare hashes of the PC file and the drive copy.
    CheckDest,
}

/// None = unchanged, nothing to do.
pub fn decide(src: Stamp, dest: Option<Stamp>, entry: Option<&Entry>) -> Option<Action> {
    let Some(dest) = dest else { return Some(Action::Copy) };
    let trusted = entry.is_some_and(|e| dest.size == e.dest_size && dest.mtime == e.dest_mtime);
    if trusted {
        let e = entry.unwrap();
        if src.size == e.size && src.mtime == e.mtime {
            return None;
        }
        if src.size != e.size {
            return Some(Action::Copy);
        }
        return Some(Action::CheckEntry);
    }
    // The drive copy was changed outside SyncApp, or the manifest is missing.
    if dest.size != src.size { Some(Action::Copy) } else { Some(Action::CheckDest) }
}

// ------------------------------------------------------------------ copying

struct CopyCtx<'a> {
    /// Where free space is checked (destination volume).
    space_root: PathBuf,
    fat32: bool,
    cancel: &'a AtomicBool,
    bytes_done: u64,
    bytes_total: u64,
}

/// Copy src -> dest atomically and verified. Returns (hash, src stamp, dest stamp).
///
/// 1. Write to a temp file beside the destination, hashing as we go.
/// 2. Flush it to the physical device.
/// 3. Make sure the source didn't change while we were reading it (else retry).
/// 4. Read the temp file back and compare checksums.
/// 5. Atomically swap it into place; the old copy survives until this instant.
fn copy_verified(
    src: &Path,
    dest: &Path,
    expect_hash: Option<&str>,
    ctx: &mut CopyCtx,
    progress: Progress,
) -> Result<(String, Stamp, Stamp), Fail> {
    let parent = dest.parent().ok_or_else(|| Fail::File("bad destination".into()))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}{TMP_SUFFIX}", dest.file_name().unwrap().to_string_lossy()));
    let mut buf = vec![0u8; CHUNK];
    let start_done = ctx.bytes_done;

    for _ in 0..COPY_ATTEMPTS {
        ctx.bytes_done = start_done;
        let md = fs::metadata(src)?;
        let before = Stamp::of(&md);
        if ctx.fat32 && before.size > FAT32_MAX_FILE {
            return Err(Fail::File(format!(
                "{} file can't be stored on a FAT32 drive (4 GB limit); reformat the drive as exFAT or NTFS",
                human_bytes(before.size)
            )));
        }
        let (free, _) = crate::win::disk_space(&ctx.space_root)?;
        if free < before.size + FREE_MARGIN {
            return Err(Fail::Fatal(format!(
                "not enough space: need {}, {} free",
                human_bytes(before.size),
                human_bytes(free)
            )));
        }

        let attempt = (|| -> Result<Option<(String, Stamp, Stamp)>, Fail> {
            let mut fi = File::open(src)?;
            let mut fo = File::create(&tmp)?;
            let mut h = blake3::Hasher::new();
            loop {
                if ctx.cancel.load(Ordering::Relaxed) {
                    return Err(Fail::Fatal("cancelled".into()));
                }
                let n = fi.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                fo.write_all(&buf[..n])?;
                ctx.bytes_done += n as u64;
                progress(Event::Bytes { done: ctx.bytes_done, total: ctx.bytes_total });
            }
            if let Ok(t) = md.modified() {
                fo.set_modified(t)?;
            }
            fo.sync_all()?; // FlushFileBuffers: data is on the device, not in a cache
            drop(fo);
            drop(fi);

            let after = stamp(src)?;
            if after != before {
                return Ok(None); // file was being edited; try again
            }
            let digest = h.finalize().to_hex().to_string();
            if let Some(exp) = expect_hash {
                if exp != digest {
                    return Err(Fail::File("backup copy is corrupted (checksum doesn't match the one recorded at backup time)".into()));
                }
            }
            if fs::metadata(&tmp)?.len() != after.size {
                return Err(Fail::File("size mismatch after writing (faulty drive?)".into()));
            }
            if hash_file_direct(&tmp, ctx.cancel)?.0 != digest {
                return Err(Fail::File("checksum mismatch after writing (faulty drive or cable?)".into()));
            }
            clear_readonly(dest);
            fs::rename(&tmp, dest)?;
            Ok(Some((digest, after, stamp(dest)?)))
        })();

        match attempt {
            Ok(Some(r)) => return Ok(r),
            Ok(None) => {
                remove_quietly(&tmp);
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                remove_quietly(&tmp);
                return Err(e);
            }
        }
    }
    Err(Fail::File("file kept changing while being copied; will retry next backup".into()))
}

fn is_fat32(root: &Path) -> bool {
    crate::win::volume_info(root).is_some_and(|v| v.fs_name.eq_ignore_ascii_case("FAT32") || v.fs_name.eq_ignore_ascii_case("FAT"))
}

// ------------------------------------------------------------------ backup

pub fn run_backup(sources: &[Source], drive_root: &Path, cancel: &AtomicBool, progress: Option<Progress>) -> Report {
    let mut noop_cb = noop;
    let progress: Progress = match progress {
        Some(p) => p,
        None => &mut noop_cb,
    };
    let t0 = Instant::now();
    let mut report = Report::default();
    let backup_dir = drive_root.join(BACKUP_DIR);
    let mut manifest = Manifest::load(drive_root);

    // Phase 1: scan. Metadata only; nothing is read or written.
    let mut items = Vec::new();
    for s in sources {
        walk(&s.path, &s.name, &s.exclude, &mut report, &mut items);
    }
    let mut plan = Vec::new();
    for it in items {
        let dest = dest_path(&backup_dir, &it.key);
        match decide(it.stamp, stamp_opt(&dest), manifest.files.get(&it.key)) {
            None => report.unchanged += 1,
            Some(a) => plan.push((it, dest, a)),
        }
    }
    let bytes_total: u64 = plan.iter().map(|p| p.0.stamp.size).sum();
    progress(Event::Planned { files: plan.len(), bytes: bytes_total });

    // Phase 2: act on the (usually short) list of candidates.
    let mut ctx = CopyCtx { space_root: drive_root.to_path_buf(), fat32: is_fat32(drive_root), cancel, bytes_done: 0, bytes_total };
    let mut last_save = Instant::now();
    let total = plan.len();
    for (i, (it, dest, action)) in plan.iter().enumerate() {
        if !drive_root.exists() {
            report.aborted = Some("drive was removed".into());
            break;
        }
        progress(Event::File { index: i + 1, total, key: &it.key });
        let res = (|| -> Result<bool, Fail> {
            if *action != Action::Copy && identical(&it.path, dest, &mut manifest, &it.key, *action, cancel)? {
                return Ok(false);
            }
            let (hash, s, d) = copy_verified(&it.path, dest, None, &mut ctx, &mut *progress)?;
            manifest.record(&it.key, s, hash, d);
            Ok(true)
        })();
        match res {
            Ok(true) => {
                report.copied.push(it.key.clone());
                report.bytes_copied += it.stamp.size;
                crate::info!("Backed up {}", it.key);
            }
            Ok(false) => report.refreshed += 1,
            Err(Fail::Fatal(msg)) => {
                report.aborted = Some(msg);
                break;
            }
            Err(Fail::File(msg)) => {
                if !drive_root.exists() {
                    report.aborted = Some("drive was removed".into());
                    break;
                }
                crate::warn!("Failed {}: {msg}", it.key);
                report.failed.push((it.key.clone(), msg));
            }
        }
        if manifest.dirty && last_save.elapsed() > MANIFEST_SAVE_EVERY {
            let _ = manifest.save();
            last_save = Instant::now();
        }
    }
    if manifest.dirty && drive_root.exists() {
        if let Err(e) = manifest.save() {
            crate::warn!("Could not save manifest: {e}");
        }
    }
    report.duration_ms = t0.elapsed().as_millis() as u64;
    report
}

pub fn dest_path(base: &Path, key: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    p.extend(key.split('/'));
    p
}

/// Checksum-compare a file whose timestamp changed but size didn't.
fn identical(src: &Path, dest: &Path, m: &mut Manifest, key: &str, action: Action, cancel: &AtomicBool) -> Result<bool, Fail> {
    let before = stamp(src)?;
    let h = hash_file(src, cancel)?;
    if stamp(src)? != before {
        return Ok(false);
    }
    let reference = match action {
        Action::CheckEntry => m.files.get(key).map(|e| e.hash.clone()).unwrap_or_default(),
        _ => hash_file(dest, cancel)?,
    };
    if h != reference {
        return Ok(false);
    }
    m.record(key, before, h, stamp(dest)?);
    Ok(true)
}

// ------------------------------------------------------------------ verify

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyStatus {
    /// Checksum read from the drive matches the one recorded at backup time.
    Ok,
    /// Contents differ from what was written: re-copied on the next backup.
    Corrupted,
    /// Recorded in the manifest but the file is gone from the drive.
    Missing,
    /// The drive returned a read error for this file.
    Unreadable,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VerifyItem {
    pub key: String,
    pub size: u64,
    /// BLAKE3 checksum recorded when the file was backed up.
    pub expected: String,
    /// BLAKE3 checksum computed just now from the drive (None if unreadable/missing).
    pub actual: Option<String>,
    pub status: VerifyStatus,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct VerifyReport {
    pub items: Vec<VerifyItem>,
    pub ok: u64,
    pub corrupted: Vec<String>,
    pub missing: Vec<String>,
    /// Files in the backup folder that SyncApp has no checksum for (e.g. copied there by hand).
    pub untracked: Vec<String>,
    pub bytes_read: u64,
    /// True when every file was read bypassing the Windows cache.
    pub direct_io: bool,
    pub aborted: Option<String>,
    pub duration_ms: u64,
}

impl VerifyReport {
    pub fn problems(&self) -> usize {
        self.items.iter().filter(|i| i.status != VerifyStatus::Ok).count()
    }
    pub fn mb_per_s(&self) -> f64 {
        if self.duration_ms == 0 { 0.0 } else { self.bytes_read as f64 / 1e6 / (self.duration_ms as f64 / 1000.0) }
    }
}

/// Re-read every backed-up file from the device and compare its checksum with the
/// one recorded at backup time. Corrupted or missing files are dropped from the
/// manifest so the next backup re-copies them from the PC.
pub fn verify_drive(drive_root: &Path, cancel: &AtomicBool, progress: Option<Progress>) -> VerifyReport {
    let mut noop_cb = noop;
    let progress: Progress = match progress {
        Some(p) => p,
        None => &mut noop_cb,
    };
    let t0 = Instant::now();
    let mut m = Manifest::load(drive_root);
    let base = drive_root.join(BACKUP_DIR);
    let mut r = VerifyReport { direct_io: true, ..Default::default() };
    let total_bytes = m.total_bytes();
    let keys: Vec<(String, Entry)> = m.files.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    progress(Event::Planned { files: keys.len(), bytes: total_bytes });
    for (i, (key, e)) in keys.iter().enumerate() {
        progress(Event::File { index: i + 1, total: keys.len(), key });
        let dest = dest_path(&base, key);
        let mut item = VerifyItem {
            key: key.clone(),
            size: e.size,
            expected: e.hash.clone(),
            actual: None,
            status: VerifyStatus::Ok,
            error: None,
        };
        match hash_file_direct(&dest, cancel) {
            Ok((h, direct)) => {
                r.direct_io &= direct;
                r.bytes_read += e.size;
                item.status = if h == e.hash { VerifyStatus::Ok } else { VerifyStatus::Corrupted };
                item.actual = Some(h);
            }
            Err(Fail::Fatal(msg)) => {
                r.aborted = Some(msg);
                break;
            }
            Err(Fail::File(msg)) => {
                if !drive_root.exists() {
                    r.aborted = Some("drive was removed".into());
                    break;
                }
                item.status = if dest.exists() { VerifyStatus::Unreadable } else { VerifyStatus::Missing };
                item.error = Some(msg);
            }
        }
        match item.status {
            VerifyStatus::Ok => r.ok += 1,
            VerifyStatus::Missing => r.missing.push(key.clone()),
            _ => r.corrupted.push(key.clone()),
        }
        progress(Event::Checked { ok: item.status == VerifyStatus::Ok });
        progress(Event::Bytes { done: r.bytes_read, total: total_bytes });
        r.items.push(item);
    }
    if r.aborted.is_none() {
        let (files, _) = list_backup(drive_root);
        r.untracked = files.into_iter().filter(|f| f.hash.is_none()).map(|f| f.key).collect();
    }
    let bad: Vec<String> = r.corrupted.iter().chain(r.missing.iter()).cloned().collect();
    if !bad.is_empty() {
        for k in &bad {
            m.files.remove(k);
        }
        let _ = m.save();
    }
    r.duration_ms = t0.elapsed().as_millis() as u64;
    r
}

// ------------------------------------------------------------------ browsing & restore

#[derive(Clone, Debug)]
pub struct BackupFile {
    /// "SourceName/rel/path"
    pub key: String,
    pub size: u64,
    /// Modification time of the PC file when it was backed up (FILETIME), if recorded.
    pub backed_up_mtime: Option<u64>,
    pub hash: Option<String>,
}

/// Every file in the backup folder on a drive, joined with the manifest.
pub fn list_backup(drive_root: &Path) -> (Vec<BackupFile>, Vec<(String, String)>) {
    let manifest = Manifest::load(drive_root);
    let base = drive_root.join(BACKUP_DIR);
    let mut report = Report::default();
    let mut items = Vec::new();
    for name in backed_up_sources(drive_root) {
        walk(&base.join(&name), &name, &[], &mut report, &mut items);
    }
    let mut out: Vec<BackupFile> = items
        .into_iter()
        .map(|it| {
            let e = manifest.files.get(&it.key);
            BackupFile {
                size: e.map(|e| e.size).unwrap_or(it.stamp.size),
                backed_up_mtime: e.map(|e| e.mtime),
                hash: e.map(|e| e.hash.clone()),
                key: it.key,
            }
        })
        .collect();
    out.sort_by(|a, b| a.key.to_lowercase().cmp(&b.key.to_lowercase()));
    (out, report.failed)
}

/// Copy the given backed-up files (key -> destination path) from the drive to the PC.
/// Each copy is checked against the checksum recorded at backup time, written to a
/// temp file, verified and swapped in. Existing files that differ are left alone
/// unless `overwrite` is set; identical ones are skipped.
pub fn restore_files(
    drive_root: &Path,
    files: &[(String, PathBuf)],
    overwrite: bool,
    cancel: &AtomicBool,
    progress: Option<Progress>,
) -> Report {
    let mut noop_cb = noop;
    let progress: Progress = match progress {
        Some(p) => p,
        None => &mut noop_cb,
    };
    let t0 = Instant::now();
    let mut report = Report::default();
    let manifest = Manifest::load(drive_root);
    let base = drive_root.join(BACKUP_DIR);
    let srcs: Vec<(PathBuf, Option<Stamp>)> = files.iter().map(|(k, _)| {
        let p = dest_path(&base, k);
        let s = stamp_opt(&p);
        (p, s)
    }).collect();
    let bytes_total = srcs.iter().filter_map(|s| s.1).map(|s| s.size).sum();
    progress(Event::Planned { files: files.len(), bytes: bytes_total });
    let mut ctx = CopyCtx { space_root: PathBuf::new(), fat32: false, cancel, bytes_done: 0, bytes_total };
    let mut fat_cache: std::collections::HashMap<String, bool> = Default::default();
    for (i, ((key, dest), (src, src_stamp))) in files.iter().zip(srcs.iter()).enumerate() {
        progress(Event::File { index: i + 1, total: files.len(), key });
        if !drive_root.exists() {
            report.aborted = Some("drive was removed".into());
            break;
        }
        let Some(src_stamp) = src_stamp else {
            report.failed.push((key.clone(), "not found on the backup drive".into()));
            continue;
        };
        let vol = crate::win::volume_root(dest).unwrap_or_default();
        ctx.space_root = PathBuf::from(&vol);
        ctx.fat32 = *fat_cache.entry(vol.clone()).or_insert_with(|| is_fat32(Path::new(&vol)));
        let expect = manifest.files.get(key).map(|e| e.hash.as_str());
        let res = (|| -> Result<bool, Fail> {
            if let Some(d) = stamp_opt(dest) {
                if d.size == src_stamp.size {
                    let want = match expect {
                        Some(h) => h.to_string(),
                        None => hash_file(src, cancel)?,
                    };
                    if hash_file(dest, cancel)? == want {
                        return Ok(false);
                    }
                }
                if !overwrite {
                    return Err(Fail::File("a different file already exists there (not replaced)".into()));
                }
            }
            copy_verified(src, dest, expect, &mut ctx, &mut *progress)?;
            Ok(true)
        })();
        match res {
            Ok(true) => {
                report.copied.push(key.clone());
                report.bytes_copied += src_stamp.size;
                crate::info!("Restored {key} -> {}", dest.display());
            }
            Ok(false) => report.unchanged += 1,
            Err(Fail::Fatal(m)) => {
                report.aborted = Some(m);
                break;
            }
            Err(Fail::File(m)) => {
                if !drive_root.exists() {
                    report.aborted = Some("drive was removed".into());
                    break;
                }
                report.failed.push((key.clone(), m));
            }
        }
    }
    report.duration_ms = t0.elapsed().as_millis() as u64;
    report
}

/// Restore a whole backed-up source into `target` (keeping its folder structure).
pub fn restore(
    drive_root: &Path,
    source_name: &str,
    target: &Path,
    overwrite: bool,
    cancel: &AtomicBool,
    progress: Option<Progress>,
) -> Report {
    let prefix = format!("{source_name}/");
    let files: Vec<(String, PathBuf)> = list_backup(drive_root)
        .0
        .into_iter()
        .filter(|f| f.key.starts_with(&prefix))
        .map(|f| {
            let rel = f.key[prefix.len()..].to_string();
            (f.key, dest_path(target, &rel))
        })
        .collect();
    restore_files(drive_root, &files, overwrite, cancel, progress)
}

/// Names of sources that have a backup folder on this drive.
pub fn backed_up_sources(drive_root: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(drive_root.join(BACKUP_DIR))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|s| s.to_lowercase());
    v
}

/// (file count, total bytes) of a source after exclusions. Metadata only.
pub fn source_size(src: &Source) -> (u64, u64) {
    let mut report = Report::default();
    let mut items = Vec::new();
    walk(&src.path, &src.name, &src.exclude, &mut report, &mut items);
    (items.len() as u64, items.iter().map(|i| i.stamp.size).sum())
}

/// FILETIME ticks -> Unix seconds.
pub fn filetime_to_unix(ft: u64) -> u64 {
    (ft / 10_000_000).saturating_sub(11_644_473_600)
}

/// Size and modification time (FILETIME) of a file on the PC, if it exists.
pub fn pc_stamp(path: &Path) -> Option<(u64, u64)> {
    stamp_opt(path).map(|s| (s.size, s.mtime))
}
