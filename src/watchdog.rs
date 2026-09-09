//! Self-healing runtime watchdog for the auto-swap daemon.
//!
//! # The outage this prevents
//!
//! The daemon once leaked file descriptors (a since-fixed `notify` kqueue bug —
//! see the Cargo.toml comment on the `notify` dep): the open-fd count climbed
//! to 4000+ until every `security(1)` keychain call failed with EMFILE ("Too
//! many open files"). Account switches then wrote the `~/.claude.json` identity
//! but FAILED the keychain write (`keychain_written=err: ... Too many open
//! files`), so Claude Code never saw the new account — auto-swap and refresh
//! silently died with no user-visible signal.
//!
//! The root cause is fixed (FSEvents backend). This module is the runtime
//! safety net: a periodic health check in the poll loop that detects this class
//! of failure and SELF-HEALS instead of silently breaking switching, even if
//! some future regression re-introduces an fd leak or the keychain starts
//! failing for another reason.
//!
//! # What it watches
//!
//! 1. **fd-count self-monitor.** Counts this process's open descriptors
//!    (`/proc/self/fd` on Linux, `/dev/fd` on macOS) once a minute. A tray app
//!    normally sits around ~40. Past [`FD_WARN_THRESHOLD`] it logs a warning;
//!    past [`FD_CRITICAL_THRESHOLD`] (still well below the 65536 rlimit but
//!    clearly abnormal) it takes corrective action.
//!
//! 2. **keychain-write health.** The switch path records every keychain/secret
//!    write result via [`record_keychain_result`]. After
//!    [`KEYCHAIN_FAILURE_THRESHOLD`] consecutive failures it takes the same
//!    corrective action, since EMFILE-from-fd-exhaustion is the known cause.
//!
//! # Remediation ladder
//!
//! Corrective action escalates: first drop+recreate the fsnotify watcher (the
//! historical fd sink) to release descriptors; if the problem persists on the
//! next check, self-restart the daemon so it comes back with a clean fd table.
//!
//! The **decision** ([`decide`]) is a pure function of (fd_count,
//! consecutive_keychain_failures, already_respawned) and is unit-tested at every
//! threshold boundary. The **side effects** (reading the fd count, respawning
//! the watcher, kickstarting launchd / exiting) are isolated behind the
//! [`WatchdogEffects`] trait so they can be mocked in tests — nothing here ever
//! spawns `launchctl` or calls `exit()` under `cargo test`.

use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Thresholds — named constants with rationale
// ---------------------------------------------------------------------------

/// Open-fd count that warrants a warning in the log. A healthy tray app sits
/// around ~40 open descriptors; 1500 is far past anything normal operation
/// produces, so crossing it is an early signal that something is leaking —
/// logged (not yet remediated) so the trend is visible before it turns critical.
pub const FD_WARN_THRESHOLD: usize = 1500;

/// Open-fd count that triggers active remediation. Still comfortably below the
/// 65536 `RLIMIT_NOFILE` the daemon raises itself to (see
/// `main::raise_nofile_limit`), but so far above a tray app's ~40-fd baseline
/// that reaching it means a genuine leak is underway and keychain calls are
/// about to start failing with EMFILE — the exact outage this module prevents.
pub const FD_CRITICAL_THRESHOLD: usize = 3000;

/// Consecutive keychain/secret write failures that trigger remediation. A
/// single failure can be transient (keychain locked for a moment); three in a
/// row is the fingerprint of fd-exhaustion EMFILE, which a watcher respawn or
/// restart clears.
pub const KEYCHAIN_FAILURE_THRESHOLD: u32 = 3;

/// Minimum wall-clock gap between health checks. The poll cadence itself varies
/// from 10s (backstop) to 1200s (rate-limit backoff); throttling on wall-clock
/// time keeps the check to roughly once a minute regardless, so a tight
/// backstop cadence doesn't run the fd scan six times a minute. The scan is a
/// single directory read, so this is about tidiness, not cost.
pub const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Keychain-failure counter (process-global, updated at every write site)
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU32, Ordering};

static KEYCHAIN_CONSEC_FAILURES: AtomicU32 = AtomicU32::new(0);

/// Record the outcome of a keychain/secret write. A success resets the
/// consecutive-failure counter to zero; a failure increments it. Called from
/// the keychain write path (`main::keychain_write`) so the watchdog can detect
/// the EMFILE-from-fd-exhaustion fingerprint without threading a counter
/// through every call site.
pub fn record_keychain_result(ok: bool) {
    if ok {
        KEYCHAIN_CONSEC_FAILURES.store(0, Ordering::Relaxed);
    } else {
        KEYCHAIN_CONSEC_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Current count of consecutive keychain/secret write failures.
pub fn keychain_consecutive_failures() -> u32 {
    KEYCHAIN_CONSEC_FAILURES.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// fd-count reader (the one OS-touching, non-pure input)
// ---------------------------------------------------------------------------

/// Count this process's open file descriptors, or `None` if the count can't be
/// read on this platform. Linux exposes them at `/proc/self/fd`; macOS at
/// `/dev/fd` (on Linux `/dev/fd` is itself a symlink to `/proc/self/fd`, so
/// trying the canonical Linux path first and falling back covers both without
/// any `cfg(target_os)` branch). The returned count includes the transient
/// descriptor `read_dir` itself holds, so it can be off by one — immaterial
/// against thresholds in the thousands.
#[cfg(unix)]
pub fn count_open_fds() -> Option<usize> {
    for dir in ["/proc/self/fd", "/dev/fd"] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            return Some(rd.count());
        }
    }
    None
}

/// Non-Unix fallback: no `/proc` or `/dev/fd`, so the fd self-monitor is inert
/// (Windows was never affected by the kqueue leak this guards against).
#[cfg(not(unix))]
pub fn count_open_fds() -> Option<usize> {
    None
}

// ---------------------------------------------------------------------------
// Pure decision core
// ---------------------------------------------------------------------------

/// What the watchdog should do this cycle. Ordered by severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogAction {
    /// Everything healthy — do nothing.
    None,
    /// fd count is elevated but not yet critical — log a warning only.
    Warn,
    /// fd count critical or keychain failing, and we haven't tried the first
    /// remediation yet: drop + recreate the fsnotify watcher to release fds.
    RespawnWatcher,
    /// The problem persists after a watcher respawn — restart the daemon so it
    /// comes back with a clean fd table.
    SelfRestart,
}

/// Inputs to the pure [`decide`] function. Kept as a struct so the boundary
/// tests read clearly and new signals can be added without churning callers.
#[derive(Debug, Clone, Copy)]
pub struct WatchdogInput {
    /// Open-fd count, or `None` if unreadable on this platform (then the fd
    /// signal is ignored and only the keychain signal drives the decision).
    pub fd_count: Option<usize>,
    /// Consecutive keychain/secret write failures.
    pub keychain_failures: u32,
    /// Whether the watcher-respawn remediation has already been tried during
    /// the current unhealthy episode (reset once a check comes back healthy).
    /// Drives the escalation from `RespawnWatcher` to `SelfRestart`.
    pub already_respawned: bool,
}

/// The pure remediation decision. No I/O, no side effects — given the measured
/// inputs it returns the action to take, which the caller executes (or a test
/// asserts on). This is the heart of the watchdog and is exhaustively
/// boundary-tested.
///
/// Ladder:
/// - fd critical OR keychain past threshold → first `RespawnWatcher`, then
///   `SelfRestart` once a respawn has already been tried and the problem remains;
/// - else fd merely past the warn threshold → `Warn`;
/// - else `None`.
pub fn decide(input: WatchdogInput) -> WatchdogAction {
    let fd_critical = input.fd_count.is_some_and(|n| n >= FD_CRITICAL_THRESHOLD);
    let fd_warn = input.fd_count.is_some_and(|n| n >= FD_WARN_THRESHOLD);
    let keychain_bad = input.keychain_failures >= KEYCHAIN_FAILURE_THRESHOLD;

    if fd_critical || keychain_bad {
        if input.already_respawned {
            WatchdogAction::SelfRestart
        } else {
            WatchdogAction::RespawnWatcher
        }
    } else if fd_warn {
        WatchdogAction::Warn
    } else {
        WatchdogAction::None
    }
}

// ---------------------------------------------------------------------------
// Side-effect seam (mockable in tests)
// ---------------------------------------------------------------------------

/// The side effects the watchdog performs, isolated behind a trait so tests can
/// assert on the decision+wiring without ever respawning a real watcher,
/// spawning `launchctl`, or calling `exit()`.
pub trait WatchdogEffects {
    /// Append a line to the debug log.
    fn log(&mut self, msg: &str);
    /// Drop + recreate the fsnotify watcher. Returns `(before, after)` open-fd
    /// counts for logging (either may be `None` if unreadable).
    fn respawn_watcher(&mut self) -> (Option<usize>, Option<usize>);
    /// Restart the daemon into a clean process (launchd kickstart, or exit under
    /// a KeepAlive policy). Returns `true` if a restart was actually issued —
    /// in production the issuing path does not return (the process is replaced),
    /// so `false` means "couldn't restart, carry on".
    fn self_restart(&mut self) -> bool;
}

/// Stateful watchdog driven once per poll-loop iteration. Owns the throttle
/// clock and the `already_respawned` escalation flag; delegates the actual
/// decision to the pure [`decide`] and the actual effects to a
/// [`WatchdogEffects`].
#[derive(Default)]
pub struct Watchdog {
    last_check: Option<Instant>,
    already_respawned: bool,
}

impl Watchdog {
    /// Call once per poll-loop iteration. Throttles to at most one real check
    /// per [`HEALTH_CHECK_INTERVAL`]; when a check is due it reads the live fd
    /// count + keychain-failure counter and runs [`run_check`](Self::run_check).
    pub fn maybe_run(&mut self, effects: &mut dyn WatchdogEffects) {
        let now = Instant::now();
        if let Some(last) = self.last_check {
            if now.duration_since(last) < HEALTH_CHECK_INTERVAL {
                return;
            }
        }
        self.last_check = Some(now);
        let fd_count = count_open_fds();
        let keychain_failures = keychain_consecutive_failures();
        self.run_check(fd_count, keychain_failures, effects);
    }

    /// The testable core: given the measured inputs, decide and execute. Pure
    /// [`decide`] chooses the action; this method applies it through `effects`
    /// and maintains the `already_respawned` escalation flag. Split from
    /// `maybe_run` so tests can drive it directly with injected inputs and a
    /// mock `effects`, with no throttle clock or real fd read in the way.
    pub fn run_check(
        &mut self,
        fd_count: Option<usize>,
        keychain_failures: u32,
        effects: &mut dyn WatchdogEffects,
    ) {
        let action = decide(WatchdogInput {
            fd_count,
            keychain_failures,
            already_respawned: self.already_respawned,
        });
        match action {
            WatchdogAction::None => {
                // Back to healthy — clear the escalation flag so the next
                // unhealthy episode starts fresh at the watcher-respawn rung.
                self.already_respawned = false;
            }
            WatchdogAction::Warn => {
                self.already_respawned = false;
                let n = fd_count.unwrap_or(0);
                effects.log(&format!("event=fd_watchdog level=warn count={n}"));
            }
            WatchdogAction::RespawnWatcher => {
                if let Some(n) = fd_count {
                    effects.log(&format!(
                        "event=fd_watchdog level=critical count={n} action=watcher_respawn"
                    ));
                }
                if keychain_failures >= KEYCHAIN_FAILURE_THRESHOLD {
                    effects.log(&format!(
                        "event=keychain_watchdog consecutive_failures={keychain_failures} \
                         action=watcher_respawn"
                    ));
                }
                let (before, after) = effects.respawn_watcher();
                effects.log(&format!(
                    "event=fd_watchdog action=watcher_respawn before={} after={}",
                    opt(before),
                    opt(after),
                ));
                // Remember we tried the cheap fix; if the next check is still
                // unhealthy, `decide` escalates to SelfRestart.
                self.already_respawned = true;
            }
            WatchdogAction::SelfRestart => {
                let n = fd_count.unwrap_or(0);
                effects.log(&format!("event=fd_watchdog action=self_restart count={n}"));
                if keychain_failures >= KEYCHAIN_FAILURE_THRESHOLD {
                    effects.log(&format!(
                        "event=keychain_watchdog consecutive_failures={keychain_failures} \
                         action=self_restart"
                    ));
                }
                // In production this replaces the process and does not return.
                // If it returns `false` (not launchd-managed, or kickstart
                // failed) we leave `already_respawned` set so we don't churn
                // the watcher respawn again every cycle — the warning/critical
                // log lines keep surfacing the unresolved condition.
                let _issued = effects.self_restart();
            }
        }
    }
}

/// Render an `Option<usize>` fd count for a log line.
fn opt(n: Option<usize>) -> String {
    match n {
        Some(v) => v.to_string(),
        None => "?".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Production effects
// ---------------------------------------------------------------------------

/// The real side effects used by the running daemon. Logging goes to the debug
/// log; watcher respawn drops+rebuilds the global fsnotify watcher; self-restart
/// reuses the battle-tested launchd kickstart path (`menubar::watchdog_self_restart`).
pub struct RealEffects;

impl WatchdogEffects for RealEffects {
    fn log(&mut self, msg: &str) {
        crate::logging::log(msg);
    }

    fn respawn_watcher(&mut self) -> (Option<usize>, Option<usize>) {
        let before = count_open_fds();
        let respawned = crate::credentials::respawn_watchers();
        if !respawned {
            crate::logging::log(
                "event=fd_watchdog action=watcher_respawn result=no_watcher_installed",
            );
        }
        // A respawn also resets the keychain-failure counter: if the respawn
        // released the descriptors, the next keychain write should succeed, and
        // we don't want a stale pre-respawn failure count to escalate us to a
        // restart before that next write gets a chance.
        KEYCHAIN_CONSEC_FAILURES.store(0, Ordering::Relaxed);
        let after = count_open_fds();
        (before, after)
    }

    fn self_restart(&mut self) -> bool {
        // Only restarts when launchd-managed (a clean kickstart into a fresh fd
        // table). A bare foreground `usagio watch` is NOT restarted — exiting it
        // would leave nothing running, and the plist ships KeepAlive=false so a
        // plain exit() would NOT be revived. Returning `false` there is correct:
        // the watcher-respawn remediation + the surfaced log lines are the
        // safety net for the non-launchd case, with zero restart-loop risk.
        crate::menubar::watchdog_self_restart()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fd reader must return a plausible value (>0) for the current test
    /// process on Unix — at minimum stdin/stdout/stderr plus whatever the test
    /// harness holds open.
    #[cfg(unix)]
    #[test]
    fn count_open_fds_returns_plausible_value() {
        let n = count_open_fds().expect("fd count should be readable on unix");
        assert!(n > 0, "expected a positive open-fd count, got {n}");
        // Sanity upper bound: a test process isn't leaking thousands of fds.
        assert!(n < FD_CRITICAL_THRESHOLD, "unexpectedly high fd count: {n}");
    }

    fn input(fd: Option<usize>, kc: u32, respawned: bool) -> WatchdogInput {
        WatchdogInput {
            fd_count: fd,
            keychain_failures: kc,
            already_respawned: respawned,
        }
    }

    #[test]
    fn decide_healthy_is_none() {
        assert_eq!(decide(input(Some(40), 0, false)), WatchdogAction::None);
        assert_eq!(decide(input(None, 0, false)), WatchdogAction::None);
        // One transient keychain failure (below threshold) is not yet action.
        assert_eq!(decide(input(Some(40), 1, false)), WatchdogAction::None);
    }

    #[test]
    fn decide_fd_warn_boundary() {
        // Just below warn → None.
        assert_eq!(
            decide(input(Some(FD_WARN_THRESHOLD - 1), 0, false)),
            WatchdogAction::None
        );
        // Exactly at warn → Warn.
        assert_eq!(
            decide(input(Some(FD_WARN_THRESHOLD), 0, false)),
            WatchdogAction::Warn
        );
        // Between warn and critical → Warn.
        assert_eq!(
            decide(input(Some(FD_CRITICAL_THRESHOLD - 1), 0, false)),
            WatchdogAction::Warn
        );
    }

    #[test]
    fn decide_fd_critical_boundary_respawns_then_restarts() {
        // Exactly at critical, not yet respawned → RespawnWatcher.
        assert_eq!(
            decide(input(Some(FD_CRITICAL_THRESHOLD), 0, false)),
            WatchdogAction::RespawnWatcher
        );
        // Still critical after a respawn → escalate to SelfRestart.
        assert_eq!(
            decide(input(Some(FD_CRITICAL_THRESHOLD), 0, true)),
            WatchdogAction::SelfRestart
        );
        // Way over critical behaves the same.
        assert_eq!(
            decide(input(Some(10_000), 0, false)),
            WatchdogAction::RespawnWatcher
        );
        assert_eq!(
            decide(input(Some(10_000), 0, true)),
            WatchdogAction::SelfRestart
        );
    }

    #[test]
    fn decide_keychain_failure_boundary() {
        // One below threshold → None.
        assert_eq!(
            decide(input(Some(40), KEYCHAIN_FAILURE_THRESHOLD - 1, false)),
            WatchdogAction::None
        );
        // At threshold, not yet respawned → RespawnWatcher.
        assert_eq!(
            decide(input(Some(40), KEYCHAIN_FAILURE_THRESHOLD, false)),
            WatchdogAction::RespawnWatcher
        );
        // At threshold, already respawned → SelfRestart.
        assert_eq!(
            decide(input(Some(40), KEYCHAIN_FAILURE_THRESHOLD, true)),
            WatchdogAction::SelfRestart
        );
    }

    #[test]
    fn decide_fd_signal_ignored_when_unreadable() {
        // fd_count None must not escalate on its own; only keychain drives it.
        assert_eq!(decide(input(None, 0, false)), WatchdogAction::None);
        assert_eq!(
            decide(input(None, KEYCHAIN_FAILURE_THRESHOLD, false)),
            WatchdogAction::RespawnWatcher
        );
    }

    // --- run_check wiring, with mocked effects (no real watcher/launchctl) ---

    #[derive(Default)]
    struct MockEffects {
        logs: Vec<String>,
        respawns: u32,
        self_restarts: u32,
        /// Whether `self_restart` reports it issued a restart (didn't return).
        restart_issues: bool,
    }
    impl WatchdogEffects for MockEffects {
        fn log(&mut self, msg: &str) {
            self.logs.push(msg.to_string());
        }
        fn respawn_watcher(&mut self) -> (Option<usize>, Option<usize>) {
            self.respawns += 1;
            (Some(4000), Some(42))
        }
        fn self_restart(&mut self) -> bool {
            self.self_restarts += 1;
            self.restart_issues
        }
    }

    #[test]
    fn run_check_escalates_respawn_then_restart() {
        let mut wd = Watchdog::default();
        let mut fx = MockEffects::default();

        // First critical check → respawn, no restart.
        wd.run_check(Some(4000), 0, &mut fx);
        assert_eq!(fx.respawns, 1);
        assert_eq!(fx.self_restarts, 0);
        assert!(wd.already_respawned);

        // Still critical next check → restart, no further respawn.
        wd.run_check(Some(4000), 0, &mut fx);
        assert_eq!(fx.respawns, 1);
        assert_eq!(fx.self_restarts, 1);
    }

    #[test]
    fn run_check_resets_after_recovery() {
        let mut wd = Watchdog::default();
        let mut fx = MockEffects::default();

        wd.run_check(Some(4000), 0, &mut fx); // respawn
        assert!(wd.already_respawned);
        wd.run_check(Some(40), 0, &mut fx); // healthy → reset
        assert!(!wd.already_respawned);
        // A subsequent critical check starts again at respawn, not restart.
        wd.run_check(Some(4000), 0, &mut fx);
        assert_eq!(fx.self_restarts, 0);
        assert_eq!(fx.respawns, 2);
    }

    #[test]
    fn run_check_warn_only_logs() {
        let mut wd = Watchdog::default();
        let mut fx = MockEffects::default();
        wd.run_check(Some(FD_WARN_THRESHOLD), 0, &mut fx);
        assert_eq!(fx.respawns, 0);
        assert_eq!(fx.self_restarts, 0);
        assert!(fx.logs.iter().any(|l| l.contains("level=warn")));
    }

    #[test]
    fn run_check_keychain_failures_trigger_remediation() {
        let mut wd = Watchdog::default();
        let mut fx = MockEffects::default();
        // Healthy fd count, but keychain failing → respawn, with a
        // keychain_watchdog log line naming the failure count.
        wd.run_check(Some(40), KEYCHAIN_FAILURE_THRESHOLD, &mut fx);
        assert_eq!(fx.respawns, 1);
        assert!(fx
            .logs
            .iter()
            .any(|l| l.contains("event=keychain_watchdog")));
    }

    #[test]
    fn record_keychain_result_counts_consecutive_failures() {
        // Use a fresh baseline — other tests may have touched the global.
        record_keychain_result(true);
        assert_eq!(keychain_consecutive_failures(), 0);
        record_keychain_result(false);
        record_keychain_result(false);
        assert_eq!(keychain_consecutive_failures(), 2);
        record_keychain_result(true); // success resets
        assert_eq!(keychain_consecutive_failures(), 0);
    }

    /// A self_restart that fails to issue (bare foreground watch: returns
    /// `false`) must NOT loop back into repeated watcher respawns — the
    /// escalation flag stays set so we don't churn the watcher every cycle.
    #[test]
    fn run_check_failed_self_restart_does_not_rechurn_respawn() {
        let mut wd = Watchdog::default();
        let mut fx = MockEffects {
            restart_issues: false,
            ..Default::default()
        };
        wd.run_check(Some(4000), 0, &mut fx); // respawn
        wd.run_check(Some(4000), 0, &mut fx); // restart attempt (returns false)
        wd.run_check(Some(4000), 0, &mut fx); // still critical
        assert_eq!(fx.respawns, 1, "watcher should not be respawned again");
        assert_eq!(fx.self_restarts, 2, "should keep attempting restart");
        assert!(wd.already_respawned);
    }
}
