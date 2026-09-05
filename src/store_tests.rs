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

fn acct(name: &str) -> Account {
    Account::from_keychain_blob(name.to_string(), &blob("acc", "ref", 123)).unwrap()
}

#[test]
fn from_keychain_blob_parses_valid() {
    let a = Account::from_keychain_blob("work".to_string(), &blob("tok", "rt", 999)).unwrap();
    assert_eq!(a.name, "work");
    assert_eq!(a.access_token, "tok");
    assert_eq!(a.refresh_token, "rt");
    assert_eq!(a.expires_at, 999);
    assert!(a.oauth_account.is_none());
    assert!(a.user_id.is_none());
}

#[test]
fn from_keychain_blob_missing_expires_defaults_zero() {
    let b = serde_json::json!({
        "claudeAiOauth": { "accessToken": "t", "refreshToken": "r" }
    })
    .to_string();
    let a = Account::from_keychain_blob("x".to_string(), &b).unwrap();
    assert_eq!(a.expires_at, 0);
}

#[test]
fn from_keychain_blob_rejects_non_json() {
    assert!(Account::from_keychain_blob("x".to_string(), "not json").is_err());
}

#[test]
fn from_keychain_blob_rejects_missing_oauth_object() {
    let b = serde_json::json!({ "somethingElse": {} }).to_string();
    assert!(Account::from_keychain_blob("x".to_string(), &b).is_err());
}

#[test]
fn from_keychain_blob_rejects_missing_access_token() {
    let b = serde_json::json!({ "claudeAiOauth": { "refreshToken": "r" } }).to_string();
    assert!(Account::from_keychain_blob("x".to_string(), &b).is_err());
}

#[test]
fn set_tokens_updates_fields_and_patches_blob() {
    let mut a = Account::from_keychain_blob("x".to_string(), &blob("old", "oldr", 1)).unwrap();
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
fn find_is_case_insensitive() {
    let mut s = State::default();
    s.accounts.push(acct("Personal"));
    assert!(s.find("personal").is_some());
    assert!(s.find("PERSONAL").is_some());
    assert!(s.find("Personal").is_some());
    assert!(s.find("other").is_none());
}

#[test]
fn find_mut_is_case_insensitive() {
    let mut s = State::default();
    s.accounts.push(acct("Work"));
    assert!(s.find_mut("work").is_some());
    assert!(s.find_mut("nope").is_none());
}

#[test]
fn remove_is_case_insensitive() {
    let mut s = State::default();
    s.accounts.push(acct("Dev1"));
    assert!(s.remove("dev1"));
    assert!(s.accounts.is_empty());
    assert!(!s.remove("dev1"));
}

#[test]
fn upsert_replaces_existing_and_keeps_stored_casing() {
    let mut s = State::default();
    s.accounts.push(acct("Personal"));
    let mut replacement = acct("personal");
    replacement.access_token = "rotated".to_string();
    s.upsert(replacement);

    assert_eq!(s.accounts.len(), 1);
    let a = &s.accounts[0];
    assert_eq!(a.name, "Personal"); // stored casing preserved
    assert_eq!(a.access_token, "rotated"); // contents replaced
}

#[test]
fn upsert_appends_new_account() {
    let mut s = State::default();
    s.accounts.push(acct("Personal"));
    s.upsert(acct("Work"));
    assert_eq!(s.accounts.len(), 2);
    assert!(s.find("work").is_some());
}

#[test]
fn rename_changes_name_and_active_case_insensitively() {
    let mut s = State::default();
    s.accounts.push(acct("Personal"));
    s.active = Some("Personal".to_string());
    s.rename("personal", "Home").unwrap();
    assert_eq!(s.accounts[0].name, "Home");
    assert_eq!(s.active.as_deref(), Some("Home"));
}

#[test]
fn rename_rejects_missing_empty_and_collision() {
    let mut s = State::default();
    s.accounts.push(acct("Personal"));
    s.accounts.push(acct("Work"));
    assert!(s.rename("nope", "X").is_err()); // missing source
    assert!(s.rename("Personal", "  ").is_err()); // empty target
    assert!(s.rename("Personal", "work").is_err()); // collides (case-insensitive)
                                                    // A pure case change of the same account is allowed.
    assert!(s.rename("personal", "PERSONAL").is_ok());
}
