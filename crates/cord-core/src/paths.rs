//! Where rustcord keeps things on disk. Tokens are not here: they live in
//! the OS keyring.

use std::path::PathBuf;

/// `~/.cache/rustcord` on Linux, `%LOCALAPPDATA%\rustcord` on Windows.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".")).join("rustcord")
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("rustcord")
}

pub fn logs_dir() -> PathBuf {
    cache_dir().join("logs")
}
