//! SyncApp: automatic, verified backups to marked external drives.

#[macro_use]
pub mod log;
pub mod net;
pub mod update;
pub mod planner;
pub mod config;
pub mod engine;
pub mod schedule;
pub mod service;
pub mod toast;
pub mod win;
