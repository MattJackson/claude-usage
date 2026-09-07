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
                DisplayState::Locked { until: s, window: BlockingWindow::Session }
            } else {
                DisplayState::Locked { until: w, window: BlockingWindow::Weekly }
            }
        }
        (Some(s), None) => DisplayState::Locked { until: s, window: BlockingWindow::Session },
        (None, Some(w)) => DisplayState::Locked { until: w, window: BlockingWindow::Weekly },
        (None, None) => DisplayState::Usage {
            session_pct: usage.session_pct,
            weekly_pct: usage.weekly_pct,
        },
    }
}

fn is_blocking(pct: Option<f64>, reset: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let pct = pct?;
    let reset = reset?;
    if pct >= LOCKED_THRESHOLD_PCT && reset > now {
        Some(reset)
    } else {
        None
    }
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
        }
    }

    #[test]
    fn neither_locked_returns_usage() {
        let u = usage(Some(42.0), Some(2000), Some(61.0), Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Usage { session_pct: Some(42.0), weekly_pct: Some(61.0) }
        );
    }

    #[test]
    fn session_locked_only() {
        let u = usage(Some(100.0), Some(2000), Some(61.0), Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked { until: t(2000), window: BlockingWindow::Session }
        );
    }

    #[test]
    fn weekly_locked_only() {
        let u = usage(Some(42.0), Some(2000), Some(100.0), Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked { until: t(5000), window: BlockingWindow::Weekly }
        );
    }

    #[test]
    fn both_locked_sooner_reset_wins_session() {
        let u = usage(Some(100.0), Some(2000), Some(100.0), Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked { until: t(2000), window: BlockingWindow::Session }
        );
    }

    #[test]
    fn both_locked_sooner_reset_wins_weekly() {
        let u = usage(Some(100.0), Some(5000), Some(100.0), Some(2000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked { until: t(2000), window: BlockingWindow::Weekly }
        );
    }

    #[test]
    fn both_locked_equal_reset_prefers_session() {
        // Ambiguity resolution: exact tie → Session (the shorter-cycle window).
        let u = usage(Some(100.0), Some(3000), Some(100.0), Some(3000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked { until: t(3000), window: BlockingWindow::Session }
        );
    }

    #[test]
    fn stale_reset_treated_as_unlocked() {
        // pct is at 100 but reset time is in the past — data is stale.
        let u = usage(Some(100.0), Some(500), Some(50.0), Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Usage { session_pct: Some(100.0), weekly_pct: Some(50.0) }
        );
    }

    #[test]
    fn boundary_exactly_99_5_is_locked() {
        let u = usage(Some(99.5), Some(2000), None, None);
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Locked { until: t(2000), window: BlockingWindow::Session }
        );
    }

    #[test]
    fn boundary_99_4_is_not_locked() {
        let u = usage(Some(99.4), Some(2000), None, None);
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Usage { session_pct: Some(99.4), weekly_pct: None }
        );
    }

    #[test]
    fn missing_reset_time_is_not_locked() {
        // pct at 100 but no reset time known → treat as usage, not locked.
        let u = usage(Some(100.0), None, None, None);
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Usage { session_pct: Some(100.0), weekly_pct: None }
        );
    }

    #[test]
    fn missing_pct_never_locks() {
        let u = usage(None, Some(2000), None, Some(5000));
        assert_eq!(
            compute_display(&u, t(1000)),
            DisplayState::Usage { session_pct: None, weekly_pct: None }
        );
    }
}
