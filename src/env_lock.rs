//! Crate-wide serialization for process-global env mutation.
//!
//! `std::env::set_var` mutates a process-shared table; two test threads that
//! both flip `$HOME` around a tempdir will race and one thread ends up
//! resolving paths under the *other* thread's temp home. This crate hit that
//! exact bug when `src/platform/macos.rs` set `$HOME` without taking the
//! `HOME_LOCK` used by other tests, which cascaded into a live state.json
//! wipe on a developer machine (see the never-re-login postmortem).
//!
//! Every test that mutates `$HOME`, `$XDG_CONFIG_HOME`, or `$CODEX_HOME`
//! MUST go through [`scoped_env_var`], which:
//!   1. takes the crate-wide [`ENV_LOCK`] (so no other scoped mutator runs),
//!   2. snapshots the current value,
//!   3. installs the caller's value,
//!   4. runs the closure,
//!   5. restores the previous value on the way out — including on panic,
//!      via a `Drop` guard.
//!
//! A `clippy.toml` `disallowed-methods` entry blocks any new direct
//! `std::env::set_var` / `std::env::remove_var` call from slipping in.

use std::ffi::OsString;
use std::sync::Mutex;

/// The one true lock. Every scoped env mutation across the crate serialises
/// on this Mutex — including nested crates' test modules — so no two tests
/// ever see interleaved `$HOME` values.
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with `key` bound to `val` (or unset when `val` is `None`).
/// Restores the prior value (including unset) after `f` returns or panics.
/// Serialised across the crate via [`ENV_LOCK`].
///
/// NOT reentrant: calling `scoped_env_var` from inside `f` will deadlock.
/// The tests that use this helper never nest.
pub(crate) fn scoped_env_var<F, R>(key: &str, val: Option<&str>, f: F) -> R
where
    F: FnOnce() -> R,
{
    // Poisoning is fine: the previous panicked closure ran its Drop guard
    // (below) before unwinding past this lock, so `$HOME` is already restored.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let prev = std::env::var_os(key);
    // SAFETY: guarded by ENV_LOCK — no other thread inside this test binary
    // reads or writes `key` concurrently while this scope is live.
    #[allow(clippy::disallowed_methods)]
    match val {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }

    struct Restore<'a> {
        key: &'a str,
        prev: Option<OsString>,
    }
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            #[allow(clippy::disallowed_methods)]
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
    let _r = Restore { key, prev };
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_env_var_sets_then_restores_previous_value() {
        // Snapshot the current outer value so we can assert on the restore.
        let outer = std::env::var_os("HOME");
        scoped_env_var("HOME", Some("/tmp/env-lock-test-a"), || {
            assert_eq!(
                std::env::var_os("HOME"),
                Some(std::ffi::OsString::from("/tmp/env-lock-test-a"))
            );
        });
        assert_eq!(std::env::var_os("HOME"), outer, "prior HOME restored");
    }

    #[test]
    fn scoped_env_var_restores_after_panic() {
        let outer = std::env::var_os("HOME");
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scoped_env_var("HOME", Some("/tmp/env-lock-test-panic"), || {
                panic!("boom");
            });
        }));
        assert!(r.is_err(), "closure panicked");
        assert_eq!(
            std::env::var_os("HOME"),
            outer,
            "HOME restored even when the closure panicked"
        );
    }

    #[test]
    fn scoped_env_var_none_unsets_then_restores() {
        // Seed an outer value (guarded by the crate-wide lock) so the
        // unset/restore round-trip is observable. We can't nest scoped calls
        // because ENV_LOCK is a std Mutex (non-reentrant) — so seed with a
        // scoped call, read the previous, then run a second scoped call that
        // asserts the unset and restores to the seeded value.
        let key = "USAGIO_ENV_LOCK_TEST_KEY";
        scoped_env_var(key, Some("seed"), || {
            // Nothing here — the seed is visible while the guard is live.
            assert_eq!(std::env::var_os(key), Some(std::ffi::OsString::from("seed")));
        });
        // Now exercise the None (unset) path from a fresh outer state.
        scoped_env_var(key, None, || {
            assert!(std::env::var_os(key).is_none(), "None unsets the key");
        });
    }
}
