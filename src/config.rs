//! User settings: what to back up, and which drives are allowed to receive backups.
//!
//! Stored at %APPDATA%\SyncApp\config.json (override the folder with SYNCAPP_HOME).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine::atomic_write;
use crate::schedule::Schedule;

fn yes() -> bool {
    true
}

fn seven() -> u32 {
    7
}

fn thirty() -> u32 {
    30
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Source {
    /// Folder name used on the drive, e.g. "Documents" -> X:\SyncApp Backup\Documents\...
    pub name: String,
    /// Folder or single file on this PC.
    pub path: PathBuf,
    /// Wildcard patterns (* and ?) matched against file names and relative paths.
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Folder or single file. Recorded when added, so restores still know where
    /// files belong after the source itself has been deleted from the PC.
    #[serde(default)]
    pub kind: Option<SourceKind>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Folder,
    File,
}

impl Source {
    /// Folder or file: the recorded kind, else whatever is on disk now, else None.
    pub fn kind(&self) -> Option<SourceKind> {
        self.kind.or_else(|| {
            if self.path.is_dir() {
                Some(SourceKind::Folder)
            } else if self.path.is_file() {
                Some(SourceKind::File)
            } else {
                None
            }
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DriveReg {
    /// Random 128-bit id, also written to X:\.syncapp\drive.json.
    pub drive_id: String,
    pub name: String,
    /// Volume serial at registration. A drive with the marker file but a
    /// different serial (e.g. a copied marker) is refused.
    pub volume_serial: Option<u32>,
    /// Back up automatically when plugged in.
    #[serde(default = "yes")]
    pub auto_backup: bool,
    /// Which sources go to this drive. None = all enabled sources.
    #[serde(default)]
    pub sources: Option<Vec<String>>,
    /// Extra backups while the drive stays plugged in.
    #[serde(default)]
    pub schedule: Schedule,
    /// Old setting (pre-schedule); migrated into `schedule` on load.
    #[serde(default, skip_serializing)]
    pub repeat_minutes: u32,
    #[serde(default)]
    pub registered_at: u64,
    /// Remind me if this drive hasn't been backed up for this many days (0 = never).
    #[serde(default = "seven")]
    pub remind_after_days: u32,
    /// Verify the drive automatically every this many days, when plugged in (0 = never).
    #[serde(default = "thirty")]
    pub verify_every_days: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub sources: Vec<Source>,
    #[serde(default)]
    pub drives: Vec<DriveReg>,
    #[serde(default = "yes")]
    pub notifications: bool,
    /// Pause automatic backups (manual ones still work).
    #[serde(default)]
    pub paused: bool,
    /// Random ID for this PC, so several PCs can share a backup drive.
    #[serde(default)]
    pub pc_id: String,
    /// Don't run scheduled backups while Windows Battery saver is on
    /// (backups when a drive is plugged in still happen).
    #[serde(default = "yes")]
    pub skip_on_battery_saver: bool,
    /// Check GitHub for a newer version once a day and notify.
    #[serde(default = "yes")]
    pub check_updates: bool,
}

impl Default for Config {
    /// Same defaults as a settings file that doesn't mention an option.
    fn default() -> Self {
        Config {
            sources: Vec::new(),
            drives: Vec::new(),
            notifications: true,
            paused: false,
            pc_id: String::new(),
            skip_on_battery_saver: true,
            check_updates: true,
        }
    }
}

impl Config {
    /// `import_into_standard_folders` for the installer: logs and maps errors to text.
    pub fn import_into_standard_folders_logged() -> Result<usize, String> {
        let n = import_into_standard_folders().map_err(|e| format!("couldn't carry over settings: {e}"))?;
        if n > 0 {
            crate::info!("Carried {n} settings/history file(s) over to the standard folders");
        }
        Ok(n)
    }

    /// This PC's identity for backups (name is the current Windows computer name).
    pub fn pc(&self) -> crate::engine::PcIdentity {
        crate::engine::PcIdentity {
            id: if self.pc_id.is_empty() { "this-pc".into() } else { self.pc_id.clone() },
            name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "This PC".into()),
        }
    }

    /// Give this PC an ID the first time SyncApp runs (saved immediately, under a lock,
    /// so the manager window and background service agree on it).
    pub fn ensure_pc_id() {
        let _lock = crate::win::NamedLock::acquire_wait("Local\\SyncApp-Config", 5000).ok().flatten();
        if let Ok(mut cfg) = Config::load() {
            if cfg.pc_id.is_empty() {
                cfg.pc_id = crate::win::random_id();
                let _ = cfg.save();
            }
        }
    }
}

/// Standard settings folder: %APPDATA%\SyncApp.
pub fn default_config_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into()).join("SyncApp")
}

/// Standard folder for history, log and other data: %LOCALAPPDATA%\SyncApp.
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into()).join("SyncApp")
}

/// Settings folder in use (SYNCAPP_HOME overrides the standard one).
pub fn config_dir() -> PathBuf {
    std::env::var_os("SYNCAPP_HOME").map(PathBuf::from).unwrap_or_else(default_config_dir)
}

/// Data folder in use (SYNCAPP_HOME overrides the standard one).
pub fn data_dir() -> PathBuf {
    std::env::var_os("SYNCAPP_HOME").map(PathBuf::from).unwrap_or_else(default_data_dir)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_lowercase();
    norm(a).trim_end_matches('\\') == norm(b).trim_end_matches('\\')
}

/// Copy one file, keeping a different file already at `dst` as "<name>.before-install".
fn import_file(src: &Path, dst: &Path) -> std::io::Result<bool> {
    if !src.is_file() {
        return Ok(false);
    }
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    if dst.exists() {
        if fs::read(src)? == fs::read(dst)? {
            return Ok(false);
        }
        let mut backup = dst.as_os_str().to_owned();
        backup.push(".before-install");
        let backup = PathBuf::from(backup);
        let _ = fs::remove_file(&backup);
        fs::rename(dst, &backup)?;
    }
    fs::copy(src, dst)?;
    Ok(true)
}

/// Bring this copy's settings, history, verification reports and log into the standard
/// folders, so an installed SyncApp carries on exactly like the copy it was installed from.
/// Does nothing when this copy already uses the standard folders. Returns files copied.
pub fn import_into_standard_folders() -> std::io::Result<usize> {
    let (src_cfg, src_data) = (config_dir(), data_dir());
    let (dst_cfg, dst_data) = (default_config_dir(), default_data_dir());
    let mut copied = 0;
    if !same_dir(&src_cfg, &dst_cfg) {
        copied += import_file(&src_cfg.join("config.json"), &dst_cfg.join("config.json"))? as usize;
    }
    if !same_dir(&src_data, &dst_data) && src_data.is_dir() {
        for entry in fs::read_dir(&src_data)?.flatten() {
            let name = entry.file_name();
            let n = name.to_string_lossy().to_lowercase();
            // Settings were handled above; skip downloads/temp files and backups.
            if n == "config.json" || n.ends_with(".tmp") || n.ends_with(".before-install") || !entry.path().is_file() {
                continue;
            }
            copied += import_file(&entry.path(), &dst_data.join(&name))? as usize;
        }
    }
    Ok(copied)
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

impl Config {
    /// Missing file -> empty config. Corrupt file -> error (we never overwrite
    /// a config we couldn't read, so a typo can't wipe your settings).
    pub fn load() -> Result<Config, String> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Config, String> {
        match fs::read(path) {
            Ok(bytes) => {
                let mut cfg: Config = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("Settings file {} is invalid: {e}", path.display()))?;
                // Older configs didn't record whether a source is a folder or a file.
                for src in cfg.sources.iter_mut() {
                    if src.kind.is_none() {
                        src.kind = src.kind();
                    }
                }
                for d in cfg.drives.iter_mut() {
                    if d.schedule.is_off() && d.repeat_minutes > 0 {
                        d.schedule = Schedule::Every { minutes: d.repeat_minutes };
                    }
                    d.repeat_minutes = 0;
                }
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("Cannot read {}: {e}", path.display())),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        atomic_write(&config_path(), &json).map_err(|e| format!("Cannot save settings: {e}"))
    }

    pub fn drive(&self, id: &str) -> Option<&DriveReg> {
        self.drives.iter().find(|d| d.drive_id == id)
    }

    pub fn drive_mut(&mut self, id: &str) -> Option<&mut DriveReg> {
        self.drives.iter_mut().find(|d| d.drive_id == id)
    }

    /// Enabled sources selected for this drive.
    pub fn sources_for(&self, d: &DriveReg) -> Vec<Source> {
        self.sources
            .iter()
            .filter(|s| s.enabled)
            .filter(|s| d.sources.as_ref().is_none_or(|sel| sel.iter().any(|n| n.eq_ignore_ascii_case(&s.name))))
            .cloned()
            .collect()
    }

    /// Add a source, deriving a unique name from the path when none is given.
    pub fn add_source(&mut self, path: &Path, name: Option<&str>, exclude: Vec<String>) -> Result<Source, String> {
        let path = std::path::absolute(path).map_err(|e| e.to_string())?;
        if !path.exists() {
            return Err(format!("{} does not exist", path.display()));
        }
        if self.sources.iter().any(|s| s.path == path) {
            return Err(format!("{} is already a source", path.display()));
        }
        let name = match name {
            Some(n) => n.trim().to_string(),
            None => {
                let base = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Drive".into());
                let base = sanitize_name(&base);
                let mut n = base.clone();
                let mut i = 2;
                while self.sources.iter().any(|s| s.name.eq_ignore_ascii_case(&n)) {
                    n = format!("{base} ({i})");
                    i += 1;
                }
                n
            }
        };
        validate_name(&name)?;
        if self.sources.iter().any(|s| s.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("A source named '{name}' already exists"));
        }
        let kind = Some(if path.is_dir() { SourceKind::Folder } else { SourceKind::File });
        let src = Source { name, path, exclude, enabled: true, kind };
        self.sources.push(src.clone());
        Ok(src)
    }
}

fn sanitize_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if r#"\/:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').to_string();
    if cleaned.is_empty() { "Source".into() } else { cleaned }
}

/// Source names become folder names on the drive, so they must be safe ones.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 100 {
        return Err("Name must be 1-100 characters".into());
    }
    if name.chars().any(|c| r#"\/:*?"<>|"#.contains(c) || c.is_control()) {
        return Err(r#"Name cannot contain \ / : * ? " < > |"#.into());
    }
    if name == "." || name == ".." || name.ends_with('.') || name.ends_with(' ') || name.starts_with(' ') {
        return Err("Name cannot start/end with a space or end with a dot".into());
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
        "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        return Err(format!("'{name}' is a reserved Windows name"));
    }
    Ok(())
}
