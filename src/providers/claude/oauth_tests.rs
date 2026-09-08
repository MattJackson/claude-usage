use super::*;

fn acct_expiring_at(expires_at: i64) -> Account {
    let blob = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "acc",
            "refreshToken": "ref",
            "expiresAt": expires_at,
        }
    })
    .to_string();
    Account::from_keychain_blob(&blob).unwrap()
}

#[test]
fn ensure_fresh_skips_refresh_when_token_is_far_from_expiry() {
    // Expires an hour out, skew of 5 min: no refresh, no network call.
    let future = chrono::Utc::now().timestamp_millis() + 3_600_000;
    let mut a = acct_expiring_at(future);
    let changed = ensure_fresh(&mut a, 300).unwrap();
    assert!(!changed);
    assert_eq!(a.access_token, "acc");
    assert_eq!(a.expires_at, future);
}

#[test]
fn needs_refresh_boundaries() {
    let now = 1_000_000_000_000; // arbitrary "now" in millis
    let skew = 300; // 5 min
                    // Far future: no refresh.
    assert!(!needs_refresh(now + 3_600_000, now, skew));
    // Within the skew window: refresh.
    assert!(needs_refresh(now + 60_000, now, skew));
    // Already expired: refresh.
    assert!(needs_refresh(now - 1, now, skew));
}

#[test]
fn needs_refresh_saturates_on_corrupt_expiry() {
    // A corrupt near-i64::MIN expires_at must read as "expired" (true), not
    // overflow — a plain subtraction would panic in debug / wrap in release.
    assert!(needs_refresh(i64::MIN, 1_000_000_000_000, 300));
    // A corrupt i64::MAX expiry with a normal skew reads as "not expiring soon"
    // without overflowing the subtraction.
    assert!(!needs_refresh(i64::MAX, 0, 300));
}

#[test]
fn refresh_with_empty_refresh_token_returns_invalid_grant_without_network() {
    // No refresh token to send is treated identically to a rejected grant so
    // the caller flags for re-login instead of hammering the endpoint.
    let mut a = acct_expiring_at(0);
    a.refresh_token.clear();
    let err = refresh(&mut a).expect_err("empty refresh token must not succeed");
    assert!(matches!(err, RefreshError::InvalidGrant));
}

#[test]
fn refresh_error_display_carries_variant() {
    // Callers log these; make sure the InvalidGrant string is stable enough
    // to be recognized (the fix hinges on catching invalid_grant explicitly).
    assert!(RefreshError::InvalidGrant
        .to_string()
        .contains("invalid_grant"));
    assert!(RefreshError::RateLimited.to_string().contains("429"));
    let s = RefreshError::Transient("boom".into()).to_string();
    assert!(s.contains("boom"));
}

/// robustness-01: a server that accepts the TCP connection but never writes a
/// response must not hang the caller forever. Before the shared
/// `http_agent()` (with `.timeout_read()` set) existed, the default
/// `ureq::Agent` only bounded the TCP *connect*, so this exact scenario
/// (connection accepted, response withheld) would block indefinitely.
#[test]
fn post_token_read_timeout_fails_fast_not_forever() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stalling server");
    let addr = listener.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        // Accept the connection and hold it open well past the client's read
        // timeout without ever writing a response byte.
        if let Ok((stream, _)) = listener.accept() {
            std::thread::sleep(std::time::Duration::from_secs(30));
            drop(stream);
        }
    });
    let url = format!("http://{addr}/oauth/token");
    set_token_url_override(Some(&url));
    let start = std::time::Instant::now();
    let mut a = acct_expiring_at(0);
    let err = refresh(&mut a).expect_err("stalled server must not succeed");
    let elapsed = start.elapsed();
    set_token_url_override(None);
    assert!(
        matches!(err, RefreshError::Transient(_)),
        "expected a transient (timeout) error, got {err:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "expected the read timeout to fire well under 20s, took {elapsed:?}"
    );
}

#[test]
fn token_response_allows_missing_refresh_token() {
    // RFC 6749 §6: refresh_token is OPTIONAL in a refresh-grant response.
    let r: TokenResponse =
        serde_json::from_str(r#"{"access_token":"a","expires_in":3600}"#).unwrap();
    assert_eq!(r.access_token, "a");
    assert!(r.refresh_token.is_none());
    // And it still parses when present.
    let r2: TokenResponse =
        serde_json::from_str(r#"{"access_token":"a","refresh_token":"r2","expires_in":10}"#)
            .unwrap();
    assert_eq!(r2.refresh_token.as_deref(), Some("r2"));
}
