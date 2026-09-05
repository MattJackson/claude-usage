//! Lightweight append-only debug log at `~/.config/claude-usage/claude-usage.log`.
//!
//! Records poll ticks, fetch outcomes (including 429s and backoff), swaps, and
//! switches so behaviour can be diagnosed after the fact. Never logs tokens or
//! other secrets — only account names, percentages, and error descriptions.

use std::io::Write;

use crate::store;

/// Rotate the log once it grows past this size (~1 MB).
const MAX_BYTES: u64 = 1_000_000;

/// If `path` has grown past `max_bytes`, rename it to `<path>.1` (keeping one
/// previous generation). Best-effort. Shared by the debug log and history.jsonl
/// so their rotation policy can't drift apart.
pub fn rotate_if_large(path: &std::path::Path, max_bytes: u64) {
    if let Ok(m) = std::fs::metadata(path) {
        if m.len() > max_bytes {
            let rotated = {
                let mut ext = path.extension().unwrap_or_default().to_os_string();
                ext.push(".1");
                path.with_extension(ext)
            };
            let _ = std::fs::rename(path, rotated);
        }
    }
}

/// Append a timestamped line to the debug log. Best-effort: any failure is
/// silently ignored so logging never disrupts the daemon.
pub fn log(msg: &str) {
    let Ok(dir) = store::config_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("claude-usage.log");

    // Rotate if the file has grown too large (keep one previous generation).
    rotate_if_large(&path, MAX_BYTES);

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = chrono::Utc::now().to_rfc3339();
        let _ = writeln!(f, "{ts} {msg}");
    }
}
