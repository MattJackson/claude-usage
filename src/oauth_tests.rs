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
