//! SyncApp: automatic, verified backups to marked external drives.

#[macro_use]
pub mod log;
pub mod planner;
pub mod config;
pub mod engine;
pub mod schedule;
pub mod service;
pub mod win;
