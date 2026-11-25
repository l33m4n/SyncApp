//! SyncApp: automatic, verified backups to marked external drives.

#[macro_use]
pub mod log;
pub mod config;
pub mod engine;
pub mod service;
pub mod win;

/// Paths of the sibling executables (they're installed side by side).
pub fn sibling_exe(name: &str) -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .unwrap_or_else(|| name.into())
}
