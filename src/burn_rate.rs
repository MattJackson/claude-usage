// burn_rate.rs — per-window per-account forecast of when a quota will be exhausted.
//
// Consumes samples persisted by src/usage_log.rs (Snapshot { ts, provider, account,
// session_pct, weekly_pct, active_model }).  Weighted linear regression, recent
// samples weighted higher.  Requires at least MIN_SAMPLES within the current window;
// slope <= FLAT_SLOPE_THRESHOLD is treated as "rate flat" (below noise floor).
//
// Wire into menu redesign as a DetailRow beneath the corresponding Session/Weekly row
// when confidence > CONFIDENCE_FLOOR (default 0.5).  See README-DRAFT.md.

use chrono::{DateTime, Duration, Utc};

use crate::providers::trait_def::Window;
use crate::usage_log::{self, AccountKey, Snapshot};

/// Minimum samples inside the current window required before we'll fit a rate.
pub const MIN_SAMPLES: usize = 6;

/// Slope below this magnitude is reported as "flat" — regression noise dominates.
pub const FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR: f32 = 0.5;

/// Ceiling on projected "empty in" — anything longer collapses to this to avoid
/// nonsense like "empty in ~470h."  Chosen so the menu row stays under one screen line.
pub const MAX_EMPTY_IN_HOURS: i64 = 999;

/// Fits below this R² are still reported but a caller can gate rendering on it
/// (menu redesign uses this).
pub const CONFIDENCE_FLOOR: f32 = 0.5;

#[derive(Clone, Debug)]
pub struct BurnRateEstimate {
    pub window: Window,
    pub current_pct: f32,
    /// Slope of the linear fit, in "percent points per hour."  May be zero.
    pub rate_pct_per_hour: f32,
    /// R² of the fit, clamped to [0, 1].
    pub confidence: f32,
    /// Projected time to hit 100%.  If `rate_pct_per_hour <= FLAT_SLOPE_THRESHOLD`
    /// this is `now + MAX_EMPTY_IN_HOURS`; callers should render via
    /// `format_menu_row` which handles the flat case in text.
    pub empty_at: DateTime<Utc>,
    pub reset_at: Option<DateTime<Utc>>,
    /// `reset_at - empty_at`.  `None` when there's no reset time, when `empty_at`
    /// is after `reset_at` (we're safe for this cycle), or when the rate is flat.
    pub margin: Option<Duration>,
}

pub fn estimate(
    account: &AccountKey,
    window: Window,
    now: DateTime<Utc>,
) -> Option<BurnRateEstimate> {
    // Session window looks at the last 5h.  Weekly looks at 7d.  In both cases we
    // rely on the log-side filter to only return samples inside the CURRENT cycle
    // (i.e. after the most recent reset); if it doesn't, the linear fit will still
    // be reasonable but the confidence will be lower.
    let lookback_days: u32 = match window {
        Window::Session => 1, // 5h fits in 1 day
        Window::Weekly => 8,
    };
    let mut samples = usage_log::last_n_days(account, lookback_days);
    // Keep only the ones whose target-window pct is Some, ordered by ts asc.
    samples.retain(|s| pct_for(&s, window).is_some());
    samples.sort_by_key(|s| s.ts);

    // Apply the per-window horizon filter.
    let horizon = match window {
        Window::Session => Duration::hours(5),
        Window::Weekly => Duration::days(7),
    };
    let cutoff = now - horizon;
    samples.retain(|s| s.ts >= cutoff);

    if samples.len() < MIN_SAMPLES {
        return None;
    }

    let current_pct = pct_for(samples.last().unwrap(), window).unwrap();
    let reset_at = reset_for(samples.last().unwrap(), window);

    let (slope_per_hour, intercept, r2) = weighted_linear_fit(&samples, window, now)?;

    // Empty-at projection: solve intercept + slope*t = 100 (t is hours from `now`).
    let (empty_at, margin) = if slope_per_hour <= FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR {
        // Flat: cap far in the future, no margin.
        (
            now + Duration::hours(MAX_EMPTY_IN_HOURS),
            None,
        )
    } else {
        // hours_from_now = (100 - current_pct) / slope
        let hours_to_empty = ((100.0 - current_pct) / slope_per_hour).max(0.0) as i64;
        let empty_at = now + Duration::hours(hours_to_empty.min(MAX_EMPTY_IN_HOURS));
        let margin = reset_at.and_then(|r| {
            if r > empty_at {
                Some(r - empty_at)
            } else {
                None
            }
        });
        (empty_at, margin)
    };

    // Silence unused-var lint on intercept — kept for debug/logging clarity.
    let _ = intercept;

    Some(BurnRateEstimate {
        window,
        current_pct,
        rate_pct_per_hour: slope_per_hour,
        confidence: r2.clamp(0.0, 1.0),
        empty_at,
        reset_at,
        margin,
    })
}

/// Render a menu row like "Weekly · 61% · empty in ~40m · 6m before reset"
/// or "Weekly · 61% · empty in ~2h 15m · safe for this cycle"
/// or "Weekly · 61% · rate flat".
pub fn format_menu_row(est: &BurnRateEstimate) -> String {
    let name = match est.window {
        Window::Session => "Session",
        Window::Weekly => "Weekly",
    };
    if est.rate_pct_per_hour <= FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR {
        return format!("{name} · {:.0}% · rate flat", est.current_pct);
    }
    let remaining = est.empty_at - Utc::now();
    let empty_str = format_short_duration(remaining);
    match est.margin {
        Some(m) if m > Duration::zero() => {
            format!(
                "{name} · {:.0}% · empty in ~{empty_str} · safe for this cycle ({} to spare)",
                est.current_pct,
                format_short_duration(m)
            )
        }
        Some(_) => unreachable!(),
        None if est.reset_at.is_some() => {
            // Empty falls before reset: figure out how much before.
            let reset = est.reset_at.unwrap();
            let before = reset - est.empty_at;
            format!(
                "{name} · {:.0}% · empty in ~{empty_str} · {} before reset",
                est.current_pct,
                format_short_duration(before)
            )
        }
        None => format!("{name} · {:.0}% · empty in ~{empty_str}", est.current_pct),
    }
}

fn pct_for(s: &Snapshot, w: Window) -> Option<f32> {
    match w {
        Window::Session => s.session_pct,
        Window::Weekly => s.weekly_pct,
    }
}

fn reset_for(s: &Snapshot, w: Window) -> Option<DateTime<Utc>> {
    // Snapshot doesn't carry reset time today; the caller (renderer) supplies it from
    // the account's cached usage.  We keep the field on BurnRateEstimate so the
    // renderer can populate it after `estimate`.  Return None here so the module is
    // testable standalone.
    let _ = (s, w);
    None
}

/// Weighted linear fit of pct vs hours-from-now.  Weights: exponential decay, most
/// recent samples weighted 1.0, oldest ~0.35 (half-life = window_horizon / 2).
///
/// Returns (slope_pct_per_hour, intercept_pct_at_now, r_squared).
fn weighted_linear_fit(
    samples: &[Snapshot],
    window: Window,
    now: DateTime<Utc>,
) -> Option<(f32, f32, f32)> {
    if samples.len() < MIN_SAMPLES {
        return None;
    }
    let horizon_hours = match window {
        Window::Session => 5.0f32,
        Window::Weekly => 24.0 * 7.0,
    };
    let half_life = horizon_hours * 0.5;
    let ln2 = std::f32::consts::LN_2;

    // x_i = hours before now (positive going into the past).
    let xs: Vec<f32> = samples
        .iter()
        .map(|s| {
            let d = now - s.ts;
            (d.num_seconds() as f32) / 3600.0
        })
        .collect();
    let ys: Vec<f32> = samples
        .iter()
        .map(|s| pct_for(s, window).unwrap())
        .collect();
    let ws: Vec<f32> = xs.iter().map(|x| (-x * ln2 / half_life).exp()).collect();

    let sum_w: f32 = ws.iter().sum();
    if sum_w <= f32::EPSILON {
        return None;
    }
    let mean_x = xs.iter().zip(&ws).map(|(x, w)| x * w).sum::<f32>() / sum_w;
    let mean_y = ys.iter().zip(&ws).map(|(y, w)| y * w).sum::<f32>() / sum_w;

    let mut cov_xy = 0.0f32;
    let mut var_x = 0.0f32;
    let mut var_y = 0.0f32;
    for i in 0..xs.len() {
        let dx = xs[i] - mean_x;
        let dy = ys[i] - mean_y;
        cov_xy += ws[i] * dx * dy;
        var_x += ws[i] * dx * dx;
        var_y += ws[i] * dy * dy;
    }
    if var_x < f32::EPSILON {
        return None;
    }
    // Slope in y-per-x, i.e. pct per hour PAST-facing.  Flip sign for future-facing.
    let slope_past = cov_xy / var_x;
    let slope_future = -slope_past;
    // Intercept "at now" (x=0).
    let intercept = mean_y - slope_past * mean_x;
    let r2 = if var_y < f32::EPSILON {
        0.0
    } else {
        (cov_xy * cov_xy) / (var_x * var_y)
    };
    Some((slope_future, intercept, r2))
}

/// "1d 23h" / "23h 52m" / "51m" / "<1m" — mirrors src/countdown.rs formatting rules.
///
/// Sub-hour buckets round to the nearest minute so a fixture that computes
/// `now + Duration::minutes(40)` still prints "40m" when the format helper is
/// called a few milliseconds later. Hour/day buckets keep floor semantics so
/// "23h 59m" doesn't jump to "1d 0h" one second before the boundary.
fn format_short_duration(d: Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        return if secs > 0 { "<1m".to_string() } else { "0m".to_string() };
    }
    let mins_total = secs / 60;
    let hours_total = mins_total / 60;
    let days = hours_total / 24;
    if days >= 1 {
        let hours = hours_total % 24;
        return format!("{days}d {hours}h");
    }
    if hours_total >= 1 {
        let mins = mins_total % 60;
        return format!("{hours_total}h {mins}m");
    }
    // Sub-hour: round to nearest minute so a value one second shy of the next
    // minute doesn't display as the previous minute. Cap at 59 so we never
    // overflow into "60m" (a caller with 3599s + rounding lands here).
    let mins_rounded = ((secs + 30) / 60).min(59);
    format!("{mins_rounded}m")
}

// ─── tests ────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::trait_def::Window;
    use crate::usage_log::{AccountKey, Snapshot};

    fn snap(mins_ago: i64, weekly: f32) -> Snapshot {
        Snapshot {
            ts: Utc::now() - Duration::minutes(mins_ago),
            provider: "claude".into(),
            account: "matt@example.com".into(),
            session_pct: None,
            weekly_pct: Some(weekly),
            active_model: None,
        }
    }

    // Linear ascent 0% -> 60% over 6 hours = 10 pct/hr.  Expect empty_at ~= now + 4h.
    #[test]
    fn linear_ascent_projects_correctly() {
        // Build a synthetic sample series: one sample every 30 min for 6h, 0..60% linear.
        let now = Utc::now();
        let samples: Vec<Snapshot> = (0..=12)
            .map(|i| {
                let mins_ago = (12 - i) * 30;
                let pct = i as f32 * 5.0;
                Snapshot {
                    ts: now - Duration::minutes(mins_ago),
                    provider: "claude".into(),
                    account: "matt@example.com".into(),
                    session_pct: None,
                    weekly_pct: Some(pct),
                    active_model: None,
                }
            })
            .collect();
        let (slope, _intercept, r2) = weighted_linear_fit(&samples, Window::Weekly, now).unwrap();
        assert!(
            (slope - 10.0).abs() < 1.5,
            "slope ~10 pct/hr, got {slope}"
        );
        assert!(r2 > 0.95, "R² should be near 1 for a linear ascent, got {r2}");
    }

    #[test]
    fn flat_rate_is_reported_flat() {
        let now = Utc::now();
        let samples: Vec<Snapshot> = (0..8).map(|i| snap(i * 15, 30.0)).collect();
        let (slope, _, _) = weighted_linear_fit(&samples, Window::Weekly, now).unwrap();
        assert!(slope.abs() <= FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR + 0.1);
    }

    #[test]
    fn fewer_than_min_samples_returns_none() {
        let now = Utc::now();
        let samples: Vec<Snapshot> = (0..(MIN_SAMPLES - 1))
            .map(|i| snap(i as i64 * 10, i as f32 * 3.0))
            .collect();
        assert!(weighted_linear_fit(&samples, Window::Weekly, now).is_none());
    }

    #[test]
    fn format_menu_row_flat() {
        let est = BurnRateEstimate {
            window: Window::Weekly,
            current_pct: 42.0,
            rate_pct_per_hour: 0.1,
            confidence: 0.9,
            empty_at: Utc::now() + Duration::hours(500),
            reset_at: None,
            margin: None,
        };
        assert_eq!(format_menu_row(&est), "Weekly · 42% · rate flat");
    }

    #[test]
    fn format_menu_row_safe_for_cycle() {
        let est = BurnRateEstimate {
            window: Window::Weekly,
            current_pct: 61.0,
            rate_pct_per_hour: 15.0,
            confidence: 0.9,
            empty_at: Utc::now() + Duration::hours(2) + Duration::minutes(15),
            reset_at: Some(Utc::now() + Duration::hours(10)),
            margin: Some(Duration::hours(7) + Duration::minutes(45)),
        };
        let s = format_menu_row(&est);
        assert!(s.contains("empty in ~2h") && s.contains("safe for this cycle"), "{s}");
    }

    #[test]
    fn format_menu_row_at_risk() {
        let est = BurnRateEstimate {
            window: Window::Weekly,
            current_pct: 61.0,
            rate_pct_per_hour: 40.0,
            confidence: 0.9,
            empty_at: Utc::now() + Duration::minutes(40),
            reset_at: Some(Utc::now() + Duration::minutes(46)),
            margin: None,
        };
        let s = format_menu_row(&est);
        assert!(s.contains("empty in ~40m") && s.contains("before reset"), "{s}");
    }

    #[test]
    fn format_short_duration_ladder() {
        assert_eq!(format_short_duration(Duration::seconds(0)), "0m");
        assert_eq!(format_short_duration(Duration::seconds(30)), "<1m");
        assert_eq!(format_short_duration(Duration::minutes(1)), "1m");
        assert_eq!(format_short_duration(Duration::minutes(59)), "59m");
        assert_eq!(format_short_duration(Duration::minutes(60)), "1h 0m");
        assert_eq!(format_short_duration(Duration::minutes(60 + 23)), "1h 23m");
        assert_eq!(format_short_duration(Duration::hours(23) + Duration::minutes(59)), "23h 59m");
        assert_eq!(format_short_duration(Duration::hours(24)), "1d 0h");
        assert_eq!(format_short_duration(Duration::days(1) + Duration::hours(23)), "1d 23h");
    }

    // Ignored by default — this hits the live usage_log crate function, which panics
    // in unit-test mode without a fixture harness.  See tests/integration when wiring.
    #[test]
    #[ignore]
    fn estimate_via_public_api() {
        let key = AccountKey::default();
        let _ = estimate(&key, Window::Weekly, Utc::now());
    }
}
