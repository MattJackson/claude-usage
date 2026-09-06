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

// --- next_interval (backoff) ---

#[test]
fn next_interval_resets_to_base_when_not_limited() {
    // A clean cycle always returns to the base cadence, even from a backed-off value.
    assert_eq!(next_interval(600, 60, false), 60);
    assert_eq!(next_interval(60, 60, false), 60);
}

#[test]
fn next_interval_doubles_on_rate_limit_capped() {
    // Doubles on a rate limit…
    assert_eq!(next_interval(60, 60, true), 120);
    // …never below base even if `current` was stale-small…
    assert_eq!(next_interval(1, 60, true), 120);
    // …and is capped at the max.
    assert_eq!(
        next_interval(WATCH_MAX_INTERVAL_SECS, 60, true),
        WATCH_MAX_INTERVAL_SECS
    );
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
    assert!(!path.with_extension("json.claude-usage.tmp").exists());
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
        ("claude-usage.log", "claude-usage.log.1"),
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
    let path = dir.path().join("claude-usage.log");
    std::fs::write(&path, b"tiny").unwrap();
    logging::rotate_if_large(&path, 1_000_000);
    assert!(path.exists());
    assert!(!dir.path().join("claude-usage.log.1").exists());
}
