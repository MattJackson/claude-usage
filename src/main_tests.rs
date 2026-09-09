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
        provider_id: CLAUDE_SLUG.to_string(),
        needs_relogin: false,
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
    row_full_with_provider(CLAUDE_SLUG, email, session, weekly, weekly_reset)
}

/// Like `row_full`, but with an explicit provider slug — for tests that need
/// to verify the swap-capability gate on non-Claude / unregistered slugs.
fn row_full_with_provider(
    provider_id: &str,
    email: &str,
    session: f64,
    weekly: f64,
    weekly_reset: DateTime<Utc>,
) -> Row {
    Row {
        provider_id: provider_id.to_string(),
        needs_relogin: false,
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

// --- menu_order (dropdown priority) ---

#[test]
fn menu_order_lists_use_first_account_first() {
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let mut rows = [
        row_full("later@e.com", 5.0, 5.0, later),
        row_full("soon@e.com", 40.0, 40.0, soon),
    ];
    rows.sort_by(menu_order);
    // Soonest weekly reset (the account auto-pick would use first) leads.
    assert_eq!(rows[0].email, "soon@e.com");
    assert_eq!(rows[1].email, "later@e.com");
}

#[test]
fn menu_order_sinks_maxed_accounts_below_usable_ones() {
    let reset = Utc::now() + Duration::hours(6);
    // A maxed account resets soonest, but it's unusable — it must sort last.
    let mut rows = [
        row_full("maxed@e.com", 100.0, 40.0, reset),
        row_full("free@e.com", 30.0, 30.0, Utc::now() + Duration::hours(24)),
    ];
    rows.sort_by(menu_order);
    assert_eq!(rows[0].email, "free@e.com");
    assert_eq!(rows[1].email, "maxed@e.com");
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
fn choose_swap_target_stays_below_trigger_when_nothing_better() {
    // Active is healthy AND already resets soonest, so no candidate is a better
    // place to be — the proactive path must not swap sideways.
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let rows = vec![
        row_full("active@e.com", 40.0, 40.0, soon),
        row_full("free@e.com", 20.0, 20.0, later),
    ];
    let guard = SwapGuard::default();
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_proactively_flips_back_to_sooner_reset() {
    // Active is healthy (below trigger) but a freed-up account resets its weekly
    // window sooner — use-it-or-lose-it says flip back to it.
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let rows = vec![
        row_full("active@e.com", 40.0, 40.0, later),
        row_full("fresh@e.com", 10.0, 10.0, soon),
    ];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("fresh@e.com"));
}

#[test]
fn choose_swap_target_no_proactive_swap_within_headroom_margin() {
    // Equal weekly reset and only a small headroom lead (< PROACTIVE_HEADROOM_MARGIN)
    // must not trigger a proactive swap, or two near-equal accounts would ping-pong.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 30.0, 30.0, reset),
        row_full("free@e.com", 25.0, 25.0, reset), // 5-point lead, under the margin
    ];
    let guard = SwapGuard::default();
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_proactive_swap_beyond_headroom_margin() {
    // Equal weekly reset but a large headroom lead (>= margin) is worth the swap.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 70.0, 70.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset), // 50-point lead
    ];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("free@e.com"));
}

#[test]
fn choose_swap_target_proactive_respects_no_return_window() {
    // Even when a sooner-resetting account would be a better place to be, the
    // no-return window still excludes an account we just left.
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let rows = vec![
        row_full("active@e.com", 40.0, 40.0, later),
        row_full("fresh@e.com", 10.0, 10.0, soon),
    ];
    let mut left_at = std::collections::HashMap::new();
    left_at.insert("fresh@e.com".to_string(), std::time::Instant::now());
    let guard = SwapGuard {
        left_at,
        ..SwapGuard::default()
    };
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
fn choose_swap_target_skips_target_whose_provider_is_unregistered() {
    // A row tagged with a provider slug that isn't in the registry (a stub /
    // reporting-only agent that phase 4+ hasn't wired up yet) must NOT be
    // selected as a swap target, even if its usage looks great.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full_with_provider("no-such-provider", "free@e.com", 5.0, 5.0, reset),
    ];
    let guard = SwapGuard::default();
    // Only candidate was filtered by the capability gate → no swap.
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_still_picks_claude_target_after_capability_filter() {
    // Regression guard for the capability filter: adding it must not have
    // stopped Claude accounts (the only registered v1 provider) from being
    // chosen. Baseline swap decision unchanged.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset),
    ];
    let guard = SwapGuard::default();
    assert_eq!(
        choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).as_deref(),
        Some("free@e.com")
    );
}

#[test]
fn provider_supports_swap_reflects_registered_claude() {
    // Claude is registered, has both usage and switching → swappable.
    assert!(provider_supports_swap(CLAUDE_SLUG));
    // Unknown slugs are treated as non-candidates (safest default).
    assert!(!provider_supports_swap("nope"));
}

#[test]
fn row_from_account_tags_provider_id_claude() {
    // Every v1-migrated row must carry the "claude" slug so the swap gate
    // recognizes it. Phase 3 (state v2) replaces this with a bucket lookup.
    let a = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":0}}"#,
    )
    .unwrap();
    assert_eq!(row_from_account(&a).provider_id, CLAUDE_SLUG);
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

#[test]
fn choose_swap_target_excludes_recently_left_account() {
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset),
    ];
    // We just left free@e.com — the no-return window must exclude it, so with no
    // other candidate there's nothing to swap to.
    let mut left_at = std::collections::HashMap::new();
    left_at.insert("free@e.com".to_string(), std::time::Instant::now());
    let guard = SwapGuard {
        left_at,
        ..SwapGuard::default()
    };
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_returns_to_high_weekly_fresh_session_account() {
    // The account we want to finish draining: weekly is high (88%, above the old
    // 85% max_pct ceiling) but its session just reset, so it's a valid target and
    // we should flip back to keep spending its weekly before it resets.
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let rows = vec![
        row_full("active@e.com", 40.0, 40.0, later),
        row_full("draining@e.com", 5.0, 88.0, soon),
    ];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("draining@e.com"));
}

#[test]
fn choose_swap_target_skips_target_whose_weekly_hit_trigger() {
    // Fresh session but weekly already at the trigger → landing there would
    // immediately want to swap away again, so it's not a valid target.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 40.0, reset),
        row_full("spent@e.com", 5.0, 96.0, reset),
    ];
    let guard = SwapGuard::default();
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

// --- env-override guard (CLAUDE_CODE_OAUTH_TOKEN) ---

#[test]
fn choose_swap_target_skips_env_overridden_provider_end_to_end() {
    // Active is over the trigger and there IS a healthy candidate — without
    // an env-override, we'd swap to it. With `CLAUDE_CODE_OAUTH_TOKEN`
    // active on the Claude provider, `claude` ignores whatever token we
    // write, so any swap is a silent no-op and `watch_cycle` must skip it.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset),
    ];
    let guard = SwapGuard::default();

    // Baseline sanity: with no override, the swap fires.
    assert_eq!(
        choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).as_deref(),
        Some("free@e.com"),
        "baseline: without env-override the auto-swap picks free@e.com",
    );

    // With the override active for `claude`, both the active row's provider
    // and every candidate row's provider are gated off → no swap.
    with_env_override_hook(&[CLAUDE_SLUG], || {
        assert!(
            choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none(),
            "env-override active: watch_cycle must NOT swap claude accounts",
        );
    });
}

#[test]
fn env_override_active_reads_shared_slug_map() {
    // The hook drives both the menu (via menubar::env_override_for) and the
    // swap filter — one source of truth for the whole app.
    assert!(!env_override_active(CLAUDE_SLUG));
    with_env_override_hook(&[CLAUDE_SLUG], || {
        assert!(env_override_active(CLAUDE_SLUG));
        // Unknown / non-Claude slugs are unaffected by the Claude env var.
        assert!(!env_override_active("codex"));
    });
    // Restored on exit.
    assert!(!env_override_active(CLAUDE_SLUG));
}

// --- next_interval (backoff + adaptive cadence) ---

const TRIGGER_FOR_TESTS: f64 = 95.0;

#[test]
fn next_interval_resets_to_base_when_not_limited_and_far_from_trigger() {
    // A clean cycle with everyone comfortably below the warning band
    // returns to the base cadence, even from a backed-off value.
    assert_eq!(
        next_interval(600, 60, false, Some(50.0), TRIGGER_FOR_TESTS),
        60
    );
    assert_eq!(
        next_interval(60, 60, false, Some(50.0), TRIGGER_FOR_TESTS),
        60
    );
    // No usage data yet → also base cadence.
    assert_eq!(next_interval(60, 60, false, None, TRIGGER_FOR_TESTS), 60);
}

#[test]
fn next_interval_doubles_on_rate_limit_capped() {
    // Rate-limit override wins over adaptive cadence.
    assert_eq!(
        next_interval(60, 60, true, Some(50.0), TRIGGER_FOR_TESTS),
        120
    );
    // Never below base even if `current` was stale-small.
    assert_eq!(
        next_interval(1, 60, true, Some(50.0), TRIGGER_FOR_TESTS),
        120
    );
    // Capped at the max.
    assert_eq!(
        next_interval(
            WATCH_MAX_INTERVAL_SECS,
            60,
            true,
            Some(99.0),
            TRIGGER_FOR_TESTS
        ),
        WATCH_MAX_INTERVAL_SECS
    );
}

#[test]
fn next_interval_tightens_to_warning_inside_the_band() {
    // Inside [trigger - 15, trigger): 30s WARNING cadence, regardless of
    // the current `current` — this is exactly the case that let the user's
    // 94% + 150s wait miss the swap on v0.4.3.
    assert_eq!(
        next_interval(150, 150, false, Some(94.9), TRIGGER_FOR_TESTS),
        30
    );
    assert_eq!(
        next_interval(150, 150, false, Some(85.0), TRIGGER_FOR_TESTS),
        30
    );
    assert_eq!(
        next_interval(150, 150, false, Some(80.001), TRIGGER_FOR_TESTS),
        30
    );
    // Exactly at the band-lower edge (trigger - 15 = 80.0) still tightens.
    assert_eq!(
        next_interval(150, 150, false, Some(80.0), TRIGGER_FOR_TESTS),
        30
    );
    // 1 tick below the band — back to base.
    assert_eq!(
        next_interval(150, 150, false, Some(79.9), TRIGGER_FOR_TESTS),
        150
    );
}

#[test]
fn next_interval_tightens_to_backstop_at_or_above_trigger() {
    // At or above the trigger: 10s BACKSTOP cadence. The auto-swap should
    // already have fired; this makes sure a transient failure doesn't
    // leave us blind for a full base cycle.
    assert_eq!(
        next_interval(150, 150, false, Some(95.0), TRIGGER_FOR_TESTS),
        10
    );
    assert_eq!(
        next_interval(150, 150, false, Some(99.9), TRIGGER_FOR_TESTS),
        10
    );
    assert_eq!(
        next_interval(150, 150, false, Some(100.0), TRIGGER_FOR_TESTS),
        10
    );
}

// --- peak_max_pct (v0.5.2 item 3: weekly-aware adaptive cadence) ---

#[test]
fn peak_max_pct_folds_weekly_not_just_session() {
    // Regression case from the addendum: session=0%, weekly=99%, trigger=95%
    // must fold to Some(99.0) — a weekly-only approach to the trigger used to
    // be invisible to the old session-only fold (`max_session_pct`), leaving
    // the daemon at BASE cadence right as an account was about to lock.
    let rows = vec![row(Some(0.0), Some(99.0))];
    assert_eq!(peak_max_pct(&rows), Some(99.0));
    assert_eq!(
        next_interval(150, 150, false, peak_max_pct(&rows), TRIGGER_FOR_TESTS),
        10,
        "session=0/weekly=99 at trigger=95 must hit BACKSTOP cadence"
    );
}

#[test]
fn peak_max_pct_ignores_rows_without_data() {
    let mut no_data = row(Some(80.0), Some(80.0));
    no_data.fetched_at = None;
    assert_eq!(peak_max_pct(&[no_data]), None);
}

#[test]
fn peak_max_pct_takes_the_max_across_accounts() {
    let rows = vec![row(Some(10.0), Some(20.0)), row(Some(50.0), Some(96.0))];
    assert_eq!(peak_max_pct(&rows), Some(96.0));
}

// --- cap_sleep_to_reset_boundary (v0.5.2 item 4: wake right after a reset) ---

#[test]
fn cap_sleep_to_reset_boundary_wakes_early_for_an_imminent_reset() {
    let now = Utc::now();
    let mut r = row(Some(50.0), Some(60.0));
    r.session.resets_at = Some(now + Duration::seconds(10));
    // Planned cadence is 150s, but the session resets in 10s — should cap to
    // 10s + the 2s buffer = 12s.
    assert_eq!(cap_sleep_to_reset_boundary(&[r], now, 150), 12);
}

#[test]
fn cap_sleep_to_reset_boundary_leaves_planned_alone_when_nothing_imminent() {
    let now = Utc::now();
    let mut r = row(Some(50.0), Some(60.0));
    r.weekly.resets_at = Some(now + Duration::days(3));
    assert_eq!(cap_sleep_to_reset_boundary(&[r], now, 150), 150);
}

#[test]
fn cap_sleep_to_reset_boundary_never_exceeds_planned() {
    // A reset 40s out is outside the 30s horizon — planned wins.
    let now = Utc::now();
    let mut r = row(Some(50.0), Some(60.0));
    r.session.resets_at = Some(now + Duration::seconds(40));
    assert_eq!(cap_sleep_to_reset_boundary(&[r], now, 150), 150);
}

// --- identity_matches (keychain adoption gate) ---

#[test]
fn identity_matches_by_uuid_when_both_present() {
    assert!(identity_matches(
        Some("u1"),
        Some("a@e.com"),
        Some("u1"),
        Some("b@e.com")
    ));
    // UUID mismatch wins over an email match — a different account is logged in.
    assert!(!identity_matches(
        Some("u1"),
        Some("a@e.com"),
        Some("u2"),
        Some("a@e.com")
    ));
}

#[test]
fn identity_matches_falls_back_to_email_case_insensitive() {
    assert!(identity_matches(
        None,
        Some("A@E.com"),
        None,
        Some("a@e.com")
    ));
    assert!(!identity_matches(
        None,
        Some("a@e.com"),
        None,
        Some("other@e.com")
    ));
}

#[test]
fn identity_matches_adopts_when_account_has_no_identity() {
    // No known identity yet → adopt (self-heal).
    assert!(identity_matches(None, None, None, Some("a@e.com")));
    // But a known email with nothing to compare against → stay safe, skip.
    assert!(!identity_matches(None, Some("a@e.com"), None, None));
}

// --- write_bytes_atomic_mode (rollback mode preservation) ---

#[cfg(unix)]
#[test]
fn write_bytes_atomic_mode_applies_requested_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    // Use a mode DISTINCT from write_private's hardcoded 0o600 default, so this
    // proves the `mode` argument controls the final mode (not write_private's
    // default) — deleting the set_permissions call would make this fail.
    let path = dir.path().join("claude.json");
    write_bytes_atomic_mode(&path, b"{\"x\":1}", 0o640).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"x\":1}");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o640,
        "the requested mode must be applied to the final file"
    );
    // No temp file left behind.
    assert!(!path.with_extension("json.usagio.tmp").exists());
}

// --- consumption_deltas (report same-account guard) ---

fn sample(account: &str, session: f64, ts: i64) -> Sample {
    Sample {
        ts,
        account: Some(account.to_string()),
        active: Some(true),
        session: Some(session),
        weekly: None,
        event: None,
    }
}

#[test]
fn consumption_deltas_only_counts_same_account_increases() {
    let a1 = sample("a@e.com", 10.0, 100);
    let a2 = sample("a@e.com", 40.0, 200); // +30, same account -> counted
    let b1 = sample("b@e.com", 5.0, 300); // a->b switch -> NOT counted
    let b2 = sample("b@e.com", 20.0, 400); // +15, same account -> counted
    let a3 = sample("a@e.com", 45.0, 500); // b->a switch -> NOT counted
    let list: Vec<&Sample> = vec![&a1, &a2, &b1, &b2, &a3];
    assert_eq!(consumption_deltas(&list), vec![(200, 30.0), (400, 15.0)]);
}

#[test]
fn consumption_deltas_ignores_negative_deltas() {
    // A reset (session drops) is not consumption.
    let a1 = sample("a@e.com", 90.0, 100);
    let a2 = sample("a@e.com", 5.0, 200); // reset, negative -> skipped
    let list: Vec<&Sample> = vec![&a1, &a2];
    assert!(consumption_deltas(&list).is_empty());
}

// --- merged_cached_usage (re-capture preserves usage) ---

fn acct_with_cache(cache: Option<CachedUsage>) -> Account {
    let mut a = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":0}}"#,
    )
    .unwrap();
    a.cached_usage = cache;
    a
}

#[test]
fn merged_cached_usage_prefers_existing_snapshot() {
    let existing = acct_with_cache(Some(CachedUsage {
        session_pct: Some(42.0),
        weekly_pct: Some(61.0),
        session_reset: None,
        weekly_reset: None,
        opus_pct: None,
        opus_reset: None,
        fetched_at: 123,
    }));
    let merged = merged_cached_usage(Some(&existing));
    assert_eq!(merged.unwrap().session_pct, Some(42.0));
}

#[test]
fn merged_cached_usage_none_for_new_account() {
    assert!(merged_cached_usage(None).is_none());
}

// --- rotate_if_large (shared log rotation) ---

#[test]
fn rotate_if_large_rotates_over_threshold_with_correct_name() {
    let dir = tempfile::tempdir().unwrap();
    for (name, rotated) in [
        ("usagio.log", "usagio.log.1"),
        ("history.jsonl", "history.jsonl.1"),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, b"0123456789").unwrap();
        logging::rotate_if_large(&path, 5); // 10 bytes > 5 -> rotate
        assert!(!path.exists(), "{name} should have been moved aside");
        assert!(
            dir.path().join(rotated).exists(),
            "expected rotated file {rotated}"
        );
    }
}

#[test]
fn rotate_if_large_leaves_small_file_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usagio.log");
    std::fs::write(&path, b"tiny").unwrap();
    logging::rotate_if_large(&path, 1_000_000);
    assert!(path.exists());
    assert!(!dir.path().join("usagio.log.1").exists());
}

// --- needs_relogin filtering in the swap picker ---

/// Construct a row with the needs_relogin flag set. Everything else mirrors
/// `row_full` so we can build side-by-side "both would otherwise win" tests.
fn row_full_flagged(email: &str, session: f64, weekly: f64, weekly_reset: DateTime<Utc>) -> Row {
    let mut r = row_full(email, session, weekly, weekly_reset);
    r.needs_relogin = true;
    r
}

#[test]
fn choose_swap_target_skips_needs_relogin_candidate() {
    // The would-be candidate has the flag: even though it's healthier and
    // would win auto-pick, choose_swap_target must not return it. With only
    // one candidate and it's flagged, we get back None.
    let reset = Utc::now() + chrono::Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full_flagged("dead@e.com", 10.0, 10.0, reset),
    ];
    let guard = SwapGuard::default();
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_prefers_unflagged_candidate_over_flagged_winner() {
    // The flagged candidate would win by auto-pick priority (soonest reset,
    // more headroom), but must be filtered out; the unflagged runner-up
    // wins instead.
    let soon = Utc::now() + chrono::Duration::hours(2);
    let later = Utc::now() + chrono::Duration::hours(48);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, later),
        row_full_flagged("dead@e.com", 10.0, 10.0, soon),
        row_full("ok@e.com", 20.0, 20.0, later),
    ];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("ok@e.com"));
}

#[test]
fn row_from_account_propagates_needs_relogin_flag() {
    let blob = r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":0}}"#;
    let mut a = Account::from_keychain_blob(blob).unwrap();
    a.email = Some("x@e.com".into());
    assert!(!row_from_account(&a).needs_relogin);
    a.needs_relogin = true;
    assert!(row_from_account(&a).needs_relogin);
}

// -----------------------------------------------------------------------------
// switch_to_guarded lock-closure invariant (H1 round-1 + H_R2_1 round-2)
//
// switch_to_guarded's contract is: any in-memory mutation to the state
// snapshot that happens BEFORE `st = State::load()?;` (the reload that
// absorbs `absorb_before_switch`'s disk writes) is DISCARDED and must not be
// relied on. Mutations AFTER the reload survive `st.save()`.
//
// This pins that contract at the state-lock/save layer without dragging in
// the keychain + apply_account plumbing. Red-before-green: swap the order
// (mutate after reload → mutate before reload) and this test fails.
// -----------------------------------------------------------------------------
#[test]
fn switch_lock_closure_invariant_mutations_after_reload_survive() {
    use crate::credentials::with_state_lock;
    use crate::store::{Account, ScopedConfigDir, State};

    let _g = ScopedConfigDir::new();

    // Seed: two accounts, A active with a "stale" access token.
    let mut a = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"stale","refreshToken":"r","expiresAt":0}}"#,
    )
    .unwrap();
    a.email = Some("a@e.com".into());
    let mut b = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"bt","refreshToken":"br","expiresAt":0}}"#,
    )
    .unwrap();
    b.email = Some("b@e.com".into());
    let mut seed = State::default();
    seed.accounts.push(a);
    seed.accounts.push(b);
    seed.active = Some("a@e.com".into());
    seed.save().unwrap();

    // Model switch_to_guarded's ordering: (1) load, (2) simulate
    // absorb_before_switch by writing a fresher token for A on disk via a
    // NESTED with_state_lock (matches the real reentrant absorb path), (3)
    // reload, (4) mutate AFTER reload, (5) save.
    with_state_lock(|| {
        let _st = State::load()?;

        // Nested lock frame simulates absorb_before_switch's on-disk write
        // for the outgoing account A.
        with_state_lock(|| {
            let mut st_nested = State::load()?;
            if let Some(x) = st_nested.find_mut("a@e.com") {
                x.access_token = "absorbed_fresh".into();
                x.expires_at = 999_999;
            }
            st_nested.save()
        })?;

        // Reload — pre-reload mutations would be discarded here.
        let mut st = State::load()?;
        // Post-reload mutation: bump B's expiry as a stand-in for what
        // sync_active_from_keychain / apply_account bookkeeping do.
        if let Some(x) = st.find_mut("b@e.com") {
            x.expires_at = 111_111;
        }
        st.active = Some("b@e.com".into());
        st.save()
    })
    .unwrap();

    // A's absorbed rotation survived (fresher token persisted, not clobbered).
    let disk = State::load().unwrap();
    let a_on_disk = disk.find("a@e.com").expect("A still present");
    assert_eq!(
        a_on_disk.access_token, "absorbed_fresh",
        "reload+save must preserve the fresher token absorbed for the outgoing account (H1)"
    );
    // And B's post-reload mutation persisted.
    let b_on_disk = disk.find("b@e.com").expect("B still present");
    assert_eq!(
        b_on_disk.expires_at, 111_111,
        "post-reload mutations must survive the final save (H_R2_1 sibling)"
    );
    assert_eq!(disk.active.as_deref(), Some("b@e.com"));
}

// --- launch_agent_exe_path / sibling_app_bundle_exe ---

// Used only by macOS launch-agent / bundle tests below (all `#[cfg(target_os = "macos")]`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mock_cellar_bundle_layout(
    prefix: &str,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let version_dir = dir.join("Cellar/usagio/0.9.9");
    let bin_dir = version_dir.join("bin");
    let bundle_macos_dir = version_dir.join("usagio.app/Contents/MacOS");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&bundle_macos_dir).unwrap();

    let bare_exe = bin_dir.join("usagio");
    std::fs::write(&bare_exe, b"bare").unwrap();
    let bundle_exe = bundle_macos_dir.join("usagio");
    std::fs::write(&bundle_exe, b"bundled").unwrap();

    (dir, bare_exe, bundle_exe)
}

// macOS-only: exercises the .app bundle path-resolution helper (Homebrew
// Cellar layout) — no such concept on Linux/Windows. Uses cfg(unix)
// which the strict-cfg guard leaves alone.
#[cfg(unix)]
#[test]
fn sibling_app_bundle_exe_finds_bundle_next_to_bin_dir() {
    let (dir, bare_exe, bundle_exe) = mock_cellar_bundle_layout("usagio-bundle-test");

    let found = sibling_app_bundle_exe(&bare_exe).expect("bundle exe should be found");
    assert_eq!(found, bundle_exe);

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn sibling_app_bundle_exe_none_when_bundle_absent() {
    let dir = std::env::temp_dir().join(format!(
        "usagio-nobundle-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let bin_dir = dir.join("Cellar/usagio/0.9.9/bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let bare_exe = bin_dir.join("usagio");
    std::fs::write(&bare_exe, b"bare").unwrap();

    assert!(sibling_app_bundle_exe(&bare_exe).is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Mocks a Homebrew-style `<Cellar>/usagio/<version>/{bin,usagio.app}` layout
/// and reproduces the exact plist-building the real `MacOsAutostart::install`
/// does (minus the `launchctl load` shell-out, which needs a real launchd job
/// and a valid Mach-O binary — orthogonal to what's under test here), then
/// asserts the written LaunchAgent plist's `ProgramArguments` points at the
/// bundle's `Contents/MacOS/usagio`, not the bare binary. This is what makes
/// Login Items show the app icon instead of the generic "exec" glyph.
#[cfg(unix)]
#[test]
fn usagio_install_prefers_app_bundle_path_when_available() {
    let (dir, bare_exe, bundle_exe) = mock_cellar_bundle_layout("usagio-install-bundle-test");

    // Resolve exactly the way `cmd_install` -> `launch_agent_exe_path` would,
    // given this mocked Cellar layout (bypassing `current_exe()`/
    // `stable_exe_path()`, which can't be pointed at a scratch dir).
    let resolved = sibling_app_bundle_exe(&bare_exe).expect("bundle should resolve");
    assert_eq!(resolved, bundle_exe);

    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let plist_path = home
        .join("Library/LaunchAgents")
        .join("com.mattjackson.usagio.test-bundle.plist");

    crate::env_lock::scoped_env_var("HOME", Some(home.to_str().unwrap()), || {
        let mut prog_args = format!("    <string>{}</string>\n", resolved.display());
        prog_args.push_str("    <string>menubar</string>\n");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>Label</key><string>com.mattjackson.usagio.test-bundle</string>
  <key>ProgramArguments</key>
  <array>
{prog_args}  </array>
</dict>
</plist>
"#
        );
        std::fs::create_dir_all(plist_path.parent().unwrap()).unwrap();
        std::fs::write(&plist_path, &plist).unwrap();

        let written = std::fs::read_to_string(&plist_path).unwrap();
        assert!(
            written.contains("usagio.app/Contents/MacOS/usagio"),
            "expected ProgramArguments to point into the app bundle, got:\n{written}"
        );
        assert!(
            !written.contains(&bare_exe.display().to_string()),
            "must not fall back to the bare binary path when a bundle exists"
        );
    });

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// M12 — .app bundle direct-launch (Finder double-click) must default to the
// menu bar, not the one-shot `list` a bare CLI invocation defaults to.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn run_dispatch_defaults_to_menubar_when_invoked_from_app_bundle() {
    let bundle_exe = std::path::PathBuf::from("/Applications/usagio.app/Contents/MacOS/usagio");
    assert_eq!(effective_first_arg(&[], &bundle_exe), Some("menubar"));
}

#[cfg(unix)]
#[test]
fn run_dispatch_defaults_to_list_for_bare_binary() {
    let bare_exe = std::path::PathBuf::from("/opt/homebrew/bin/usagio");
    assert_eq!(effective_first_arg(&[], &bare_exe), None);
}

#[cfg(unix)]
#[test]
fn run_dispatch_prefers_an_explicit_arg_over_bundle_detection() {
    // Even when launched from inside a bundle, an explicit argument (e.g.
    // `usagio list` run via Terminal against the bundled binary) wins.
    let bundle_exe = std::path::PathBuf::from("/Applications/usagio.app/Contents/MacOS/usagio");
    let args = vec!["list".to_string()];
    assert_eq!(effective_first_arg(&args, &bundle_exe), Some("list"));
}

// ---------------------------------------------------------------------------
// Active-account CAS refresh (v0.5.0). `active_refresh_cas` is exercised
// directly against a `MockActiveSlotProvider` rather than through the full
// `refresh_usage_cache()` — the real `ClaudeProvider::read_active_slot` /
// `mirror_rotated_token` on macOS route through the REAL login keychain
// (`Platform::secrets()`), which unit tests must never touch (see the
// `#[ignore]`d `secret_store_roundtrip` in `platform/macos.rs` for the same
// rule). The mock provider gives `active_refresh_cas` an in-memory "OS-native
// slot" it can read/write freely, while the `/token` POST itself still goes
// through the same thread-local URL override + in-process mock HTTP server
// every other refresh test in this file uses (`oauth::refresh` is
// Claude-specific and not swappable per-provider).
// ---------------------------------------------------------------------------

/// Minimal `Provider` stand-in whose "OS-native active slot" is an in-memory
/// `Mutex<Option<String>>` instead of the real keychain / credentials file.
/// Every method besides `read_active_slot` / `mirror_rotated_token` is an
/// inert default — `active_refresh_cas` never calls them.
struct MockActiveSlotProvider {
    slot: std::sync::Mutex<Option<String>>,
}

impl MockActiveSlotProvider {
    fn new(initial: &str) -> Self {
        Self {
            slot: std::sync::Mutex::new(Some(initial.to_string())),
        }
    }

    /// Simulate Claude Code rotating the slot out from under usagio, as if a
    /// live `claude` session refreshed independently.
    #[allow(dead_code)]
    fn rotate_externally(&self, new_blob: &str) {
        *self.slot.lock().unwrap() = Some(new_blob.to_string());
    }
}

impl crate::providers::Provider for MockActiveSlotProvider {
    fn provider_id(&self) -> &'static str {
        "mock-active-slot"
    }
    fn display_name(&self) -> &'static str {
        "Mock"
    }
    fn capabilities(&self) -> crate::providers::trait_def::Capabilities {
        crate::providers::trait_def::Capabilities {
            supports_usage: false,
            supports_switching: false,
            supports_launch: false,
            supports_remove: false,
            supports_email_capture: false,
            secret_backend: crate::providers::trait_def::SecretBackend::Keychain,
            capture_mode: crate::providers::trait_def::CaptureMode::CredsOnDisk,
        }
    }
    fn capture_current_login(
        &self,
    ) -> crate::providers::trait_def::PResult<Option<crate::providers::trait_def::CapturedAccount>>
    {
        Ok(None)
    }
    fn parse_stored_blob(
        &self,
        _blob: &str,
    ) -> crate::providers::trait_def::PResult<crate::providers::trait_def::TokenGrant> {
        Err(crate::providers::trait_def::ProviderError::Unsupported)
    }
    fn patch_stored_blob(
        &self,
        _blob: &str,
        _grant: &crate::providers::trait_def::TokenGrant,
    ) -> crate::providers::trait_def::PResult<String> {
        Err(crate::providers::trait_def::ProviderError::Unsupported)
    }
    fn read_active_slot(&self) -> crate::providers::trait_def::PResult<Option<String>> {
        Ok(self.slot.lock().unwrap().clone())
    }
    fn mirror_rotated_token(&self, blob: &str) -> crate::providers::trait_def::PResult<()> {
        *self.slot.lock().unwrap() = Some(blob.to_string());
        Ok(())
    }
}

fn mock_claude_blob(access: &str, refresh: &str, expires_at: i64) -> String {
    serde_json::json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires_at,
        }
    })
    .to_string()
}

// TODO(v0.5.2): fix mock-server bootstrap race — this test flakes on macOS
// CI with "parsing token response: Failed to read JSON: Invalid argument
// (os error 22)". The mock server's `stream.read(&mut buf)` handler may
// complete before ureq has finished writing the request headers, or ureq
// reads the response before Content-Length bytes have all arrived. The
// three sibling CAS tests (`_lost`, `_skipped_drift`) and the Codex CAS
// tests exercise the same code path via a different, more robust harness
// (Codex's mock server loops read/write). Marking `#[ignore]` unblocks
// v0.5.1 release; real fix is either a read-loop-until-\r\n\r\n in the
// mock server or switching to a proper HTTP framework like `httpmock`.
#[test]
fn active_refresh_never_posts_token_for_active_account() {
    use crate::providers::claude::oauth;
    use crate::store::ScopedConfigDir;

    // `active_refresh_cas` logs via `logging::log`, which resolves
    // `store::config_dir()` — that panics in tests without a
    // `HOME_OVERRIDE` installed (see the tripwire in `store::config_dir`).
    let _g = ScopedConfigDir::new();
    // Point the OAuth refresh at a counting server. The whole point of the
    // v0.5.5 fix is that usagio NEVER mints a token for the active account
    // (a POST would rotate Anthropic's single-use refresh token and burn the
    // copy Claude Code holds → /login). So this server must receive ZERO hits.
    let (base_url, hits) = spawn_mock_token_server();
    oauth::set_token_url_override(Some(&format!("{base_url}/v1/oauth/token")));

    let blob = mock_claude_blob("active-at", "active-rt", 0);
    let provider = MockActiveSlotProvider::new(&blob);
    let mut acct = Account::from_keychain_blob(&blob).unwrap();
    acct.email = Some("active@example.com".into());

    let outcome = active_refresh_cas(&provider, &mut acct);

    oauth::set_token_url_override(None);

    assert!(
        matches!(outcome, ActiveRefreshOutcome::Adopted),
        "outcome={outcome:?}"
    );
    // The contract: no HTTP refresh was ever attempted.
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "usagio must not POST /token for the active account"
    );
    // Tokens are unchanged (adopted straight from the slot — Claude Code's).
    assert_eq!(acct.access_token, "active-at");
    assert_eq!(acct.refresh_token, "active-rt");
    // The slot itself is untouched — usagio wrote nothing back.
    let slot_blob = provider.slot.lock().unwrap().clone().unwrap();
    let slot_acct = Account::from_keychain_blob(&slot_blob).unwrap();
    assert_eq!(slot_acct.access_token, "active-at");
}

#[test]
fn active_refresh_adopts_cc_rotation_from_slot() {
    let _g = crate::store::ScopedConfigDir::new();
    // Claude Code has rotated its token behind us: the slot holds a DIFFERENT
    // (fresher) blob than our cached state. usagio must adopt it verbatim —
    // never refresh, never write back.
    let cc_blob = mock_claude_blob("cc-rotated-at", "cc-rotated-rt", 999_999_999_999);
    let provider = MockActiveSlotProvider::new(&cc_blob);

    let stale_blob = mock_claude_blob("stale-at", "stale-rt", 0);
    let mut acct = Account::from_keychain_blob(&stale_blob).unwrap();
    acct.email = Some("active@example.com".into());
    acct.needs_relogin = true; // was flagged; adopting a live grant clears it

    let outcome = active_refresh_cas(&provider, &mut acct);

    assert!(
        matches!(outcome, ActiveRefreshOutcome::Adopted),
        "outcome={outcome:?}"
    );
    assert_eq!(acct.access_token, "cc-rotated-at");
    assert_eq!(acct.refresh_token, "cc-rotated-rt");
    assert_eq!(acct.expires_at, 999_999_999_999);
    assert!(
        !acct.needs_relogin,
        "adopting the active slot clears a stale needs_relogin flag"
    );
}

#[test]
fn active_refresh_in_sync_is_a_noop_adopt() {
    let _g = crate::store::ScopedConfigDir::new();
    // Slot and state already agree — the common case. Adopt (no-op) and never
    // POST.
    let blob = mock_claude_blob("same-at", "same-rt", 7);
    let provider = MockActiveSlotProvider::new(&blob);
    let mut acct = Account::from_keychain_blob(&blob).unwrap();
    acct.email = Some("active@example.com".into());

    let outcome = active_refresh_cas(&provider, &mut acct);

    assert!(
        matches!(outcome, ActiveRefreshOutcome::Adopted),
        "outcome={outcome:?}"
    );
    assert_eq!(acct.access_token, "same-at");
    assert_eq!(acct.refresh_token, "same-rt");
    assert_eq!(acct.expires_at, 7);
}

// ---------------------------------------------------------------------------
// Inactive-account refresh still uses the plain network-outside-the-lock
// path (unaffected by the active-account CAS redesign above).
// ---------------------------------------------------------------------------

#[test]
fn refresh_usage_cache_still_refreshes_inactive_accounts() {
    use crate::providers::claude::{oauth, usage};
    use crate::store::{Account, ScopedConfigDir};

    let _g = ScopedConfigDir::new();
    let (base_url, hits) = spawn_mock_token_server();
    oauth::set_token_url_override(Some(&format!("{base_url}/v1/oauth/token")));
    usage::set_usage_url_override(Some(&format!("{base_url}/api/oauth/usage")));

    let expired = Utc::now().timestamp_millis() - 1_000;
    let mut inactive = Account::from_keychain_blob(&format!(
        r#"{{"claudeAiOauth":{{"accessToken":"inactive-at","refreshToken":"inactive-rt","expiresAt":{expired}}}}}"#
    ))
    .unwrap();
    inactive.email = Some("inactive@example.com".into());
    // No account is marked active, so refresh_usage_cache's active-CAS branch
    // is never entered (and never touches the real keychain) for this test.
    let mut seed = State::default();
    seed.accounts.push(inactive);
    seed.active = None;
    seed.save().unwrap();

    refresh_usage_cache();
    oauth::set_token_url_override(None);
    usage::set_usage_url_override(None);

    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "expected exactly one /token POST for the (only, inactive) account"
    );
    let after = State::load().unwrap();
    let inactive_after = after.find("inactive@example.com").unwrap();
    assert_eq!(inactive_after.access_token, "mock-refreshed-access-token");
    assert_eq!(inactive_after.refresh_token, "mock-refreshed-refresh-token");
}

/// Minimal single-threaded mock token endpoint: accepts TCP connections,
/// reads (and discards) the request, and replies with a fixed valid
/// refresh-grant JSON body. Returns (base_url, hits) where `hits` is bumped
/// once per accepted connection.
fn spawn_mock_token_server() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock token server");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            // Read the request line so we can tell a /token POST apart from
            // a usage GET; everything after it (headers/body) is discarded.
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let is_token_post = request.starts_with("POST");
            let body = if is_token_post {
                hits_thread.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({
                    "access_token": "mock-refreshed-access-token",
                    "refresh_token": "mock-refreshed-refresh-token",
                    "expires_in": 3600,
                })
                .to_string()
            } else {
                // Usage GET — any well-formed empty snapshot avoids a parse
                // error; this test doesn't assert on usage contents.
                serde_json::json!({
                    "five_hour": null,
                    "seven_day": null,
                    "seven_day_opus": null,
                })
                .to_string()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}"), hits)
}

// ---------------------------------------------------------------------------
// codex-switch-e2e: generic (non-Claude) capture / switch / CAS dispatch.
// Every test below uses `ScopedConfigDir` (state.json → tempdir) and
// `env_lock::scoped_env_var("CODEX_HOME", ...)` (auth.json → tempdir), so
// none of them ever touch a real `~/.codex/auth.json` or the real OS
// keychain — consistent with the crate-wide test-hermeticity contract.
// ---------------------------------------------------------------------------

fn codex_id_token(email: &str, exp: i64) -> String {
    use base64::Engine;
    let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::json!({ "email": email, "exp": exp })
            .to_string()
            .as_bytes(),
    );
    format!("{hdr}.{payload}.sig")
}

/// A full Codex `auth.json` blob for `email`, expiring at `exp` (unix
/// seconds). Mirrors `providers::codex::mod::tests::make_blob`, duplicated
/// here rather than imported since it's `#[cfg(test)]`-private to that
/// module and this file's edit scope doesn't include changing that.
fn codex_auth_json(email: &str, exp: i64, access: &str, refresh: &str) -> String {
    let id_token = codex_id_token(email, exp);
    serde_json::json!({
        "tokens": {
            "id_token": id_token,
            "access_token": access,
            "refresh_token": refresh,
        }
    })
    .to_string()
}

#[test]
fn capture_current_generic_persists_a_codex_account_into_state_v2() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let blob = codex_auth_json("codex-user@example.com", 4_000_000_000, "at1", "rt1");
        std::fs::write(dir.path().join("auth.json"), &blob).unwrap();

        let (key, existed) = capture_current_generic("codex").expect("capture must succeed");
        assert_eq!(key, "codex-user@example.com");
        assert!(
            !existed,
            "first capture of this account must report existed=false"
        );

        let state = State::load().unwrap();
        let acct = state
            .find_provider_account("codex", "codex-user@example.com")
            .expect("captured account must be persisted");
        assert_eq!(acct.access_token, "at1");
        assert_eq!(acct.refresh_token, "rt1");
        assert_eq!(acct.secret_blob, blob);
        assert_eq!(
            state.provider_accounts("codex").unwrap().active.as_deref(),
            Some("codex-user@example.com"),
            "capture must also become the active account for its provider"
        );

        // Re-capturing the same account (same auth.json) reports existed=true
        // and doesn't lose the identity.
        let (key2, existed2) = capture_current_generic("codex").unwrap();
        assert_eq!(key2, "codex-user@example.com");
        assert!(existed2);
    });
}

#[test]
fn switch_to_provider_account_writes_auth_json_and_updates_active() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        // Capture two accounts (A then B) so B ends up active, then switch
        // back to A and confirm auth.json + state.providers["codex"].active
        // both flip.
        let blob_a = codex_auth_json("a@example.com", 4_000_000_000, "at-a", "rt-a");
        std::fs::write(dir.path().join("auth.json"), &blob_a).unwrap();
        capture_current_generic("codex").unwrap();

        let blob_b = codex_auth_json("b@example.com", 4_000_000_000, "at-b", "rt-b");
        std::fs::write(dir.path().join("auth.json"), &blob_b).unwrap();
        capture_current_generic("codex").unwrap();

        let label = switch_to_provider_account("codex", "a@example.com").unwrap();
        assert_eq!(label, "a@example.com");

        let on_disk = std::fs::read_to_string(dir.path().join("auth.json")).unwrap();
        assert_eq!(on_disk, blob_a, "auth.json must now hold account A's blob");

        let state = State::load().unwrap();
        assert_eq!(
            state.provider_accounts("codex").unwrap().active.as_deref(),
            Some("a@example.com")
        );
        // Both accounts are still there — switching must not drop B.
        assert!(state
            .find_provider_account("codex", "b@example.com")
            .is_some());
    });
}

#[test]
fn switch_to_provider_account_errors_for_unknown_key() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let err = switch_to_provider_account("codex", "nobody@example.com").unwrap_err();
        assert!(format!("{err}").contains("no codex account matches"));
    });
}

#[test]
fn resolve_provider_selector_matches_exact_and_unique_prefix() {
    let mut state = State::default();
    state.upsert_provider_account(
        "codex",
        crate::store::ProviderAccount {
            key: "dev@example.com".into(),
            secret_blob: "{}".into(),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 0,
            identity_email: Some("dev@example.com".into()),
            identity_uuid: None,
            identity_display_name: None,
            identity_native_blob: serde_json::Value::Null,
            cached_usage: None,
            notif_state: Default::default(),
            needs_relogin: false,
        },
    );

    assert_eq!(
        resolve_provider_selector(&state, "dev@example.com"),
        Some(("codex".to_string(), "dev@example.com".to_string()))
    );
    assert_eq!(
        resolve_provider_selector(&state, "dev"),
        Some(("codex".to_string(), "dev@example.com".to_string()))
    );
    assert_eq!(resolve_provider_selector(&state, "nobody"), None);
}

#[test]
fn refresh_provider_active_account_noop_when_provider_does_not_support_active_refresh() {
    // `opencode` is registered but doesn't override `supports_active_refresh`
    // (defaults `false`) — the gate this test pins must stop
    // `refresh_provider_active_account` before it ever looks at
    // `state.providers["opencode"]`, let alone tries a network call.
    let _cfg = crate::store::ScopedConfigDir::new();
    let seed = crate::store::ProviderAccount {
        key: "x@example.com".into(),
        secret_blob: "untouched".into(),
        access_token: "untouched-at".into(),
        refresh_token: "untouched-rt".into(),
        expires_at: 1,
        identity_email: Some("x@example.com".into()),
        identity_uuid: None,
        identity_display_name: None,
        identity_native_blob: serde_json::Value::Null,
        cached_usage: None,
        notif_state: Default::default(),
        needs_relogin: false,
    };
    with_state_lock(|| {
        let mut st = State::load()?;
        st.upsert_provider_account("opencode", seed);
        st.provider_accounts_mut("opencode").active = Some("x@example.com".to_string());
        st.save()
    })
    .unwrap();

    refresh_provider_active_account("opencode");

    let after = State::load().unwrap();
    let acct = after
        .find_provider_account("opencode", "x@example.com")
        .unwrap();
    assert_eq!(acct.secret_blob, "untouched");
    assert_eq!(acct.access_token, "untouched-at");
}

#[test]
fn refresh_provider_active_account_codex_fresh_token_is_a_noop() {
    // The stored token is far from expiry and has no `last_refresh` at all,
    // so `grant_needs_refresh` reports `Fresh` — no network call should even
    // be attempted, and the account must be byte-for-byte unchanged.
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let blob = codex_auth_json("fresh@example.com", 4_000_000_000, "fresh-at", "fresh-rt");
        std::fs::write(dir.path().join("auth.json"), &blob).unwrap();
        capture_current_generic("codex").unwrap();

        refresh_provider_active_account("codex");

        let state = State::load().unwrap();
        let acct = state
            .find_provider_account("codex", "fresh@example.com")
            .unwrap();
        assert_eq!(acct.access_token, "fresh-at");
        assert_eq!(acct.secret_blob, blob);
    });
}

#[test]
fn refresh_provider_active_account_codex_refreshes_and_persists_new_grant() {
    // End-to-end: an expired stored token triggers `codex_active_refresh`,
    // which drives `providers::codex::oauth::active_refresh_cas` against a
    // local mock token endpoint, wins the CAS (nothing else touches
    // auth.json during the test), and the new grant lands back in
    // `state.providers["codex"]` — this is gap-4 of codex-switch-e2e end to
    // end, without touching the real network or `~/.codex/auth.json`.
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().to_str().unwrap().to_string();

    // `scoped_env_var` isn't reentrant, so both env vars are set inside one
    // call (mirrors `providers::codex::oauth_tests::with_codex_env`, which
    // this file's edit scope doesn't extend to import from).
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(&codex_home), || {
        let expired_blob = codex_auth_json("stale@example.com", 1, "stale-at", "stale-rt");
        std::fs::write(dir.path().join("auth.json"), &expired_blob).unwrap();
        capture_current_generic("codex").unwrap();

        let (base_url, _hits) = spawn_mock_token_server();
        #[allow(clippy::disallowed_methods)]
        let prev_url = std::env::var_os("CODEX_REFRESH_TOKEN_URL_OVERRIDE");
        #[allow(clippy::disallowed_methods)]
        std::env::set_var(
            "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
            format!("{base_url}/v1/oauth/token"),
        );

        refresh_provider_active_account("codex");

        #[allow(clippy::disallowed_methods)]
        match prev_url {
            Some(v) => std::env::set_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", v),
            None => std::env::remove_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE"),
        }

        let state = State::load().unwrap();
        let acct = state
            .find_provider_account("codex", "stale@example.com")
            .expect("account survives the refresh");
        assert_eq!(acct.access_token, "mock-refreshed-access-token");
        assert_ne!(acct.secret_blob, expired_blob);

        let on_disk = std::fs::read_to_string(dir.path().join("auth.json")).unwrap();
        assert!(on_disk.contains("mock-refreshed-access-token"));
    });
}

// usagio must NEVER mint a token for the active account — Anthropic's refresh
// tokens are single-use, so a usagio POST would rotate the family server-side
// and invalidate the copy Claude Code holds (→ /login). The active-account
// path (`active_refresh_cas`) is therefore a pure adopt-from-slot, exercised in
// isolation above against `MockActiveSlotProvider` so no test here ever touches
// the real OS keychain that `refresh_usage_cache` would resolve
// `ClaudeProvider::read_active_slot()` to.
