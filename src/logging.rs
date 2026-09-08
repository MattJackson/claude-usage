//! Lightweight append-only debug log at `~/.config/usagio/usagio.log`.
//!
//! Records poll ticks, fetch outcomes (including 429s and backoff), swaps, and
//! switches so behaviour can be diagnosed after the fact. Never logs tokens or
//! other secrets — only account names, percentages, and error descriptions.

use std::io::Write;
use std::sync::Once;

use crate::store;

/// Rotate a rotating file once it grows past this size (~1 MB). Shared by the
/// debug log and history.jsonl so their threshold genuinely can't drift.
pub const MAX_BYTES: u64 = 1_000_000;

/// Cap on `.1`, `.2`, ... uniquification attempts when the primary rotated
/// name is already taken (e.g. a previous rotation's target is still locked
/// by another process). Mirrors the small bounded-retry uniquification
/// `store.rs`'s backup writer uses for `state-rejected-*`/`pre-restore-*`.
const ROTATE_UNIQUIFY_ATTEMPTS: u32 = 20;

/// robustness-04 (v0.5.1 audit): surface a rotation failure at least once per
/// process instead of swallowing it forever. Without this, a single rename
/// failure (Windows sharing violation from another `usagio` process/CLI
/// invocation holding the log file open, AV/EDR scan handle, etc.) silently
/// and PERMANENTLY disabled rotation — `rotate_if_large` kept hitting the
/// same failing `rename` on every call thereafter with no diagnostic, and
/// `usagio.log` grew unbounded past its documented 1 MB cap for the rest of
/// the process's life.
static ROTATE_FAILURE_LOGGED: Once = Once::new();

/// If `path` has grown past `max_bytes`, rename it to `<path>.1` (keeping one
/// previous generation). Best-effort. Shared by the debug log and history.jsonl
/// so their rotation policy can't drift apart.
///
/// If the primary rotated name can't be produced (rename fails, e.g. it's
/// still held open by another process), fall back to a uniquified name
/// (`.2`, `.3`, ...) so a single lock-contention event doesn't permanently
/// disable rotation for the rest of the process's life. The very first
/// failure across ALL attempts is logged once per process (`Once`-guarded,
/// so a persistently-locked file doesn't spam the log it's trying to
/// rotate).
pub fn rotate_if_large(path: &std::path::Path, max_bytes: u64) {
    let Ok(m) = std::fs::metadata(path) else {
        return;
    };
    if m.len() <= max_bytes {
        return;
    }
    let rotated_name = |gen: u32| -> std::path::PathBuf {
        let mut ext = path.extension().unwrap_or_default().to_os_string();
        if gen <= 1 {
            ext.push(".1");
        } else {
            ext.push(format!(".{gen}"));
        }
        path.with_extension(ext)
    };
    let mut last_err = None;
    for gen in 1..=ROTATE_UNIQUIFY_ATTEMPTS {
        let rotated = rotated_name(gen);
        match std::fs::rename(path, &rotated) {
            Ok(()) => return,
            Err(e) => last_err = Some(e),
        }
    }
    if let Some(e) = last_err {
        ROTATE_FAILURE_LOGGED.call_once(|| {
            // Best-effort direct write (bypass `log()` — it calls this
            // function, which would recurse into the same failing rotation
            // we're trying to report). Falls back to stderr if even that
            // fails, so the failure is never fully silent.
            let msg = format!(
                "usagio.log rotation failed after {ROTATE_UNIQUIFY_ATTEMPTS} attempts \
                 for {}: {e}",
                path.display()
            );
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let ts = chrono::Utc::now().to_rfc3339();
                let _ = writeln!(f, "{ts} event=log_rotation_failed reason={msg}");
            } else {
                eprintln!("usagio: {msg}");
            }
        });
    }
}

/// Redact a token to its first 20 chars + `..` so structured token-lifecycle
/// log lines (`event=...`) can show enough of a token to correlate across
/// lines (e.g. "did the CAS-lost adopted prefix match the next cycle's
/// before-blob prefix?") without ever writing a usable secret to disk.
/// Tokens shorter than 20 chars (should never happen for a real OAuth access
/// token, but keeps this total for stray test/fixture strings) are redacted
/// in full rather than panicking on the slice.
pub fn tok_prefix(t: &str) -> String {
    if t.len() >= 20 {
        format!("{}..", &t[..20])
    } else {
        format!("{t}..")
    }
}

/// Append a timestamped line to the debug log. Best-effort: any failure is
/// silently ignored so logging never disrupts the daemon.
pub fn log(msg: &str) {
    let Ok(dir) = store::config_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("usagio.log");

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

#[cfg(test)]
mod tests {
    use super::*;

    /// robustness-04: when the primary rotated name (`.1`) is unavailable,
    /// rotation should fall through to the next uniquified name rather than
    /// silently leaving the oversized file in place.
    #[test]
    fn rotate_if_large_falls_back_to_uniquified_name_when_primary_target_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usagio.log");
        std::fs::write(&path, b"0123456789").unwrap();
        // Occupy the primary rotated name with a directory so `rename(file,
        // existing_dir)` fails there (ENOTDIR/EISDIR on unix).
        std::fs::create_dir(dir.path().join("usagio.log.1")).unwrap();
        rotate_if_large(&path, 5);
        assert!(!path.exists(), "original should have been moved aside");
        assert!(
            dir.path().join("usagio.log.2").exists(),
            "expected fallback to the next uniquified rotated name"
        );
    }

    /// robustness-04: when every uniquified rotated name is unavailable, the
    /// failure must be surfaced (not silently swallowed forever) — appended
    /// directly to the still-in-place original file since `log()` itself
    /// can't be used without recursing back into this same rotation path.
    #[test]
    fn rotate_if_large_surfaces_failure_when_every_attempt_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usagio.log");
        std::fs::write(&path, b"0123456789").unwrap();
        for gen in 1..=ROTATE_UNIQUIFY_ATTEMPTS {
            let name = if gen == 1 {
                "usagio.log.1".to_string()
            } else {
                format!("usagio.log.{gen}")
            };
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        rotate_if_large(&path, 5);
        // Every rename attempt failed, so the original file is still here —
        // and should now carry the failure line the `Once` guard appended.
        assert!(path.exists());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("event=log_rotation_failed"),
            "expected the rotation-failed event to be appended, got: {contents}"
        );
    }
}
