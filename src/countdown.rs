//! Countdown-when-locked helper for the usagio menu redesign.
//!
//! When an account is fully consumed (session or weekly at ≥99.5%), the menu
//! row swaps its percentage display for a countdown to when the account is
//! usable again. See `compute_display` for the picking logic.
//!
//! Copied verbatim from `/tmp/usagio-drafts/countdown.rs` (locked spec, 22
//! tests) with two adaptations to match the crate:
//!   * `f32` → `f64` on pct fields (matches `crate::Cell::pct`).
//!   * Test-only `usage()` builder uses the crate `f64` types too.

use chrono::{DateTime, Duration, Utc};

const LOCKED_THRESHOLD_PCT: f64 = 99.5;

#[derive(Clone, Debug)]
pub struct AccountUsage {
    pub session_pct: Option<f64>,
    pub session_reset: Option<DateTime<Utc>>,
    pub weekly_pct: Option<f64>,
    pub weekly_reset: Option<DateTime<Utc>>,
    /// When the cached usage values were fetched. Used to detect a stale
    /// cache that predates a reset boundary (see `DisplayState::StaleAfterReset`).
    pub fetched_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DisplayState {
    Usage {
        session_pct: Option<f64>,
        weekly_pct: Option<f64>,
    },
    Locked {
        until: DateTime<Utc>,
        window: BlockingWindow,
    },
    /// The cached percentage is at/above the locked threshold, but the
    /// window's reset time has already passed *and* our last fetch predates
    /// that reset — i.e. the cache is known-stale rather than a live
    /// "still locked" reading. Rendering the raw cached pct here (typically
    /// a misleading `100%`) would lie to the user; callers should render a
    /// "pending refresh" placeholder (e.g. "-% / -%") until the next poll
    /// picks up fresh, post-reset data.
    StaleAfterReset {
        window: BlockingWindow,
        reset: DateTime<Utc>,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockingWindow {
    Session,
    Weekly,
}

pub fn compute_display(usage: &AccountUsage, now: DateTime<Utc>) -> DisplayState {
    // A window is "blocking" only if it's over the threshold AND its reset is
    // still in the future. Stale reset times (past) are treated as unlocked
    // since data is presumed stale — the next refresh will correct it.
    let session_blocking = is_blocking(usage.session_pct, usage.session_reset, now);
    let weekly_blocking = is_blocking(usage.weekly_pct, usage.weekly_reset, now);

    match (session_blocking, weekly_blocking) {
        (Some(s), Some(w)) => {
            // Both blocking — pick the sooner reset. Sooner = smaller DateTime.
            if s <= w {
                DisplayState::Locked {
                    until: s,
                    window: BlockingWindow::Session,
                }
            } else {
                DisplayState::Locked {
                    until: w,
                    window: BlockingWindow::Weekly,
                }
            }
        }
        (Some(s), None) => DisplayState::Locked {
            until: s,
            window: BlockingWindow::Session,
        },
        (None, Some(w)) => DisplayState::Locked {
            until: w,
            window: BlockingWindow::Weekly,
        },
        (None, None) => {
            // Neither window is actively blocking. Before falling back to a
            // plain Usage display, check whether either window's cached pct
            // is stale-after-reset: still at/above threshold, its reset has
            // already passed, and our last fetch predates that reset. That
            // combination means the cached value is a known lie (it can't
            // still be true post-reset) rather than fresh data that happens
            // to be low/zero. Session takes priority when both are stale,
            // mirroring the Locked-branch tie-break above.
            let session_stale = is_stale_after_reset(
                usage.session_pct,
                usage.session_reset,
                usage.fetched_at,
                now,
            );
            let weekly_stale =
                is_stale_after_reset(usage.weekly_pct, usage.weekly_reset, usage.fetched_at, now);

            match (session_stale, weekly_stale) {
                (Some(reset), _) => DisplayState::StaleAfterReset {
                    window: BlockingWindow::Session,
                    reset,
                },
                (None, Some(reset)) => DisplayState::StaleAfterReset {
                    window: BlockingWindow::Weekly,
                    reset,
                },
                (None, None) => DisplayState::Usage {
                    session_pct: usage.session_pct,
                    weekly_pct: usage.weekly_pct,
                },
            }
        }
    }
}

fn is_blocking(
    pct: Option<f64>,
    reset: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let pct = pct?;
    let reset = reset?;
    if pct >= LOCKED_THRESHOLD_PCT && reset > now {
        Some(reset)
    } else {
        None
    }
}

/// Whether a usage refresh can be skipped this cycle because the account is
/// locked: `true` iff some window is at/above the lock threshold with a reset
/// still in the future (i.e. [`is_blocking`] fires for session or weekly).
///
/// While any window is locked with an unexpired reset, the account is
/// unusable, so its usage cannot change until that reset — polling it only
/// burns request budget (and risks the shared-tenant HTTP 429s that then
/// starve the accounts we DO need to refresh). The reset-boundary wake
/// (`menubar::next_reset_wake_secs`) brings the poller back exactly at expiry,
/// at which point this returns `false` (the reset is now in the past) and the
/// account refreshes to its fresh post-reset reading.
///
/// Returns `false` when the account is NOT locked (refresh normally), when a
/// lock's reset has already passed (stale-after-reset — must refresh to
/// correct the cache), and when a window is at the limit but its reset time is
/// unknown (we can't prove when it unlocks, so we don't skip). The caller must
/// never skip the ACTIVE account — its adopt/CAS keeps the vendor slot in sync
/// regardless of lock state.
pub fn is_locked_until_reset(u: &AccountUsage, now: DateTime<Utc>) -> bool {
    is_blocking(u.session_pct, u.session_reset, now).is_some()
        || is_blocking(u.weekly_pct, u.weekly_reset, now).is_some()
}

/// Mirrors `is_blocking`, but for the *already-reset* case: the reset time
/// has passed (so `is_blocking` returns `None`), yet the cached pct is still
/// at/above threshold because it was fetched before that reset happened.
fn is_stale_after_reset(
    pct: Option<f64>,
    reset: Option<DateTime<Utc>>,
    fetched_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let pct = pct?;
    let reset = reset?;
    let fetched_at = fetched_at?;
    if pct >= LOCKED_THRESHOLD_PCT && reset <= now && fetched_at < reset {
        Some(reset)
    } else {
        None
    }
}

/// Returns the soonest of `{session_reset, weekly_reset}` that falls
/// strictly within `horizon` of `now` (i.e. `now < reset <= now + horizon`),
/// or `None` if neither reset is known or both fall outside the horizon.
///
/// Intended for `main.rs`'s `cmd_watch` loop: rather than waiting for the
/// next adaptive-cadence poll tick to notice a window has rolled over
/// (which can leave a stale `Locked`/cached-100% reading on screen for up
/// to that tick's full interval), the watch loop can call this to find the
/// next reset boundary and schedule an extra wake-up right at/after it, so
/// the stale-cache window is minimized.
pub fn any_reset_within(
    u: &AccountUsage,
    now: DateTime<Utc>,
    horizon: Duration,
) -> Option<DateTime<Utc>> {
    let deadline = now + horizon;
    [u.session_reset, u.weekly_reset]
        .into_iter()
        .flatten()
        .filter(|&reset| reset > now && reset <= deadline)
        .min()
}

/// Format a remaining duration into one of: "1d 23h" | "23h 52m" | "51m" | "<1m".
/// Saturating: negative/huge durations are clamped to "<1m" and the largest
/// representable-day-count respectively; never panics.
pub fn format_countdown(remaining: Duration) -> String {
    // Saturating: anything at-or-below zero → "<1m" (the "never show 0" rule).
    if remaining <= Duration::zero() {
        return "<1m".to_string();
    }

    let total_secs = remaining.num_seconds();
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86_400;

    if total_days >= 1 {
        let hours_within_day = (total_secs % 86_400) / 3600;
        format!("{}d {}h", total_days, hours_within_day)
    } else if total_hours >= 1 {
        let mins_within_hour = (total_secs % 3600) / 60;
        format!("{}h {}m", total_hours, mins_within_hour)
    } else if total_mins >= 1 {
        format!("{}m", total_mins)
    } else {
        // 0 < remaining < 60s
        "<1m".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs_from_epoch: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs_from_epoch, 0).unwrap()
    }

    // ---- format_countdown ----

    #[test]
    fn format_zero_is_lt_1m() {
        assert_eq!(format_countdown(Duration::zero()), "<1m");
    }

    #[test]
    fn format_sub_minute_is_lt_1m() {
        assert_eq!(format_countdown(Duration::seconds(1)), "<1m");
        assert_eq!(format_countdown(Duration::seconds(59)), "<1m");
    }

    #[test]
    fn format_one_minute() {
        assert_eq!(format_countdown(Duration::seconds(60)), "1m");
    }

    #[test]
    fn format_59_minutes() {
        assert_eq!(format_countdown(Duration::minutes(59)), "59m");
    }

    #[test]
    fn format_60_minutes_is_1h_0m() {
        assert_eq!(format_countdown(Duration::minutes(60)), "1h 0m");
    }

    #[test]
    fn format_61_minutes_is_1h_1m() {
        assert_eq!(format_countdown(Duration::minutes(61)), "1h 1m");
    }

    #[test]
    fn format_23h_59m() {
        assert_eq!(
            format_countdown(Duration::hours(23) + Duration::minutes(59)),
            "23h 59m"
        );
    }

    #[test]
    fn format_24h_is_1d_0h() {
        assert_eq!(format_countdown(Duration::hours(24)), "1d 0h");
    }

    #[test]
    fn format_1d_23h() {
        assert_eq!(
            format_countdown(Duration::days(1) + Duration::hours(23)),
            "1d 23h"
        );
    }

    #[test]
    fn format_6d_23h() {
        assert_eq!(
            format_countdown(Duration::days(6) + Duration::hours(23)),
            "6d 23h"
        );
    }

    #[test]
    fn format_7d_0h() {
        assert_eq!(format_countdown(Duration::days(7)), "7d 0h");
    }

    #[test]
    fn format_negative_saturates_to_lt_1m() {
        assert_eq!(format_countdown(Duration::seconds(-999)), "<1m");
    }

    #[test]
    fn format_huge_duration_does_not_panic() {
        // 10 years — well beyond any real weekly reset. Must not panic.
        let s = format_countdown(Duration::days(3650));
        assert!(s.ends_with(" 0h") && s.starts_with("3650d"));
    }

    // ---- compute_display ----

    fn usage(sp: Option<f64>, sr: Option<i64>, wp: Option<f64>, wr: Option<i64>) -> AccountUsage {
        AccountUsage {
            session_pct: sp,
            session_reset: sr.map(t),
            weekly_pct: wp,
            weekly_reset: wr.map(t),
            fetched_at: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn usage_with_fetched_at(
        sp: Option<f64>,
        sr: Option<i64>,
        wp: Option<f64>,
        wr: Option<i64>,
        fetched_at: i64,
    ) -> AccountUsage {
        AccountUsage {
            session_pct: sp,
            session_reset: sr.map(t),
            weekly_pct: wp,
            weekly_reset: wr.map(t),
            fetched_at: Some(t(fetched_at)),
        }
    }

    // ---- is_locked_until_reset (refresh-skip predicate) ----

    #[test]
    fn not_locked_is_not_skippable() {
        // Both windows below threshold → must refresh (the pq.io case).
        let u = usage(Some(83.0), Some(2000), Some(75.0), Some(5000));
        assert!(!is_locked_until_reset(&u, t(1000)));
    }

    #[test]
    fn session_locked_future_reset_is_skippable() {
        let u = usage(Some(100.0), Some(2000), Some(50.0), Some(5000));
        assert!(is_locked_until_reset(&u, t(1000)));
    }

    #[test]
    fn weekly_locked_future_reset_is_skippable() {
        let u = usage(Some(30.0), Some(2000), Some(100.0), Some(5000));
        assert!(is_locked_until_reset(&u, t(1000)));
    }

    #[test]
    fn locked_but_reset_already_passed_is_not_skippable() {
        // Reset in the past: cache is stale-after-reset, must refresh.
        let u = usage(Some(100.0), Some(500), Some(50.0), Some(400));
        assert!(!is_locked_until_reset(&u, t(1000)));
    }

    #[test]
    fn at_limit_without_reset_time_is_not_skippable() {
        // We can't prove when it unlocks, so we don't skip.
        let u = usage(Some(100.0), None, Some(100.0), None);
        assert!(!is_locked_until_reset(&u, t(1000)));
    }

    #[test]
    fn boundary_99_5_locked_is_skippable_99_4_is_not() {
        let at = usage(Some(99.5), Some(2000), None, None);
        assert!(is_locked_until_reset(&at, t(1000)));
        let below = usage(Some(99.4), Some(2000), None, None);
        assert!(!is_locked_until_reset(&below, t(1000)));
    }

    #[test]
    fn neither_locked_returns_usage() {
        let u = usage(Some(42.0), Some(2000), Some(61.0), Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Usage {
                session_pct: Some(42.0),
                weekly_pct: Some(61.0)
            }
        );
    }

    #[test]
    fn session_locked_only() {
        let u = usage(Some(100.0), Some(2000), Some(61.0), Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked {
                until: t(2000),
                window: BlockingWindow::Session
            }
        );
    }

    #[test]
    fn weekly_locked_only() {
        let u = usage(Some(42.0), Some(2000), Some(100.0), Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked {
                until: t(5000),
                window: BlockingWindow::Weekly
            }
        );
    }

    #[test]
    fn both_locked_sooner_reset_wins_session() {
        let u = usage(Some(100.0), Some(2000), Some(100.0), Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked {
                until: t(2000),
                window: BlockingWindow::Session
            }
        );
    }

    #[test]
    fn both_locked_sooner_reset_wins_weekly() {
        let u = usage(Some(100.0), Some(5000), Some(100.0), Some(2000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked {
                until: t(2000),
                window: BlockingWindow::Weekly
            }
        );
    }

    #[test]
    fn both_locked_equal_reset_prefers_session() {
        // Ambiguity resolution: exact tie → Session (the shorter-cycle window).
        let u = usage(Some(100.0), Some(3000), Some(100.0), Some(3000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked {
                until: t(3000),
                window: BlockingWindow::Session
            }
        );
    }

    #[test]
    fn stale_reset_treated_as_unlocked() {
        // pct is at 100 but reset time is in the past — data is stale.
        let u = usage(Some(100.0), Some(500), Some(50.0), Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Usage {
                session_pct: Some(100.0),
                weekly_pct: Some(50.0)
            }
        );
    }

    #[test]
    fn boundary_exactly_99_5_is_locked() {
        let u = usage(Some(99.5), Some(2000), None, None);
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked {
                until: t(2000),
                window: BlockingWindow::Session
            }
        );
    }

    #[test]
    fn boundary_99_4_is_not_locked() {
        let u = usage(Some(99.4), Some(2000), None, None);
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Usage {
                session_pct: Some(99.4),
                weekly_pct: None
            }
        );
    }

    #[test]
    fn missing_reset_time_is_not_locked() {
        // pct at 100 but no reset time known → treat as usage, not locked.
        let u = usage(Some(100.0), None, None, None);
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Usage {
                session_pct: Some(100.0),
                weekly_pct: None
            }
        );
    }

    #[test]
    fn missing_pct_never_locks() {
        let u = usage(None, Some(2000), None, Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Usage {
                session_pct: None,
                weekly_pct: None
            }
        );
    }

    // ---- stale-after-reset ----

    #[test]
    fn stale_after_weekly_reset_returns_stale_variant() {
        // weekly at 100%, reset 30s ago, but our cache was fetched 5min ago
        // (i.e. before the reset happened) — the cached 100% is a known lie.
        let now = t(10_000);
        let weekly_reset = 9_970; // 30s before `now`
        let fetched_at = 9_700; // 5min before `now`, i.e. before weekly_reset too
        let u = usage_with_fetched_at(None, None, Some(100.0), Some(weekly_reset), fetched_at);
        assert_eq!(
            compute_display(&u, now),
            DisplayState::StaleAfterReset {
                window: BlockingWindow::Weekly,
                reset: t(weekly_reset),
            }
        );
    }

    #[test]
    fn fresh_poll_after_reset_returns_usage_variant() {
        // Same reset as above, but fetched_at is only 10s ago — i.e. after
        // the reset happened, so the cached pct (whatever it is) is fresh
        // and trusted as-is.
        let now = t(10_000);
        let weekly_reset = 9_970; // 30s before `now`
        let fetched_at = 9_990; // 10s before `now`, i.e. after weekly_reset
        let u = usage_with_fetched_at(None, None, Some(0.0), Some(weekly_reset), fetched_at);
        assert_eq!(
            compute_display(&u, now),
            DisplayState::Usage {
                session_pct: None,
                weekly_pct: Some(0.0),
            }
        );
    }

    #[test]
    fn still_before_reset_returns_locked_variant() {
        // Reset is still 2min in the future — current locked behavior must
        // be preserved regardless of fetched_at.
        let now = t(10_000);
        let session_reset = 10_120; // +2min
        let u = usage_with_fetched_at(Some(100.0), Some(session_reset), None, None, 9_700);
        assert_eq!(
            compute_display(&u, now),
            DisplayState::Locked {
                until: t(session_reset),
                window: BlockingWindow::Session,
            }
        );
    }

    // ---- any_reset_within ----

    #[test]
    fn any_reset_within_finds_nearest() {
        let now = t(10_000);
        let session_reset = 10_300; // +5min
        let weekly_reset = 10_600; // +10min
        let u = usage(
            Some(10.0),
            Some(session_reset),
            Some(20.0),
            Some(weekly_reset),
        );
        let horizon = Duration::minutes(7);
        assert_eq!(any_reset_within(&u, now, horizon), Some(t(session_reset)));
    }
}
