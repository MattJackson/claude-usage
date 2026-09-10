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

// ---------------------------------------------------------------------------
// save_state_safe — overwrite protection + rolling backups
//
// Every test in this block uses `ScopedConfigDir` so `config_dir()` resolves
// to a per-test tempdir. Any test that omits the guard would trip the
// cfg(test) tripwire in `config_dir()` and panic on the first save — which is
// exactly the safety net we want.
// ---------------------------------------------------------------------------

fn make_state_with(emails: &[&str]) -> State {
    let mut st = State::default();
    for e in emails {
        st.accounts.push(acct(e));
    }
    st
}

#[test]
fn save_state_safe_refuses_dropping_account_without_remove() {
    let _g = ScopedConfigDir::new();
    // Seed disk with two accounts.
    make_state_with(&["a@e.com", "b@e.com"]).save().unwrap();

    // Build a NEW state that only has one — WITHOUT calling remove(). This
    // simulates a stale/blank load about to wipe the config.
    let mut bad = State::default();
    bad.accounts.push(acct("a@e.com"));
    // pending_removals stays empty — save must refuse.
    let err = bad.save().expect_err("save must refuse silent drop");
    let msg = format!("{err:#}");
    assert!(msg.contains("REFUSED save_state"), "message: {msg}");
    assert!(
        msg.contains("b@e.com"),
        "message names the dropped account: {msg}"
    );

    // On-disk state.json is UNCHANGED (still has both accounts).
    let on_disk = State::load().unwrap();
    let keys: Vec<String> = on_disk
        .accounts
        .iter()
        .map(|a| a.key().to_string())
        .collect();
    assert!(keys.contains(&"a@e.com".to_string()));
    assert!(keys.contains(&"b@e.com".to_string()));
}

#[test]
fn save_state_safe_dumps_rejected_state_redacted_into_backups() {
    // H2 (round-1 codeaudit): the rejected-state dump used to go to
    // /tmp/usagio-state-rejected-<ts>.json (shared, default umask,
    // world-readable) and carried plaintext OAuth tokens. It now lives under
    // config_dir/backups/rejected-<ts>.json (0600), with tokens redacted.
    let _g = ScopedConfigDir::new();
    make_state_with(&["a@e.com", "b@e.com"]).save().unwrap();
    let mut bad = State::default();
    bad.accounts.push(acct("a@e.com"));
    let _ = bad.save().unwrap_err();

    let backups_dir = config_dir().unwrap().join("backups");
    let mut found = None;
    for entry in std::fs::read_dir(&backups_dir).unwrap().flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().to_string();
        if name.starts_with("state-rejected-") && name.ends_with(".json") {
            found = Some(entry.path());
            break;
        }
    }
    let dump = found.expect("rejected state was dumped into config_dir/backups/");
    let contents = std::fs::read_to_string(&dump).unwrap();
    assert!(
        contents.contains("a@e.com"),
        "dump preserves account keys: {contents}"
    );
    // Tokens must NOT leak — the tempdir-backed acct() blob uses "acc"/"ref"
    // as its access/refresh tokens; the redacted dump replaces them.
    assert!(
        !contents.contains("\"acc\""),
        "access token must not appear verbatim in dump: {contents}"
    );
    assert!(
        !contents.contains("\"ref\""),
        "refresh token must not appear verbatim in dump: {contents}"
    );
    assert!(
        contents.contains("<redacted>"),
        "dump uses redaction placeholder: {contents}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dump).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "rejected dump must be owner-only");
    }
}

#[test]
fn save_state_safe_allows_explicit_remove_via_remove_method() {
    let _g = ScopedConfigDir::new();
    make_state_with(&["a@e.com", "b@e.com"]).save().unwrap();

    let mut st = State::load().unwrap();
    assert!(st.remove("b@e.com"), "remove reports success");
    st.save().expect("explicit remove is authorised");

    let on_disk = State::load().unwrap();
    let keys: Vec<String> = on_disk
        .accounts
        .iter()
        .map(|a| a.key().to_string())
        .collect();
    assert_eq!(keys, vec!["a@e.com".to_string()]);
}

#[test]
fn save_state_safe_writes_rolling_backup_before_overwrite() {
    let g = ScopedConfigDir::new();
    // First save creates state.json with no prior file → no backup written.
    make_state_with(&["a@e.com"]).save().unwrap();
    // Second save overwrites → prior state.json must be copied to backups/.
    make_state_with(&["a@e.com", "b@e.com"]).save().unwrap();

    let backups_dir = g.home().join(".config/usagio/backups");
    assert!(
        backups_dir.exists(),
        "backups dir must be created on first overwrite"
    );
    let count = std::fs::read_dir(&backups_dir).unwrap().count();
    assert_eq!(count, 1, "exactly one backup after one overwrite");

    // The backup contains the pre-overwrite state (single-account version).
    let backup = std::fs::read_dir(&backups_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let contents = std::fs::read_to_string(&backup).unwrap();
    assert!(contents.contains("a@e.com"));
    assert!(!contents.contains("b@e.com"), "backup is the OLD state");
}

#[test]
fn save_state_safe_prunes_backups_to_cap() {
    let g = ScopedConfigDir::new();
    // Seed 25 pre-existing backup files with staggered mtimes so pruning has
    // an unambiguous order.
    let backups_dir = g.home().join(".config/usagio/backups");
    std::fs::create_dir_all(&backups_dir).unwrap();
    for i in 0..25u32 {
        let p = backups_dir.join(format!(
            "state-2020{:02}{:02}-000000.json",
            i / 12 + 1,
            i % 12 + 1
        ));
        std::fs::write(&p, format!("stub-{i}")).unwrap();
        // Space the mtimes so ordering is deterministic.
        let ts = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(1_700_000_000 + u64::from(i) * 60);
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(ts)
            .unwrap();
    }
    prune_backups(&backups_dir, BACKUP_KEEP_COUNT).unwrap();
    let count = std::fs::read_dir(&backups_dir).unwrap().count();
    assert_eq!(count, BACKUP_KEEP_COUNT);
    // The oldest 5 must have been removed (the ones with the smallest mtime).
    for i in 0..5u32 {
        let name = format!("state-2020{:02}{:02}-000000.json", i / 12 + 1, i % 12 + 1);
        assert!(
            !backups_dir.join(&name).exists(),
            "oldest backup {name} should have been pruned"
        );
    }
}

#[test]
fn save_state_safe_no_backup_when_no_prior_state() {
    let g = ScopedConfigDir::new();
    // First save ever — no state.json on disk, so nothing to back up.
    make_state_with(&["a@e.com"]).save().unwrap();
    let backups_dir = g.home().join(".config/usagio/backups");
    assert!(
        !backups_dir.exists(),
        "no backup written on first-ever save"
    );
}

#[cfg(unix)]
#[test]
fn save_state_safe_backup_dir_is_0700_and_file_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let g = ScopedConfigDir::new();
    make_state_with(&["a@e.com"]).save().unwrap();
    make_state_with(&["a@e.com", "b@e.com"]).save().unwrap();

    let backups_dir = g.home().join(".config/usagio/backups");
    let dir_mode = std::fs::metadata(&backups_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700, "backups dir is 0700");
    let backup = std::fs::read_dir(&backups_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let file_mode = std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600, "backup file is 0600");
}

#[test]
fn config_dir_itself_is_0700() {
    // security-01 (v0.5.2 audit): the top-level `~/.config/usagio` dir must
    // be owner-only, not just `backups/` beneath it. `config_dir()` should
    // harden it on every call, even before anything has ever been written
    // (e.g. a fresh install whose first ever call is a read-only `list`).
    use std::os::unix::fs::PermissionsExt;
    let g = ScopedConfigDir::new();
    let dir = config_dir().unwrap();
    assert!(dir.exists(), "config_dir() must create the dir it returns");
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "top-level config dir must be 0700");
    // Roundtrip: a save() afterwards must not regress the permission.
    make_state_with(&["a@e.com"]).save().unwrap();
    let mode_after_save = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode_after_save, 0o700, "config dir stays 0700 after a save");
    drop(g);
}

// -----------------------------------------------------------------------------
// cfg(test) tripwire: `config_dir()` must panic when called without an active
// ScopedConfigDir/TestConfigDir guard. This is the safety net that keeps a
// stray test from silently writing to the developer's real state.json.
// -----------------------------------------------------------------------------

#[test]
#[should_panic(expected = "no HOME_OVERRIDE installed")]
fn config_dir_panics_without_guard() {
    // Deliberately DO NOT install a ScopedConfigDir; the call itself panics.
    let _ = config_dir();
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

// ---------------------------------------------------------------------------
// Reconciler defensiveness (never-re-login part 2)
//
// The v0.3.x menu-bar reconciler deleted keychain items whenever an account
// went missing from state.json — the incident. v0.4.0 has no such
// deletion path (the only SecretStore::delete callers are tests), and
// `save_state_safe` refuses to persist a state that has silently dropped
// an account. This test pins that invariant so a future refactor can't
// reintroduce the reconciler foot-gun without a red test.
// ---------------------------------------------------------------------------

#[test]
fn reconciler_never_persists_silent_account_drop() {
    // Seed disk with three accounts — represents three keychain items indexed
    // by these emails.
    let _g = ScopedConfigDir::new();
    make_state_with(&["a@e.com", "b@e.com", "c@e.com"])
        .save()
        .unwrap();

    // Simulate a buggy reconciler: load state, then remove an account from
    // the in-memory Vec *directly* (not via `State::remove`), so
    // `pending_removals` is NOT populated. This is exactly the shape the
    // v0.3.x code took before the shrink triggered a keychain purge.
    let mut buggy = State::load().unwrap();
    buggy.accounts.retain(|a| a.key() != "b@e.com");
    assert_eq!(buggy.accounts.len(), 2, "reconciler shrunk in-memory state");

    // save_state_safe MUST refuse the write.
    let err = buggy
        .save()
        .expect_err("silent shrink must be refused by save_state_safe");
    let msg = format!("{err:#}");
    assert!(msg.contains("REFUSED save_state"), "refusal is loud: {msg}");
    assert!(
        msg.contains("b@e.com"),
        "refusal names the vanished account: {msg}"
    );

    // The on-disk state is untouched — every account key that any keychain
    // index would resolve is still present, so no downstream reconciler
    // could compute an "orphan" set and start purging.
    let on_disk = State::load().unwrap();
    let keys: Vec<String> = on_disk
        .accounts
        .iter()
        .map(|a| a.key().to_string())
        .collect();
    assert_eq!(keys.len(), 3, "all three accounts still on disk");
    for want in ["a@e.com", "b@e.com", "c@e.com"] {
        assert!(
            keys.iter().any(|k| k == want),
            "{want} preserved after silent-shrink attempt"
        );
    }
}

// ---------------------------------------------------------------------------
// H2 (v0.5.0 codeaudit) — restore flow: accounts_dropped_by / save_state_restore
// / stash_pre_restore. Exercises the store-level building blocks that
// `menubar::handle_backup_restore_dialog` composes; the dialog itself opens a
// native file picker + osascript confirm, so it isn't unit-testable, but its
// entire safety-relevant behaviour lives in these three functions.
// ---------------------------------------------------------------------------

#[test]
fn accounts_dropped_by_reports_specific_emails_not_just_a_count() {
    let _g = ScopedConfigDir::new();
    make_state_with(&["dev@getbusbar.com", "matthew@pq.io"])
        .save()
        .unwrap();

    let restore_target = make_state_with(&["dev@getbusbar.com"]);
    let dropped = accounts_dropped_by(&restore_target).unwrap();
    assert_eq!(dropped, vec!["matthew@pq.io".to_string()]);
}

#[test]
fn accounts_dropped_by_is_empty_when_restore_only_adds_accounts() {
    let _g = ScopedConfigDir::new();
    make_state_with(&["a@e.com"]).save().unwrap();

    // Restoring a file with a SUPERSET of the current accounts must report no
    // drops — this is the "restore adds an account" case, which must succeed
    // with no confirmation prompt at all.
    let restore_target = make_state_with(&["a@e.com", "b@e.com"]);
    let dropped = accounts_dropped_by(&restore_target).unwrap();
    assert!(dropped.is_empty());
}

#[test]
fn accounts_dropped_by_empty_when_no_live_state_on_disk() {
    let _g = ScopedConfigDir::new();
    // No prior save() at all — a first-ever restore has nothing to drop.
    let restore_target = make_state_with(&["a@e.com"]);
    let dropped = accounts_dropped_by(&restore_target).unwrap();
    assert!(dropped.is_empty());
}

#[test]
fn save_state_restore_succeeds_when_only_adding_accounts() {
    let _g = ScopedConfigDir::new();
    make_state_with(&["a@e.com"]).save().unwrap();

    // Unlike a bare `State::save()` (which would REFUSE this because
    // pending_removals is empty and nothing was dropped — but here nothing
    // WAS dropped, so a plain save would also succeed). The point of this
    // test is that adding an account via restore never needs a
    // confirmation / never gets refused.
    let restore_target = make_state_with(&["a@e.com", "b@e.com"]);
    save_state_restore(restore_target).expect("restore that only adds accounts must succeed");

    let on_disk = State::load().unwrap();
    let keys: Vec<String> = on_disk
        .accounts
        .iter()
        .map(|a| a.key().to_string())
        .collect();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&"a@e.com".to_string()));
    assert!(keys.contains(&"b@e.com".to_string()));
}

#[test]
fn save_state_restore_authorizes_drops_that_a_plain_save_would_refuse() {
    let _g = ScopedConfigDir::new();
    make_state_with(&["a@e.com", "b@e.com"]).save().unwrap();

    // A restore to a file with fewer accounts must NOT hit save_state_safe's
    // drop-protection refusal — save_state_restore is the explicitly-
    // authorized path (the caller — the Restore… dialog — already confirmed
    // the drop with the user by the time this is called).
    let restore_target = make_state_with(&["a@e.com"]);
    save_state_restore(restore_target).expect("save_state_restore must authorize the drop");

    let on_disk = State::load().unwrap();
    let keys: Vec<String> = on_disk
        .accounts
        .iter()
        .map(|a| a.key().to_string())
        .collect();
    assert_eq!(keys, vec!["a@e.com".to_string()]);
}

#[test]
fn stash_pre_restore_lands_under_config_backups_not_tmp() {
    // H2: the pre-restore stash must never touch /tmp — it must land in
    // config_dir()/backups/pre-restore-<ts>.json, owner-only.
    let g = ScopedConfigDir::new();
    make_state_with(&["a@e.com"]).save().unwrap();

    let dir = config_dir().unwrap();
    let stash_path = stash_pre_restore(&dir)
        .unwrap()
        .expect("live state existed to stash");

    assert!(
        stash_path.starts_with(g.home().join(".config/usagio/backups")),
        "stash must live under config_dir/backups, not /tmp: {}",
        stash_path.display()
    );
    assert!(
        stash_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("pre-restore-"),
        "stash filename: {}",
        stash_path.display()
    );
    // Historical H2 concern: never move OAuth-token-bearing state to /tmp.
    // On Linux CI, the whole ScopedConfigDir tempdir root is under /tmp/xyz/,
    // so `starts_with("/tmp")` is a false positive there (the earlier
    // positive assertion above already pins the correct location under
    // config_dir/backups). Assert what actually matters: the stash is NOT
    // in a shared tmp dir that isn't a subdirectory of config_dir/backups.
    // The positive assertion above (`starts_with(g.home()/.config/usagio/
    // backups)`) is the real invariant; this is belt-and-suspenders in
    // case someone refactors stash_pre_restore to a shared temp path.
    assert!(
        stash_path.starts_with(g.home().join(".config/usagio/backups")),
        "stash must be under config_dir/backups; got: {}",
        stash_path.display()
    );

    // The live state.json is gone (moved), and the stash carries its bytes.
    assert!(!config_dir().unwrap().join("state.json").exists());
    let stashed = std::fs::read_to_string(&stash_path).unwrap();
    assert!(stashed.contains("a@e.com"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&stash_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "pre-restore stash must be owner-only");
    }
}

#[test]
fn stash_pre_restore_is_noop_when_no_live_state() {
    let _g = ScopedConfigDir::new();
    let dir = config_dir().unwrap();
    assert!(stash_pre_restore(&dir).unwrap().is_none());
}

#[test]
fn unstash_pre_restore_rolls_the_live_state_back_exactly() {
    // Audit finding 11: if a restore WRITE fails after stash_pre_restore has
    // already moved state.json aside, unstash_pre_restore must put the original
    // back so state.json is never left MISSING (which State::load would read as
    // zero accounts). Round-trip: stash removes the live file, unstash restores
    // it byte-for-byte, owner-only.
    let _g = ScopedConfigDir::new();
    make_state_with(&["keep@e.com"]).save().unwrap();
    let live = config_dir().unwrap().join("state.json");
    let original = std::fs::read(&live).unwrap();

    let dir = config_dir().unwrap();
    let stash = stash_pre_restore(&dir).unwrap().expect("live state existed");
    assert!(!live.exists(), "stash should have moved the live file aside");

    unstash_pre_restore(&stash).expect("rollback must restore the stashed state");

    assert!(live.exists(), "state.json must exist again after rollback");
    assert_eq!(
        std::fs::read(&live).unwrap(),
        original,
        "rolled-back state.json must match the pre-restore bytes exactly"
    );
    assert!(!stash.exists(), "the stash file should be consumed by the rollback");
    // The recovered accounts load correctly (not the zero-account default).
    assert!(State::load()
        .unwrap()
        .accounts
        .iter()
        .any(|a| a.email.as_deref() == Some("keep@e.com")));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&live).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "rolled-back state.json must stay owner-only");
    }
}

// ---------------------------------------------------------------------------
// M1 — notification_config parse failures must not be silent.
// ---------------------------------------------------------------------------

#[test]
fn corrupt_notification_config_falls_back_to_default_and_logs() {
    let g = ScopedConfigDir::new();

    // Seed state.json with a notification_config shape that can never
    // deserialize (a string where an object/array is expected).
    let dir = config_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let bad = serde_json::json!({
        "accounts": {},
        "active": null,
        "notification_config": "this-is-not-a-valid-config-shape",
    });
    std::fs::write(dir.join("state.json"), serde_json::to_vec(&bad).unwrap()).unwrap();

    // Loading must still succeed with defaulted notification_config.
    let loaded = State::load().expect("load must succeed despite corrupt field");
    assert_eq!(loaded.notification_config, Default::default());

    // A discoverable log line must exist mentioning the failure.
    let log_path = dir.join("usagio.log");
    let contents = std::fs::read_to_string(&log_path)
        .expect("log file should exist after a logged parse failure");
    assert!(
        contents.contains("notification_config"),
        "log should mention notification_config: {contents}"
    );
    assert!(
        contents.to_lowercase().contains("parse"),
        "log should mention parse failure: {contents}"
    );

    drop(g);
}

// ---------------------------------------------------------------------------
// State v2 — per-provider multi-account slot (codex-switch-e2e)
// ---------------------------------------------------------------------------

fn provider_account(key: &str) -> ProviderAccount {
    ProviderAccount {
        key: key.to_string(),
        secret_blob: format!("{{\"tokens\":{{\"access_token\":\"at-{key}\"}}}}"),
        access_token: format!("at-{key}"),
        refresh_token: format!("rt-{key}"),
        expires_at: 1_000,
        identity_email: Some(key.to_string()),
        identity_uuid: None,
        identity_display_name: None,
        identity_native_blob: serde_json::Value::Null,
        cached_usage: None,
        notif_state: Default::default(),
        needs_relogin: false,
    }
}

#[test]
fn v1_state_json_loads_and_upgrades_to_v2() {
    // A real (well, hand-built but shape-accurate) v1 state.json: flat
    // `accounts` list, no `schema_version`, no `providers` map at all. This
    // is the exact fixture `STATE_SCHEMA_VERSION`'s doc comment promises
    // loads with nothing lost.
    let v1 = serde_json::json!({
        "accounts": [
            {
                "email": "dev@getbusbar.com",
                "access_token": "a1",
                "refresh_token": "r1",
                "expires_at": 1_700_000_000_000i64,
                "keychain_blob": "{}",
                "cached_usage": {
                    "session_pct": 42.0,
                    "fetched_at": 1_757_000_000i64
                }
            }
        ],
        "active": "dev@getbusbar.com",
        "autoswap_disabled": true,
        "trigger_pct": 90.0
    });
    let s = State::from_value(&v1);

    // Nothing lost: the Claude account, its tokens, its cached usage, and
    // the policy bits all survive untouched.
    assert_eq!(s.schema_version, STATE_SCHEMA_VERSION);
    assert_eq!(s.accounts.len(), 1);
    let acct = s.find("dev@getbusbar.com").unwrap();
    assert_eq!(acct.access_token, "a1");
    assert_eq!(acct.cached_usage.as_ref().unwrap().session_pct, Some(42.0));
    assert_eq!(s.active.as_deref(), Some("dev@getbusbar.com"));
    assert!(s.autoswap_disabled);
    assert_eq!(s.trigger_pct, Some(90.0));
    // The new v2 bucket is simply empty — no non-Claude accounts existed to
    // migrate.
    assert!(s.providers.is_empty());
}

#[test]
fn v2_state_json_round_trips_provider_accounts() {
    let mut s = State::default();
    s.upsert_provider_account("codex", provider_account("a@example.com"));
    s.upsert_provider_account("codex", provider_account("b@example.com"));
    s.provider_accounts_mut("codex").active = Some("a@example.com".to_string());

    let bytes = serde_json::to_vec(&s).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let reloaded = State::from_value(&v);

    assert_eq!(reloaded.schema_version, STATE_SCHEMA_VERSION);
    let pa = reloaded.provider_accounts("codex").unwrap();
    assert_eq!(pa.accounts.len(), 2);
    assert_eq!(pa.active.as_deref(), Some("a@example.com"));
    assert_eq!(
        reloaded
            .find_provider_account("codex", "b@example.com")
            .unwrap()
            .access_token,
        "at-b@example.com"
    );
}

#[test]
fn upsert_provider_account_replaces_by_key() {
    let mut s = State::default();
    s.upsert_provider_account("codex", provider_account("a@example.com"));
    let mut updated = provider_account("a@example.com");
    updated.access_token = "rotated".to_string();
    s.upsert_provider_account("codex", updated);

    let pa = s.provider_accounts("codex").unwrap();
    assert_eq!(pa.accounts.len(), 1);
    assert_eq!(pa.accounts[0].access_token, "rotated");
}

#[test]
fn remove_provider_account_clears_active_and_authorizes_the_drop() {
    let mut s = State::default();
    s.upsert_provider_account("codex", provider_account("a@example.com"));
    s.provider_accounts_mut("codex").active = Some("a@example.com".to_string());

    assert!(s.remove_provider_account("codex", "a@example.com"));
    assert!(s.provider_accounts("codex").unwrap().accounts.is_empty());
    assert!(s.provider_accounts("codex").unwrap().active.is_none());
    assert!(s
        .pending_provider_removals
        .contains(&("codex".to_string(), "a@example.com".to_string())));
}

#[test]
fn save_state_safe_refuses_dropping_provider_account_without_remove() {
    let _g = ScopedConfigDir::new();
    let mut seed = State::default();
    seed.upsert_provider_account("codex", provider_account("a@example.com"));
    seed.save().expect("seed save");

    // Reload, then silently drop the codex account without calling
    // `remove_provider_account` (no authorization recorded) — must be
    // refused exactly like an unauthorized Claude account drop.
    let mut reloaded = State::load().unwrap();
    reloaded
        .providers
        .get_mut("codex")
        .unwrap()
        .accounts
        .clear();
    let err = reloaded.save().unwrap_err();
    assert!(format!("{err}").contains("provider account"));

    // On-disk state is unchanged.
    let still_there = State::load().unwrap();
    assert!(still_there
        .find_provider_account("codex", "a@example.com")
        .is_some());
}

#[test]
fn save_state_safe_allows_explicit_provider_account_removal() {
    let _g = ScopedConfigDir::new();
    let mut seed = State::default();
    seed.upsert_provider_account("codex", provider_account("a@example.com"));
    seed.save().expect("seed save");

    let mut reloaded = State::load().unwrap();
    assert!(reloaded.remove_provider_account("codex", "a@example.com"));
    reloaded.save().expect("authorized drop must succeed");

    let after = State::load().unwrap();
    assert!(after
        .find_provider_account("codex", "a@example.com")
        .is_none());
}

// --- robustness-05: v1 state.json duplicate-email dedup on load ------------

#[test]
fn from_value_dedups_duplicate_emails_keeping_the_newer_grant() {
    let _g = ScopedConfigDir::new();
    let v = serde_json::json!({
        "accounts": [
            {
                "email": "dup@example.com",
                "access_token": "old-at",
                "refresh_token": "old-rt",
                "expires_at": 1_000i64,
                "keychain_blob": "{}",
            },
            {
                "email": "dup@example.com",
                "access_token": "new-at",
                "refresh_token": "new-rt",
                "expires_at": 2_000i64,
                "keychain_blob": "{}",
            },
        ],
    });
    let s = State::from_value(&v);
    assert_eq!(
        s.accounts.len(),
        1,
        "the duplicate must be dropped, not kept"
    );
    let acct = s.find("dup@example.com").unwrap();
    assert_eq!(acct.access_token, "new-at");
    assert_eq!(acct.expires_at, 2_000);
}

#[test]
fn from_value_dedup_is_case_insensitive_and_order_independent() {
    let _g = ScopedConfigDir::new();
    // Newer entry listed FIRST this time — dedup must not assume ordering.
    let v = serde_json::json!({
        "accounts": [
            {
                "email": "Dup@Example.com",
                "access_token": "new-at",
                "refresh_token": "new-rt",
                "expires_at": 5_000i64,
                "keychain_blob": "{}",
            },
            {
                "email": "dup@example.com",
                "access_token": "old-at",
                "refresh_token": "old-rt",
                "expires_at": 1_000i64,
                "keychain_blob": "{}",
            },
        ],
    });
    let s = State::from_value(&v);
    assert_eq!(s.accounts.len(), 1);
    let acct = s.find("dup@example.com").unwrap();
    assert_eq!(acct.access_token, "new-at");
}

#[test]
fn from_value_keeps_distinct_accounts_untouched() {
    let _g = ScopedConfigDir::new();
    let v = serde_json::json!({
        "accounts": [
            {
                "email": "a@example.com",
                "access_token": "a-at",
                "refresh_token": "a-rt",
                "expires_at": 1_000i64,
                "keychain_blob": "{}",
            },
            {
                "email": "b@example.com",
                "access_token": "b-at",
                "refresh_token": "b-rt",
                "expires_at": 1_000i64,
                "keychain_blob": "{}",
            },
        ],
    });
    let s = State::from_value(&v);
    assert_eq!(s.accounts.len(), 2);
    assert!(s.find("a@example.com").is_some());
    assert!(s.find("b@example.com").is_some());
}
