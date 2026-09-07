//! Shared integration-test helpers.
//!
//! `TestLogDir` swaps `$HOME` for a fresh tempdir, so `usage_log` (and any
//! other module that resolves paths under `~/.config/claude-usage`) reads and
//! writes into an isolated location. On drop the previous `HOME` is restored
//! and the tempdir is deleted, so tests never leak state between runs.
//!
//! Because `HOME` is process-global, tests using `TestLogDir` must not run in
//! parallel with anything else that touches the config dir. Wrap those tests
//! in a `serial_test`-style mutex (or run them with `--test-threads=1`) if you
//! add more than one.

#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};
use serde::Serialize;
use tempfile::TempDir;

/// A scoped `$HOME` override. Constructing one points every module that
/// reads `$HOME` (notably `store::config_dir` and, via it, `usage_log`) at
/// a fresh empty tempdir. Dropping restores the previous `HOME`.
pub struct TestLogDir {
    _home: TempDir,
    // Kept so callers can inspect / write extra fixtures without recomputing.
    home_path: PathBuf,
    prev_home: Option<OsString>,
}

impl TestLogDir {
    /// Allocate a fresh tempdir and repoint `$HOME` at it.
    pub fn new() -> Self {
        let home = tempfile::tempdir().expect("tempdir for TestLogDir");
        let home_path = home.path().to_path_buf();
        let prev_home = std::env::var_os("HOME");
        // SAFETY: process-wide env mutation. Callers coordinate access.
        std::env::set_var("HOME", &home_path);
        // Pre-create the config dir so any writer that assumes existence
        // finds it without extra ceremony.
        let cfg = home_path.join(".config").join("claude-usage");
        std::fs::create_dir_all(&cfg).expect("create config dir");
        Self {
            _home: home,
            home_path,
            prev_home,
        }
    }

    /// Path to the isolated home root (equal to the value of `$HOME` while
    /// this fixture is alive).
    pub fn home(&self) -> &Path {
        &self.home_path
    }

    /// Path to the isolated `~/.config/claude-usage` directory.
    pub fn config_dir(&self) -> PathBuf {
        self.home_path.join(".config").join("claude-usage")
    }

    /// Append one NDJSON row into the `history.YYYY-MM.ndjson` file whose
    /// month matches `snap.ts`. The `snap` argument is any `Serialize` value
    /// with the same shape as `usage_log::Snapshot` (kept generic so this
    /// helper doesn't have to depend on the crate's internal type).
    pub fn append_snapshot<S: Serialize>(&self, ts: DateTime<Utc>, snap: &S) {
        let path = self.config_dir().join(format!(
            "history.{:04}-{:02}.ndjson",
            ts.year(),
            ts.month()
        ));
        let line = serde_json::to_string(snap).expect("serialize snap");
        let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
        existing.push_str(&line);
        existing.push('\n');
        std::fs::write(&path, existing).expect("write history line");
    }
}

impl Drop for TestLogDir {
    fn drop(&mut self) {
        match self.prev_home.take() {
            Some(p) => std::env::set_var("HOME", p),
            None => std::env::remove_var("HOME"),
        }
    }
}
