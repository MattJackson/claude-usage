use super::*;
use chrono::{Duration, Utc};

// --- bar ---

#[test]
fn bar_none_is_dash() {
    assert_eq!(bar(None), "-");
}

#[test]
fn bar_zero_percent() {
    assert_eq!(bar(Some(0.0)), "[----------]   0%");
}

#[test]
fn bar_fifty_percent() {
    assert_eq!(bar(Some(50.0)), "[#####-----]  50%");
}

#[test]
fn bar_hundred_percent() {
    assert_eq!(bar(Some(100.0)), "[##########] 100%");
}

#[test]
fn bar_clamps_over_hundred() {
    assert_eq!(bar(Some(250.0)), "[##########] 100%");
}

#[test]
fn bar_clamps_negative() {
    assert_eq!(bar(Some(-5.0)), "[----------]   0%");
}

// --- truncate ---

#[test]
fn truncate_short_string_unchanged() {
    assert_eq!(truncate("hello", 10), "hello");
}

#[test]
fn truncate_exact_length_unchanged() {
    assert_eq!(truncate("hello", 5), "hello");
}

#[test]
fn truncate_long_string_adds_ellipsis() {
    let out = truncate("abcdefghij", 5);
    assert_eq!(out.chars().count(), 5);
    assert!(out.ends_with('…'));
    assert!(out.starts_with("abcd"));
}

// --- humanize_until ---

#[test]
fn humanize_until_past_is_now() {
    assert_eq!(humanize_until(Utc::now() - Duration::hours(1)), "now");
}

#[test]
fn humanize_until_hours_and_minutes() {
    // 2h30m out (+ a few seconds of slack): shows hours and minutes.
    let s = humanize_until(Utc::now() + Duration::minutes(150) + Duration::seconds(5));
    assert_eq!(s, "2h 30m");
}

#[test]
fn humanize_until_days_and_hours() {
    let s = humanize_until(Utc::now() + Duration::hours(50) + Duration::seconds(5));
    assert_eq!(s, "2d 2h");
}

#[test]
fn humanize_until_minutes_only() {
    let s = humanize_until(Utc::now() + Duration::minutes(30) + Duration::seconds(5));
    assert_eq!(s, "30m");
}

// --- Row helpers ---

fn cell(pct: Option<f64>) -> Cell {
    Cell {
        pct,
        resets_at: None,
    }
}

fn row(session: Option<f64>, weekly: Option<f64>) -> Row {
    Row {
        email: "x@e.com".to_string(),
        session: cell(session),
        weekly: cell(weekly),
        opus: None,
        error: None,
        fetched_at: Some(0),
    }
}

/// A row with an email and a weekly reset time, for pick/order tests.
fn row_full(email: &str, session: f64, weekly: f64, weekly_reset: DateTime<Utc>) -> Row {
    Row {
        email: email.to_string(),
        session: Cell {
            pct: Some(session),
            resets_at: None,
        },
        weekly: Cell {
            pct: Some(weekly),
            resets_at: Some(weekly_reset),
        },
        opus: None,
        error: None,
        fetched_at: Some(Utc::now().timestamp()),
    }
}

#[test]
fn row_available_when_both_have_headroom() {
    assert!(row(Some(50.0), Some(80.0)).available());
}

#[test]
fn row_unavailable_when_session_maxed() {
    assert!(!row(Some(100.0), Some(10.0)).available());
}

#[test]
fn row_unavailable_when_weekly_maxed() {
    assert!(!row(Some(10.0), Some(100.0)).available());
}

#[test]
fn row_available_when_pct_unknown() {
    assert!(row(None, None).available());
}

#[test]
fn row_max_pct_takes_tightest() {
    assert_eq!(row(Some(30.0), Some(70.0)).max_pct(), 70.0);
    assert_eq!(row(Some(90.0), Some(20.0)).max_pct(), 90.0);
}

#[test]
fn row_headroom_is_complement_of_max() {
    assert_eq!(row(Some(30.0), Some(70.0)).headroom(), 30.0);
}

#[test]
fn row_has_data_tracks_fetched_at() {
    assert!(row(Some(10.0), Some(20.0)).has_data());
    let mut r = row(Some(10.0), Some(20.0));
    r.fetched_at = None;
    assert!(!r.has_data());
}

// --- age_str ---

#[test]
fn age_str_never_without_timestamp() {
    assert_eq!(age_str(None), "never");
}

#[test]
fn age_str_minutes_and_hours() {
    let now = Utc::now().timestamp();
    assert_eq!(age_str(Some(now - 120)), "2m ago");
    assert_eq!(age_str(Some(now - 7200)), "2h ago");
}

// --- cached_from_usage / row_from_account ---

#[test]
fn cached_from_usage_extracts_windows() {
    let u: usage::Usage = serde_json::from_str(
        r#"{"five_hour":{"utilization":9.0,"resets_at":"2026-09-05T08:00:00Z"},
            "seven_day":{"utilization":61.0,"resets_at":"2026-09-09T00:00:00Z"},
            "seven_day_opus":null}"#,
    )
    .unwrap();
    let c = cached_from_usage(&u);
    assert_eq!(c.session_pct, Some(9.0));
    assert_eq!(c.weekly_pct, Some(61.0));
    assert_eq!(c.session_reset.as_deref(), Some("2026-09-05T08:00:00Z"));
    assert!(c.opus_pct.is_none());
    assert!(c.fetched_at > 0);
}

#[test]
fn row_from_account_without_cache_has_no_data() {
    let a = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":0}}"#,
    )
    .unwrap();
    assert!(!row_from_account(&a).has_data());
}

// --- candidate_order / auto_pick tie-break (D1) ---

#[test]
fn candidate_order_prefers_more_headroom_on_equal_reset() {
    let reset = Utc::now() + Duration::hours(24);
    // a: 80% used (20% headroom); b: 10% used (90% headroom). Same reset.
    let a = row_full("a@e.com", 80.0, 80.0, reset);
    let b = row_full("b@e.com", 10.0, 10.0, reset);
    // b (more headroom) must sort BEFORE a.
    assert_eq!(candidate_order(&a, &b), std::cmp::Ordering::Greater);
    assert_eq!(candidate_order(&b, &a), std::cmp::Ordering::Less);
}

#[test]
fn auto_pick_tie_break_picks_higher_headroom() {
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("high@e.com", 80.0, 80.0, reset),
        row_full("low@e.com", 10.0, 10.0, reset),
    ];
    // Equal soonest reset → the account with MORE headroom (lower usage) wins.
    assert_eq!(auto_pick(&rows).unwrap(), "low@e.com");
}

#[test]
fn auto_pick_prefers_soonest_reset() {
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    // The soonest-resetting account wins even with slightly less headroom.
    let rows = vec![
        row_full("later@e.com", 5.0, 5.0, later),
        row_full("soon@e.com", 40.0, 40.0, soon),
    ];
    assert_eq!(auto_pick(&rows).unwrap(), "soon@e.com");
}

// --- choose_swap_target (auto-swap guard) ---

#[test]
fn choose_swap_target_moves_off_over_trigger_account() {
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset),
    ];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("free@e.com"));
}

#[test]
fn choose_swap_target_none_when_active_below_trigger() {
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 40.0, 40.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset),
    ];
    let guard = SwapGuard::default();
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_respects_cooldown() {
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset),
    ];
    let guard = SwapGuard {
        last_swap: Some(std::time::Instant::now()),
        ..SwapGuard::default()
    };
    // Just swapped → cooldown blocks another swap.
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_skips_ceiling_and_maxed_targets() {
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("alsohigh@e.com", 90.0, 90.0, reset), // over the 85% ceiling
    ];
    let guard = SwapGuard::default();
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}
