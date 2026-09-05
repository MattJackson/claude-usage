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
