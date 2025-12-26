//! Core library for mailz - agent coordination via mail-like messaging.
//!
//! This crate provides:
//! - Data models: Agent, Message, FileReservation, Project
//! - SQLite storage with FTS5 search
//! - Configuration and XDG path handling

pub mod config;
pub mod error;
pub mod models;
pub mod paths;
pub mod storage;

pub use config::{AppConfig, LoggingConfig, PathsConfig, RuntimeConfig};
pub use error::{CoreError, Result};
pub use models::*;
pub use paths::{AppPaths, default_cache_dir};
pub use storage::Storage;

/// Application name used for config directories and environment prefix.
pub const APP_NAME: &str = "mailz";

/// Returns the environment variable prefix for this application.
pub fn env_prefix() -> String {
    APP_NAME
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Returns the default parallelism based on available CPU cores.
pub fn default_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
