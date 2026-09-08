//! Linux-only integration test: round-trips `Platform::secrets()` against a
//! REAL Secret Service D-Bus daemon (gnome-keyring), not the offline-file
//! fallback `LinuxSecrets` falls back to when no daemon is reachable.
//!
//! Every existing unit test for `LinuxSecrets` (`src/platform/linux.rs`)
//! injects a fake `keyring::Error` to exercise the fallback logic — none of
//! them ever talk to a real daemon over D-Bus, because there usually isn't
//! one running in a plain `cargo test` environment. This test closes that
//! gap: it drives an actual `gnome-keyring-daemon` session end to end.
//!
//! This crate has no `[lib]` target, so an integration test can't reach
//! `crate::platform::linux::LinuxSecrets` directly the way an in-crate unit
//! test can. Instead this drives the compiled binary as a subprocess via the
//! undocumented `usagio __secrets_selftest <service> <account> <secret>`
//! hook (see `cmd_secrets_selftest` in `src/main.rs`) — the same black-box
//! pattern `tests/cli.rs` already uses for every other CLI behavior.
//!
//! `#[ignore]`d by default — requires a real D-Bus session bus with an
//! unlocked `gnome-keyring-daemon --components=secrets` reachable on it,
//! which local `cargo test` doesn't set up. CI launches one explicitly; see
//! the "Linux Secret Service integration test" step in
//! `.github/workflows/ci.yml`.

#![cfg(target_os = "linux")]

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
#[ignore = "requires a real D-Bus session bus + an unlocked gnome-keyring-daemon --components=secrets; see the CI step that launches one before running with --ignored"]
fn linux_secrets_round_trip_against_real_secret_service_daemon() {
    // Test-scoped service name (includes this process's pid) so a run never
    // collides with a leftover entry from a previous run or another usagio
    // install on the same keyring.
    let service = format!("usagio-ci-test-{}", std::process::id());
    let account = "ci-test-account";
    let secret = "ci-test-secret-value";

    let mut cmd = Command::cargo_bin("usagio").expect("binary builds");
    cmd.args(["__secrets_selftest", &service, account, secret]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("OK"));
}
