//! Threshold / reset-back / weekly-pace notifications driven by the watch
//! cycle. `evaluate` is pure over a (prev, curr) usage-log snapshot pair;
//! `dedup_and_apply` walks the resulting `Vec<Trigger>` against a small
//! per-account `NotifState` so a crossing that already fired doesn't re-fire
//! every poll, and so a genuine window reset re-arms the same thresholds for
//! the next cycle. `fire` hands the formatted (summary, body) to notify-rust.
//!
//! The module is deliberately unopinionated about where `NotifState` lives —
//! v1 hangs it off `store::Account` (see the `notif_state` field), a future
//! phase can move it into the StateV2 per-account bucket instead. The pure
//! `evaluate` / `dedup_and_apply` split lets tests exercise the crossing
//! algebra without any I/O; `fire` is the only function that touches DBus /
//! NSUserNotification.

use std::collections::BTreeSet;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::usage_log::{AccountKey, PaceEstimate, Snapshot};

/// Which usage window a trigger pertains to. Serialized as a kebab-case
/// string so persisted state (crossings set on `NotifState`) is
/// human-inspectable.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Window {
    Session,
    Weekly,
}

impl Window {
    /// Short human label used in notification bodies.
    pub fn label(self) -> &'static str {
        match self {
            Window::Session => "Session (5h)",
            Window::Weekly => "Weekly (7d)",
        }
    }
}

/// One notification-worthy event. `Threshold` carries the exact percent that
/// was crossed so the same window can fire twice per cycle (e.g. 70 then 90).
/// `WeeklyPace` carries the projected 100%-hit time so the body can name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The `window`'s utilization just crossed `pct` upward (typ. 70 or 90).
    Threshold { window: Window, pct: u8 },
    /// The `window` just reset — utilization dropped from a non-trivial prior
    /// level, so the user has fresh headroom.
    ResetBack { window: Window },
    /// Weekly pace projects hitting 100% at `hit_time` before the weekly
    /// reset. Fires at most once per weekly window.
    WeeklyPace { hit_time: DateTime<Utc> },
}

/// Per-trigger enable flags. Thresholds and reset-back default on; pace is
/// experimental and defaults off (the user opts in from Settings ▸
/// Notifications).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationConfig {
    pub threshold_enabled: bool,
    pub reset_back_enabled: bool,
    pub pace_enabled: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        NotificationConfig {
            threshold_enabled: true,
            reset_back_enabled: true,
            pace_enabled: false,
        }
    }
}

/// Persisted per-account bookkeeping used to dedup fires across ticks. Kept
/// tiny (a `BTreeSet` and a `bool`) so hanging it off every `Account` in
/// state.json is cheap. `BTreeSet` (not `HashSet`) so JSON output is
/// deterministic across saves — a state.json diff shouldn't churn on set
/// iteration order.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotifState {
    /// Thresholds already fired since the last reset of their window.
    #[serde(default)]
    pub crossings: BTreeSet<(Window, u8)>,
    /// True if this account has already fired a WeeklyPace notification
    /// during its current weekly window.
    #[serde(default)]
    pub pace_fired_this_window: bool,
}

/// Thresholds we check on every tick, ascending so a `Vec<Trigger>` from
/// `evaluate` is deterministically ordered (session-70, session-90,
/// weekly-70, weekly-90). Keeping this a `const` (not a config field) is
/// intentional — the "70 / 90" ladder is the product spec; users tune what's
/// enabled via `NotificationConfig`, not which numbers are checked.
const THRESHOLDS: [u8; 2] = [70, 90];

// ---------------------------------------------------------------------------
// Pure algebra: evaluate + dedup_and_apply
// ---------------------------------------------------------------------------

/// Pure: raw candidate triggers implied by the (prev, curr) transition, in
/// the order the menu should surface them. Threshold crossings come first
/// (session before weekly, ascending pct within each window), then
/// ResetBacks (session before weekly). `WeeklyPace` is *not* considered here
/// — it lives on the pace-estimator path (`evaluate_pace`) because it needs
/// history beyond the last tick.
///
/// A field that's `None` on either side of the pair produces no trigger for
/// that window — first-boot after a capture shouldn't blast the user with
/// "you crossed 70" for percentages that were simply unknown before.
pub fn evaluate(
    prev: &Snapshot,
    curr: &Snapshot,
    cfg: &NotificationConfig,
) -> Vec<Trigger> {
    let mut out = Vec::new();
    let pairs = [
        (Window::Session, prev.session_pct, curr.session_pct),
        (Window::Weekly, prev.weekly_pct, curr.weekly_pct),
    ];
    if cfg.threshold_enabled {
        for (w, p, c) in pairs {
            let (Some(p), Some(c)) = (p, c) else { continue };
            for pct in THRESHOLDS {
                let t = pct as f32;
                if p < t && c >= t {
                    out.push(Trigger::Threshold { window: w, pct });
                }
            }
        }
    }
    if cfg.reset_back_enabled {
        for (w, p, c) in pairs {
            let (Some(p), Some(c)) = (p, c) else { continue };
            if is_window_reset(p, c) {
                out.push(Trigger::ResetBack { window: w });
            }
        }
    }
    out
}

/// True iff the (prev, curr) percentages for one window describe a reset:
/// utilization dropped from a non-trivial prior level. The 50-percent floor
/// keeps normal noise (small fluctuations near zero) from being mistaken for
/// a reset — a genuine reset lands a high percentage back at (near-)zero.
fn is_window_reset(prev_pct: f32, curr_pct: f32) -> bool {
    curr_pct < prev_pct && prev_pct > 50.0
}

/// True iff `curr` shows a reset for `w` relative to `prev`. Public so the
/// watch loop can query it independently of `evaluate` (e.g. to log a reset
/// even when `reset_back_enabled` is off).
pub fn is_reset(prev: &Snapshot, curr: &Snapshot, w: Window) -> bool {
    let (p, c) = match w {
        Window::Session => (prev.session_pct, curr.session_pct),
        Window::Weekly => (prev.weekly_pct, curr.weekly_pct),
    };
    matches!((p, c), (Some(p), Some(c)) if is_window_reset(p, c))
}

/// Filter `triggers` against `state`, updating `state` in place. A Threshold
/// already present in `state.crossings` is dropped; the rest are recorded so
/// the next tick doesn't refire them. A ResetBack for a window first clears
/// that window's crossings (so a subsequent 70/90 crossing re-fires as
/// intended), and — for a weekly reset — clears `pace_fired_this_window` too.
/// Reset detection uses the (prev, curr) pair directly (not just the
/// Trigger stream) so it fires even when `cfg.reset_back_enabled` is false.
pub fn dedup_and_apply(
    state: &mut NotifState,
    prev: &Snapshot,
    curr: &Snapshot,
    triggers: Vec<Trigger>,
) -> Vec<Trigger> {
    // Reset detection first so a Threshold trigger in the same tick as a
    // reset (rare in practice — resets zero-out the window) still fires.
    if is_reset(prev, curr, Window::Session) {
        state.crossings.retain(|(w, _)| *w != Window::Session);
    }
    if is_reset(prev, curr, Window::Weekly) {
        state.crossings.retain(|(w, _)| *w != Window::Weekly);
        state.pace_fired_this_window = false;
    }
    let mut kept = Vec::with_capacity(triggers.len());
    for t in triggers {
        match &t {
            Trigger::Threshold { window, pct } => {
                if state.crossings.contains(&(*window, *pct)) {
                    continue;
                }
                state.crossings.insert((*window, *pct));
                kept.push(t);
            }
            Trigger::ResetBack { .. } => kept.push(t),
            Trigger::WeeklyPace { .. } => {
                // Guard against a caller that queued a pace trigger without
                // going through `evaluate_pace` (which already dedups).
                if state.pace_fired_this_window {
                    continue;
                }
                state.pace_fired_this_window = true;
                kept.push(t);
            }
        }
    }
    kept
}

/// Weekly-pace evaluation. Fires at most once per weekly window (deduped by
/// `NotifState.pace_fired_this_window`). Requires `cfg.pace_enabled`, a
/// pace estimate with sane confidence and a strictly-positive slope, a known
/// current weekly percentage below 100, and a projected hit-time strictly
/// after `now` (a projection into the past means the fit is degenerate).
/// Returns the trigger to consider firing; the caller still runs it through
/// `dedup_and_apply` to actually flip the "fired" bit.
pub fn evaluate_pace(
    pace: Option<&PaceEstimate>,
    curr: &Snapshot,
    cfg: &NotificationConfig,
    now: DateTime<Utc>,
) -> Option<Trigger> {
    if !cfg.pace_enabled {
        return None;
    }
    let pace = pace?;
    if pace.confidence < 0.5 || pace.slope_pct_per_hour <= 0.0 {
        return None;
    }
    let cur_pct = curr.weekly_pct? as f64;
    if !(0.0..100.0).contains(&cur_pct) {
        return None;
    }
    let hours_to_100 = (100.0 - cur_pct) / pace.slope_pct_per_hour;
    if !hours_to_100.is_finite() || hours_to_100 <= 0.0 {
        return None;
    }
    // Millis (not seconds) so a fractional hour doesn't quantize to whole
    // seconds — the printed body ends up wrong by up to 59s otherwise.
    let millis = (hours_to_100 * 3_600_000.0) as i64;
    let hit = now.checked_add_signed(Duration::milliseconds(millis))?;
    Some(Trigger::WeeklyPace { hit_time: hit })
}

// ---------------------------------------------------------------------------
// Presentation + delivery
// ---------------------------------------------------------------------------

/// Golden-tested (summary, body) pair for a Trigger. Kept separate from `fire`
/// so tests can assert the exact wording without dragging in notify-rust.
pub fn format_message(trigger: &Trigger, account: &AccountKey) -> (String, String) {
    let summary = format!("{} · {}", account.provider, account.account);
    let body = match trigger {
        Trigger::Threshold { window, pct } => {
            format!("{} usage crossed {pct}%", window.label())
        }
        Trigger::ResetBack { window } => {
            format!("{} window reset — headroom restored", window.label())
        }
        Trigger::WeeklyPace { hit_time } => {
            format!(
                "Weekly on pace to hit 100% at {}",
                hit_time.format("%Y-%m-%d %H:%M UTC")
            )
        }
    };
    (summary, body)
}

/// Send a desktop notification for `trigger`. Best-effort from the caller's
/// perspective — the watch loop swallows the Err so a broken dbus connection
/// can't stall the poll cycle. Runs synchronously; notify-rust's own thread
/// model varies by backend and we don't need async here.
pub fn fire(trigger: &Trigger, account: &AccountKey) -> Result<()> {
    let (summary, body) = format_message(trigger, account);
    notify_rust::Notification::new()
        .summary(&summary)
        .body(&body)
        .show()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("notify-rust show failed: {e}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn snap(session: Option<f32>, weekly: Option<f32>) -> Snapshot {
        Snapshot {
            ts: Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
            provider: "claude".to_string(),
            account: "a@x.io".to_string(),
            session_pct: session,
            weekly_pct: weekly,
            active_model: None,
        }
    }

    // --- evaluate: no crossing produces nothing ------------------------------

    #[test]
    fn evaluate_no_crossing_returns_empty() {
        let prev = snap(Some(40.0), Some(30.0));
        let curr = snap(Some(45.0), Some(35.0));
        let cfg = NotificationConfig::default();
        assert!(evaluate(&prev, &curr, &cfg).is_empty());
    }

    // --- evaluate: a single upward crossing --------------------------------

    #[test]
    fn evaluate_upward_70_produces_one_threshold() {
        let prev = snap(Some(50.0), Some(30.0));
        let curr = snap(Some(75.0), Some(35.0));
        let cfg = NotificationConfig::default();
        let ts = evaluate(&prev, &curr, &cfg);
        assert_eq!(
            ts,
            vec![Trigger::Threshold {
                window: Window::Session,
                pct: 70,
            }]
        );
    }

    // --- evaluate: both 70 and 90 in one tick, ordered ---------------------

    #[test]
    fn evaluate_both_70_and_90_same_tick_are_ordered_70_then_90() {
        let prev = snap(Some(50.0), None);
        let curr = snap(Some(95.0), None);
        let cfg = NotificationConfig::default();
        let ts = evaluate(&prev, &curr, &cfg);
        assert_eq!(
            ts,
            vec![
                Trigger::Threshold {
                    window: Window::Session,
                    pct: 70
                },
                Trigger::Threshold {
                    window: Window::Session,
                    pct: 90
                },
            ]
        );
    }

    // --- evaluate: session and weekly both cross, session-first ------------

    #[test]
    fn evaluate_orders_session_before_weekly() {
        let prev = snap(Some(50.0), Some(50.0));
        let curr = snap(Some(75.0), Some(75.0));
        let cfg = NotificationConfig::default();
        let ts = evaluate(&prev, &curr, &cfg);
        assert_eq!(
            ts,
            vec![
                Trigger::Threshold {
                    window: Window::Session,
                    pct: 70
                },
                Trigger::Threshold {
                    window: Window::Weekly,
                    pct: 70
                },
            ]
        );
    }

    // --- evaluate: missing prev/curr percent for a window is a no-op --------

    #[test]
    fn evaluate_skips_window_when_either_side_is_none() {
        let prev = snap(None, Some(50.0));
        let curr = snap(Some(75.0), Some(75.0));
        let cfg = NotificationConfig::default();
        let ts = evaluate(&prev, &curr, &cfg);
        // Session had `None` on prev → no session trigger; weekly is fine.
        assert_eq!(
            ts,
            vec![Trigger::Threshold {
                window: Window::Weekly,
                pct: 70
            }]
        );
    }

    // --- evaluate: disable flags gate their triggers -----------------------

    #[test]
    fn evaluate_respects_threshold_disabled_flag() {
        let prev = snap(Some(50.0), None);
        let curr = snap(Some(75.0), None);
        let cfg = NotificationConfig {
            threshold_enabled: false,
            ..NotificationConfig::default()
        };
        assert!(evaluate(&prev, &curr, &cfg).is_empty());
    }

    #[test]
    fn evaluate_reset_back_produces_reset_trigger() {
        let prev = snap(Some(85.0), None);
        let curr = snap(Some(5.0), None);
        let cfg = NotificationConfig::default();
        let ts = evaluate(&prev, &curr, &cfg);
        assert_eq!(
            ts,
            vec![Trigger::ResetBack {
                window: Window::Session,
            }]
        );
    }

    #[test]
    fn evaluate_small_drop_is_not_a_reset() {
        // Drop is real but prev was <= 50; that's noise, not a window reset.
        let prev = snap(Some(45.0), None);
        let curr = snap(Some(5.0), None);
        let cfg = NotificationConfig::default();
        assert!(evaluate(&prev, &curr, &cfg).is_empty());
    }

    // --- dedup: same crossing fires only once per window --------------------

    #[test]
    fn dedup_prevents_a_threshold_from_firing_twice() {
        let mut state = NotifState::default();
        let prev = snap(Some(50.0), None);
        let curr = snap(Some(75.0), None);
        let cfg = NotificationConfig::default();

        let first = dedup_and_apply(
            &mut state,
            &prev,
            &curr,
            evaluate(&prev, &curr, &cfg),
        );
        assert_eq!(first.len(), 1, "first crossing must fire");

        // A follow-up tick that stays above 70 without crossing again — the
        // pair itself produces no new Threshold from `evaluate`, and even if
        // it did, `state.crossings` would suppress it.
        let curr2 = snap(Some(80.0), None);
        let second = dedup_and_apply(
            &mut state,
            &curr,
            &curr2,
            evaluate(&curr, &curr2, &cfg),
        );
        assert!(second.is_empty(), "same threshold must not re-fire");
        assert!(state.crossings.contains(&(Window::Session, 70)));
    }

    // --- reset: crossings for the reset window are cleared, then re-arm ----

    #[test]
    fn reset_clears_crossings_and_lets_threshold_refire() {
        let mut state = NotifState::default();
        let cfg = NotificationConfig::default();
        // 1st tick: cross 70.
        let prev = snap(Some(50.0), None);
        let curr = snap(Some(75.0), None);
        let _ = dedup_and_apply(
            &mut state,
            &prev,
            &curr,
            evaluate(&prev, &curr, &cfg),
        );
        assert!(state.crossings.contains(&(Window::Session, 70)));

        // 2nd tick: session resets (drop from 75 to 5) → crossings clear +
        // a ResetBack fires.
        let reset = snap(Some(5.0), None);
        let fired = dedup_and_apply(
            &mut state,
            &curr,
            &reset,
            evaluate(&curr, &reset, &cfg),
        );
        assert_eq!(
            fired,
            vec![Trigger::ResetBack {
                window: Window::Session,
            }]
        );
        assert!(state.crossings.is_empty(), "reset clears session crossings");

        // 3rd tick: cross 70 again — must fire again because we re-armed.
        let climb_lo = snap(Some(40.0), None);
        let climb_hi = snap(Some(75.0), None);
        let fired = dedup_and_apply(
            &mut state,
            &climb_lo,
            &climb_hi,
            evaluate(&climb_lo, &climb_hi, &cfg),
        );
        assert_eq!(
            fired,
            vec![Trigger::Threshold {
                window: Window::Session,
                pct: 70
            }]
        );
    }

    // --- weekly reset clears both crossings and pace_fired -----------------

    #[test]
    fn weekly_reset_clears_pace_fired_this_window() {
        let mut state = NotifState {
            crossings: [(Window::Weekly, 70), (Window::Weekly, 90)].into_iter().collect(),
            pace_fired_this_window: true,
        };
        // A weekly reset happens; `dedup_and_apply` must clear both.
        let prev = snap(None, Some(85.0));
        let curr = snap(None, Some(3.0));
        let cfg = NotificationConfig::default();
        let _ = dedup_and_apply(
            &mut state,
            &prev,
            &curr,
            evaluate(&prev, &curr, &cfg),
        );
        assert!(state.crossings.is_empty());
        assert!(!state.pace_fired_this_window);
    }

    // --- session reset does NOT clear pace_fired (pace is weekly only) -----

    #[test]
    fn session_reset_leaves_pace_fired_untouched() {
        let mut state = NotifState {
            crossings: BTreeSet::new(),
            pace_fired_this_window: true,
        };
        let prev = snap(Some(85.0), None);
        let curr = snap(Some(3.0), None);
        let cfg = NotificationConfig::default();
        let _ = dedup_and_apply(
            &mut state,
            &prev,
            &curr,
            evaluate(&prev, &curr, &cfg),
        );
        assert!(
            state.pace_fired_this_window,
            "session reset must not touch weekly pace state"
        );
    }

    // --- WeeklyPace: fires only once per window -----------------------------

    #[test]
    fn weekly_pace_fires_once_per_window_and_rearms_on_reset() {
        let cfg = NotificationConfig {
            pace_enabled: true,
            ..NotificationConfig::default()
        };
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        let pace = PaceEstimate {
            slope_pct_per_hour: 2.0,
            confidence: 0.9,
            sample_count: 12,
        };
        let curr = snap(None, Some(50.0));

        // 1st tick: emits a pace trigger; dedup lets it through.
        let t = evaluate_pace(Some(&pace), &curr, &cfg, now).expect("pace should fire");
        let mut state = NotifState::default();
        let prev = snap(None, Some(48.0));
        let kept =
            dedup_and_apply(&mut state, &prev, &curr, vec![t]);
        assert_eq!(kept.len(), 1);
        assert!(state.pace_fired_this_window);

        // 2nd tick: still on pace, same window — must not re-fire.
        let curr2 = snap(None, Some(55.0));
        let t = evaluate_pace(Some(&pace), &curr2, &cfg, now).expect("still on pace");
        let kept = dedup_and_apply(&mut state, &curr, &curr2, vec![t]);
        assert!(kept.is_empty(), "pace must fire at most once per window");

        // 3rd tick: weekly resets — pace_fired clears; pace fires again.
        let reset = snap(None, Some(3.0));
        let _ = dedup_and_apply(
            &mut state,
            &curr2,
            &reset,
            evaluate(&curr2, &reset, &cfg),
        );
        assert!(!state.pace_fired_this_window);
        let t = evaluate_pace(Some(&pace), &reset, &cfg, now).expect("pace re-arms after reset");
        let kept = dedup_and_apply(&mut state, &reset, &reset, vec![t]);
        assert_eq!(kept.len(), 1, "pace should re-fire in the fresh window");
    }

    #[test]
    fn evaluate_pace_returns_none_when_disabled() {
        let cfg = NotificationConfig {
            pace_enabled: false,
            ..NotificationConfig::default()
        };
        let pace = PaceEstimate {
            slope_pct_per_hour: 5.0,
            confidence: 0.99,
            sample_count: 10,
        };
        let curr = snap(None, Some(50.0));
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        assert!(evaluate_pace(Some(&pace), &curr, &cfg, now).is_none());
    }

    #[test]
    fn evaluate_pace_returns_none_for_low_confidence_or_flat_slope() {
        let cfg = NotificationConfig {
            pace_enabled: true,
            ..NotificationConfig::default()
        };
        let curr = snap(None, Some(50.0));
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let low_conf = PaceEstimate {
            slope_pct_per_hour: 2.0,
            confidence: 0.1,
            sample_count: 10,
        };
        assert!(evaluate_pace(Some(&low_conf), &curr, &cfg, now).is_none());
        let flat = PaceEstimate {
            slope_pct_per_hour: 0.0,
            confidence: 0.9,
            sample_count: 10,
        };
        assert!(evaluate_pace(Some(&flat), &curr, &cfg, now).is_none());
    }

    // --- Golden: formatted messages are intentional -------------------------

    fn ak() -> AccountKey {
        AccountKey::new("claude", "dev@example.com")
    }

    #[test]
    fn golden_threshold_message() {
        let (s, b) = format_message(
            &Trigger::Threshold {
                window: Window::Session,
                pct: 70,
            },
            &ak(),
        );
        assert_eq!(s, "claude · dev@example.com");
        assert_eq!(b, "Session (5h) usage crossed 70%");
    }

    #[test]
    fn golden_reset_back_message() {
        let (s, b) = format_message(
            &Trigger::ResetBack {
                window: Window::Weekly,
            },
            &ak(),
        );
        assert_eq!(s, "claude · dev@example.com");
        assert_eq!(b, "Weekly (7d) window reset — headroom restored");
    }

    #[test]
    fn golden_weekly_pace_message() {
        let hit = Utc.with_ymd_and_hms(2026, 9, 5, 14, 30, 0).unwrap();
        let (s, b) = format_message(&Trigger::WeeklyPace { hit_time: hit }, &ak());
        assert_eq!(s, "claude · dev@example.com");
        assert_eq!(b, "Weekly on pace to hit 100% at 2026-09-05 14:30 UTC");
    }

    // --- NotifState round-trips through serde -------------------------------

    #[test]
    fn notif_state_serde_roundtrips() {
        let s = NotifState {
            crossings: [(Window::Session, 70), (Window::Weekly, 90)]
                .into_iter()
                .collect(),
            pace_fired_this_window: true,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: NotifState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        // The Window enum serializes as kebab-case, so a persisted state.json
        // reads well by eye.
        assert!(json.contains("session"), "{json}");
        assert!(json.contains("weekly"), "{json}");
    }
}
