//! Shared integration-test helpers.
//!
//! `TestLogDir` swaps `$HOME` for a fresh tempdir, so `usage_log` (and any
//! other module that resolves paths under `~/.config/usagio`) reads and
//! writes into an isolated location. On drop the previous `HOME` is restored
//! and the tempdir is deleted, so tests never leak state between runs.
//!
//! # WARNING to future maintainers (H4, round-1 codeaudit)
//!
//! This file is currently DEAD CODE — no integration test in `tests/`
//! includes it via `mod common;`. Wiring it in without also serializing
//! against every other `$HOME` mutator in the same binary is how one prior
//! test wiped a live developer `~/.config/claude-usage/state.json`. The
//! in-crate replacement is `src/env_lock.rs::scoped_env_var` (see the
//! never-re-login postmortem). Integration binaries can't import
//! `env_lock` directly (they compile without `cfg(test)`); this file's
//! `TestLogDir` / `TestConfigDir` therefore take a file-local `Mutex` so
//! two of them constructed on different threads within the SAME
//! integration binary can't race each other's `$HOME` swap. Cross-binary
//! isolation is separately provided by cargo test spawning each
//! integration binary in its own process.

#![allow(dead_code)]
// Integration-test binaries can't reach the binary crate's `env_lock` module
// (they link against a separate compilation without `cfg(test)`), and each
// integration test binary is a separate process so the process-global `$HOME`
// races that motivated `env_lock` don't cross binaries. The `disallowed_methods`
// guard applies to the in-crate build; suppress it here at the module scope
// with an explanation so the fixture stays readable.
#![allow(clippy::disallowed_methods)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use chrono::{DateTime, Datelike, Utc};
use serde::Serialize;
use tempfile::TempDir;

/// File-local serialization for `$HOME` mutations across `TestLogDir` /
/// `TestConfigDir` instances constructed on separate threads within the SAME
/// integration binary. See the H4 warning at the top of the module. Held as
/// a `MutexGuard<'static, ()>` for the lifetime of each fixture.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// A scoped `$HOME` override. Constructing one points every module that
/// reads `$HOME` (notably `store::config_dir` and, via it, `usage_log`) at
/// a fresh empty tempdir. Dropping restores the previous `HOME`.
pub struct TestLogDir {
    _home: TempDir,
    // Kept so callers can inspect / write extra fixtures without recomputing.
    home_path: PathBuf,
    prev_home: Option<OsString>,
    // Held for the lifetime of the fixture; released on Drop. Serialises
    // `$HOME` mutation across sibling `TestLogDir` / `TestConfigDir` in the
    // same binary. See the H4 warning at the top of the module.
    _env_guard: MutexGuard<'static, ()>,
}

impl TestLogDir {
    /// Allocate a fresh tempdir and repoint `$HOME` at it.
    pub fn new() -> Self {
        // Acquire the file-local env lock BEFORE reading prev_home / mutating,
        // so no sibling fixture in the same integration binary races with us.
        let _env_guard = env_lock();
        let home = tempfile::tempdir().expect("tempdir for TestLogDir");
        let home_path = home.path().to_path_buf();
        let prev_home = std::env::var_os("HOME");
        // SAFETY: process-wide env mutation, serialised on `env_lock()`.
        std::env::set_var("HOME", &home_path);
        // Pre-create the config dir so any writer that assumes existence
        // finds it without extra ceremony.
        let cfg = home_path.join(".config").join("usagio");
        std::fs::create_dir_all(&cfg).expect("create config dir");
        Self {
            _home: home,
            home_path,
            prev_home,
            _env_guard,
        }
    }

    /// Path to the isolated home root (equal to the value of `$HOME` while
    /// this fixture is alive).
    pub fn home(&self) -> &Path {
        &self.home_path
    }

    /// Path to the isolated `~/.config/usagio` directory.
    pub fn config_dir(&self) -> PathBuf {
        self.home_path.join(".config").join("usagio")
    }

    /// Append one NDJSON row into the `history.YYYY-MM.ndjson` file whose
    /// month matches `snap.ts`. The `snap` argument is any `Serialize` value
    /// with the same shape as `usage_log::Snapshot` (kept generic so this
    /// helper doesn't have to depend on the crate's internal type).
    pub fn append_snapshot<S: Serialize>(&self, ts: DateTime<Utc>, snap: &S) {
        let path =
            self.config_dir()
                .join(format!("history.{:04}-{:02}.ndjson", ts.year(), ts.month()));
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

// ---------------------------------------------------------------------------
// TestConfigDir — the general-purpose RAII fixture integration tests should
// use when they touch state.json (directly or via the CLI subprocess). It's a
// thinner cousin of `TestLogDir`: no history-file convenience methods, just an
// isolated `$HOME` and (optionally) a pre-seeded `state.json` inside it.
//
// Both this and the in-crate `store::ScopedConfigDir` funnel through the same
// contract: no test process (parent OR spawned CLI) ever touches the real
// `~/.config/usagio`. The parent-side `$HOME` swap here isolates any
// spawned subprocess (which inherits `HOME` from us); the in-crate
// `ScopedConfigDir` layers on top of the same thread-local `HOME_OVERRIDE`
// that the crate's `cfg(test)` tripwire enforces.
// ---------------------------------------------------------------------------

/// A scoped `$HOME` guard for integration tests that spawn `usagio` as a
/// subprocess. The child inherits `$HOME` from the current process, so pinning
/// `HOME` here plus asserting inside the subprocess-safe `cfg(not(test))` path
/// (the built binary is compiled without `cfg(test)`) keeps every state.json
/// read/write inside the tempdir.
pub struct TestConfigDir {
    _home: TempDir,
    home_path: PathBuf,
    prev_home: Option<OsString>,
    // See H4 warning at the top of the module.
    _env_guard: MutexGuard<'static, ()>,
}

impl TestConfigDir {
    /// Fresh tempdir, `$HOME` repointed at it, `~/.config/usagio`
    /// pre-created so callers can seed a state.json without extra ceremony.
    pub fn new() -> Self {
        let _env_guard = env_lock();
        let home = tempfile::tempdir().expect("tempdir for TestConfigDir");
        let home_path = home.path().to_path_buf();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home_path);
        let cfg = home_path.join(".config").join("usagio");
        std::fs::create_dir_all(&cfg).expect("create config dir");
        Self {
            _home: home,
            home_path,
            prev_home,
            _env_guard,
        }
    }

    pub fn home(&self) -> &Path {
        &self.home_path
    }

    pub fn config_dir(&self) -> PathBuf {
        self.home_path.join(".config").join("usagio")
    }

    pub fn state_path(&self) -> PathBuf {
        self.config_dir().join("state.json")
    }

    /// Write `contents` to the fixture's state.json (creating the parent dir
    /// if it isn't there yet). Convenience for seeding a starting state.
    pub fn seed_state(&self, contents: &str) {
        let p = self.state_path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).expect("create config dir");
        }
        std::fs::write(&p, contents).expect("seed state.json");
    }
}

impl Drop for TestConfigDir {
    fn drop(&mut self) {
        match self.prev_home.take() {
            Some(p) => std::env::set_var("HOME", p),
            None => std::env::remove_var("HOME"),
        }
    }
}
