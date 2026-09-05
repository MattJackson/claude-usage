//! Lightweight append-only debug log at `~/.config/claude-usage/claude-usage.log`.
//!
//! Records poll ticks, fetch outcomes (including 429s and backoff), swaps, and
//! switches so behaviour can be diagnosed after the fact. Never logs tokens or
//! other secrets — only account names, percentages, and error descriptions.

use std::io::Write;

use crate::store;

/// Rotate the log once it grows past this size (~1 MB).
const MAX_BYTES: u64 = 1_000_000;

/// Append a timestamped line to the debug log. Best-effort: any failure is
/// silently ignored so logging never disrupts the daemon.
pub fn log(msg: &str) {
    let Ok(dir) = store::config_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("claude-usage.log");

    // Rotate if the file has grown too large (keep one previous generation).
    if let Ok(m) = std::fs::metadata(&path) {
        if m.len() > MAX_BYTES {
            let _ = std::fs::rename(&path, dir.join("claude-usage.log.1"));
        }
    }

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = chrono::Utc::now().to_rfc3339();
        let _ = writeln!(f, "{ts} {msg}");
    }
}
