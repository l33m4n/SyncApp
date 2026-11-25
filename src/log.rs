//! Minimal append-only log at %LOCALAPPDATA%\SyncApp\syncapp.log (rotated at 5 MB).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::data_dir;

static LOCK: Mutex<()> = Mutex::new(());
static ECHO: AtomicBool = AtomicBool::new(false);
const MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Also print log lines to stderr (used by the CLI).
pub fn echo_to_stderr(on: bool) {
    ECHO.store(on, Ordering::Relaxed);
}

pub fn log_path() -> std::path::PathBuf {
    data_dir().join("syncapp.log")
}

pub fn write(level: &str, msg: &str) {
    let line = format!("[{}] {level:5} {msg}\r\n", crate::win::local_now_string());
    if ECHO.load(Ordering::Relaxed) {
        eprint!("{level}: {msg}\n");
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = log_path();
    let _ = fs::create_dir_all(path.parent().unwrap());
    if fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = fs::rename(&path, path.with_extension("log.old"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::log::write("INFO", &format!($($t)*)) } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::log::write("WARN", &format!($($t)*)) } }
#[macro_export]
macro_rules! error { ($($t:tt)*) => { $crate::log::write("ERROR", &format!($($t)*)) } }
