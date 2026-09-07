use super::*;

fn blob(access: &str, refresh: &str, expires: i64) -> String {
    serde_json::json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires,
        }
    })
    .to_string()
}

/// An account keyed by `email`.
fn acct(email: &str) -> Account {
    let mut a = Account::from_keychain_blob(&blob("acc", "ref", 123)).unwrap();
    a.email = Some(email.to_string());
    a
}

#[test]
fn from_keychain_blob_parses_valid() {
    let a = Account::from_keychain_blob(&blob("tok", "rt", 999)).unwrap();
    assert_eq!(a.access_token, "tok");
    assert_eq!(a.refresh_token, "rt");
    assert_eq!(a.expires_at, 999);
    assert!(a.email.is_none());
    assert!(a.oauth_account.is_none());
    assert!(a.user_id.is_none());
}

#[test]
fn set_tokens_if_newer_only_applies_when_at_least_as_new() {
    let mut a = Account::from_keychain_blob(&blob("old", "oldr", 100)).unwrap();
    // Older expiry: rejected, tokens unchanged.
    assert!(!a.set_tokens_if_newer("stale".into(), "staler".into(), 50));
    assert_eq!(a.access_token, "old");
    assert_eq!(a.expires_at, 100);
    // Equal expiry: applied (>=).
    assert!(a.set_tokens_if_newer("eq".into(), "eqr".into(), 100));
    assert_eq!(a.access_token, "eq");
    // Newer expiry: applied, and the blob is kept in sync.
    assert!(a.set_tokens_if_newer("new".into(), "newr".into(), 200));
    assert_eq!(a.access_token, "new");
    assert_eq!(a.refresh_token, "newr");
    assert_eq!(a.expires_at, 200);
    assert!(a.keychain_blob.contains("new"));
}

#[test]
fn from_keychain_blob_missing_expires_defaults_zero() {
    let b = serde_json::json!({
        "claudeAiOauth": { "accessToken": "t", "refreshToken": "r" }
    })
    .to_string();
    let a = Account::from_keychain_blob(&b).unwrap();
    assert_eq!(a.expires_at, 0);
}

#[test]
fn from_keychain_blob_rejects_non_json() {
    assert!(Account::from_keychain_blob("not json").is_err());
}

#[test]
fn from_keychain_blob_rejects_missing_oauth_object() {
    let b = serde_json::json!({ "somethingElse": {} }).to_string();
    assert!(Account::from_keychain_blob(&b).is_err());
}

#[test]
fn from_keychain_blob_rejects_missing_access_token() {
    let b = serde_json::json!({ "claudeAiOauth": { "refreshToken": "r" } }).to_string();
    assert!(Account::from_keychain_blob(&b).is_err());
}

#[test]
fn set_tokens_updates_fields_and_patches_blob() {
    let mut a = Account::from_keychain_blob(&blob("old", "oldr", 1)).unwrap();
    a.set_tokens("new".to_string(), "newr".to_string(), 42);
    assert_eq!(a.access_token, "new");
    assert_eq!(a.refresh_token, "newr");
    assert_eq!(a.expires_at, 42);

    // The embedded keychain blob must be patched too.
    let v: serde_json::Value = serde_json::from_str(&a.keychain_blob).unwrap();
    let o = &v["claudeAiOauth"];
    assert_eq!(o["accessToken"], "new");
    assert_eq!(o["refreshToken"], "newr");
    assert_eq!(o["expiresAt"], 42);
}

#[test]
fn set_tokens_clears_needs_relogin_flag() {
    // A successful refresh must clear the flag automatically — recovery is
    // silent after the user runs `claude /login` and we adopt the new blob.
    let mut a = Account::from_keychain_blob(&blob("old", "oldr", 1)).unwrap();
    a.needs_relogin = true;
    a.set_tokens("new".to_string(), "newr".to_string(), 42);
    assert!(!a.needs_relogin);
}

#[test]
fn set_tokens_if_newer_clears_needs_relogin_on_successful_update() {
    let mut a = Account::from_keychain_blob(&blob("old", "oldr", 100)).unwrap();
    a.needs_relogin = true;
    assert!(a.set_tokens_if_newer("new".into(), "newr".into(), 200));
    assert!(!a.needs_relogin);
}

#[test]
fn needs_relogin_defaults_false_on_load() {
    // Legacy state.json entries with no needs_relogin key must load as false.
    let v = serde_json::json!({
        "accounts": [
            {
                "email": "x@e.com",
                "access_token": "a",
                "refresh_token": "r",
                "expires_at": 1i64,
                "keychain_blob": "",
            }
        ]
    });
    let s = State::from_value(&v);
    assert_eq!(s.accounts.len(), 1);
    assert!(!s.accounts[0].needs_relogin);
}

#[test]
fn needs_relogin_round_trips_through_save_load() {
    // Set the flag, serialize with serde_json::to_value, and reload via
    // from_value — the flag survives the round trip (both #[serde(default)]
    // on the struct and the explicit key-lookup in from_value).
    let mut a = acct("x@e.com");
    a.needs_relogin = true;
    let state = State {
        accounts: vec![a],
        ..State::default()
    };
    let v = serde_json::to_value(&state).unwrap();
    let s = State::from_value(&v);
    assert!(s.accounts[0].needs_relogin);
}

#[test]
fn identity_uuid_reads_oauth_account() {
    let mut a = acct("x@e.com");
    assert!(a.identity_uuid().is_none());
    a.oauth_account = Some(serde_json::json!({ "accountUuid": "u-123" }));
    assert_eq!(a.identity_uuid().as_deref(), Some("u-123"));
}

#[test]
fn find_is_case_insensitive_by_email() {
    let mut s = State::default();
    s.accounts.push(acct("Person@Example.com"));
    assert!(s.find("person@example.com").is_some());
    assert!(s.find("PERSON@EXAMPLE.COM").is_some());
    assert!(s.find("other@example.com").is_none());
}

#[test]
fn find_mut_is_case_insensitive_by_email() {
    let mut s = State::default();
    s.accounts.push(acct("work@e.com"));
    assert!(s.find_mut("WORK@e.com").is_some());
    assert!(s.find_mut("nope@e.com").is_none());
}

#[test]
fn remove_is_case_insensitive_by_email() {
    let mut s = State::default();
    s.accounts.push(acct("dev@e.com"));
    assert!(s.remove("DEV@e.com"));
    assert!(s.accounts.is_empty());
    assert!(!s.remove("dev@e.com"));
}

#[test]
fn upsert_replaces_existing_by_email() {
    let mut s = State::default();
    s.accounts.push(acct("me@e.com"));
    let mut replacement = acct("ME@e.com");
    replacement.access_token = "rotated".to_string();
    s.upsert(replacement);
    assert_eq!(s.accounts.len(), 1);
    assert_eq!(s.accounts[0].access_token, "rotated");
}

#[test]
fn upsert_appends_new_account() {
    let mut s = State::default();
    s.accounts.push(acct("a@e.com"));
    s.upsert(acct("b@e.com"));
    assert_eq!(s.accounts.len(), 2);
    assert!(s.find("b@e.com").is_some());
}

#[test]
fn resolve_exact_and_unique_prefix() {
    let mut s = State::default();
    s.accounts.push(acct("dev@getbusbar.com"));
    s.accounts.push(acct("matthew@pq.io"));
    // Exact (case-insensitive).
    assert_eq!(s.resolve("DEV@getbusbar.com").unwrap(), "dev@getbusbar.com");
    // Unique prefix.
    assert_eq!(s.resolve("dev").unwrap(), "dev@getbusbar.com");
    assert_eq!(s.resolve("matt").unwrap(), "matthew@pq.io");
}

#[test]
fn resolve_ambiguous_and_missing_error() {
    let mut s = State::default();
    s.accounts.push(acct("dev1@e.com"));
    s.accounts.push(acct("dev2@e.com"));
    assert!(s.resolve("dev").is_err()); // ambiguous
    assert!(s.resolve("nobody").is_err()); // no match
    assert!(s.resolve("").is_err()); // empty
}

#[test]
fn migrates_old_name_keyed_state() {
    // Old shape: accounts have `name`, and `active` is a name.
    let old = serde_json::json!({
        "accounts": [
            {
                "name": "dev1",
                "email": "dev@getbusbar.com",
                "access_token": "a1",
                "refresh_token": "r1",
                "expires_at": 1,
                "keychain_blob": "{}"
            },
            {
                "name": "Personal",
                "oauth_account": { "emailAddress": "matthew@pq.io", "accountUuid": "u1" },
                "access_token": "a2",
                "refresh_token": "r2",
                "expires_at": 2,
                "keychain_blob": "{}"
            }
        ],
        "active": "Personal"
    });
    let s = State::from_value(&old);
    assert_eq!(s.accounts.len(), 2);
    assert_eq!(s.find("dev@getbusbar.com").unwrap().access_token, "a1");
    // email backfilled from oauth_account.emailAddress
    assert!(s.find("matthew@pq.io").is_some());
    // active migrated from the legacy name to that account's email
    assert_eq!(s.active.as_deref(), Some("matthew@pq.io"));
}
