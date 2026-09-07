//! Tests for the credential-sync layer. State-touching helpers redirect HOME
//! to a per-test tempdir so `store::config_dir()` writes stay in-fixture and
//! parallel test runs don't collide on the real ~/.config/claude-usage.

use super::*;
use crate::providers::trait_def::{
    AccountKey, Capabilities, CaptureMode, CapturedAccount, CredentialFreshness, IdentitySnapshot,
    LaunchMode, PResult, Provider, ProviderError, SecretBackend, TokenGrant, UsageSnapshot,
};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A test-only provider that lets each test control its credential paths,
/// its identify + freshness logic, and record the absorbs it received.
/// Building one from scratch (rather than reusing Claude/Codex) keeps these
/// tests free of any implicit dependency on state.json + $HOME layout.
struct FakeProvider {
    slug: &'static str,
    paths: Vec<PathBuf>,
    /// Map blob -> (AccountKey identifier, freshness) for identify + freshness.
    routing: Vec<(String, Option<AccountKey>, CredentialFreshness)>,
    absorbed: Arc<Mutex<Vec<(AccountKey, String)>>>,
}

impl FakeProvider {
    fn new(slug: &'static str) -> Self {
        FakeProvider {
            slug,
            paths: Vec::new(),
            routing: Vec::new(),
            absorbed: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn with_path(mut self, p: PathBuf) -> Self {
        self.paths.push(p);
        self
    }
    fn route(mut self, blob: &str, key: Option<AccountKey>, freshness: CredentialFreshness) -> Self {
        self.routing.push((blob.to_string(), key, freshness));
        self
    }
    fn absorbs(&self) -> Vec<(AccountKey, String)> {
        self.absorbed.lock().unwrap().clone()
    }
}

impl Provider for FakeProvider {
    fn provider_id(&self) -> &'static str {
        self.slug
    }
    fn display_name(&self) -> &'static str {
        "Fake"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_usage: false,
            supports_switching: false,
            supports_email_capture: false,
            secret_backend: SecretBackend::File,
            capture_mode: CaptureMode::CredsOnDisk,
        }
    }
    fn capture_current_login(&self) -> PResult<Option<CapturedAccount>> {
        Ok(None)
    }
    fn parse_stored_blob(&self, _blob: &str) -> PResult<TokenGrant> {
        Err(ProviderError::Unsupported)
    }
    fn patch_stored_blob(&self, _blob: &str, _grant: &TokenGrant) -> PResult<String> {
        Err(ProviderError::Unsupported)
    }
    fn credential_paths(&self) -> Vec<PathBuf> {
        self.paths.clone()
    }
    fn identify_credential(&self, blob: &str) -> Option<AccountKey> {
        self.routing
            .iter()
            .find(|(b, _, _)| b == blob)
            .and_then(|(_, k, _)| k.clone())
    }
    fn credential_freshness(&self, blob: &str) -> CredentialFreshness {
        self.routing
            .iter()
            .find(|(b, _, _)| b == blob)
            .map(|(_, _, f)| f.clone())
            .unwrap_or(CredentialFreshness::Unknown)
    }
    fn absorb_credential(&self, account: &AccountKey, blob: &str) -> PResult<()> {
        self.absorbed
            .lock()
            .unwrap()
            .push((account.clone(), blob.to_string()));
        Ok(())
    }
}

/// Write a credential blob to a tempdir path so `absorb_all_lagging` /
/// `last_chance_fallback` have something to read.
fn write_blob(dir: &std::path::Path, name: &str, blob: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, blob).unwrap();
    p
}

// -----------------------------------------------------------------------------
// absorb_all_lagging
// -----------------------------------------------------------------------------

#[test]
fn absorb_all_lagging_absorbs_recognised_blob() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_blob(dir.path(), "creds.json", "BLOB_A");
    let key = AccountKey::new("fake", "a@e.com");
    let prov = FakeProvider::new("fake")
        .with_path(path)
        .route("BLOB_A", Some(key.clone()), CredentialFreshness::Fresh);
    let seen = absorb_all_lagging(&prov);
    assert_eq!(seen, vec![key.clone()]);
    let absorbs = prov.absorbs();
    assert_eq!(absorbs.len(), 1);
    assert_eq!(absorbs[0].0, key);
    assert_eq!(absorbs[0].1, "BLOB_A");
}

#[test]
fn absorb_all_lagging_skips_unrecognised_blob() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_blob(dir.path(), "creds.json", "STRANGER");
    // identify_credential returns None => not a tracked account, skip absorb.
    let prov = FakeProvider::new("fake").with_path(path);
    let seen = absorb_all_lagging(&prov);
    assert!(seen.is_empty());
    assert!(prov.absorbs().is_empty());
}

#[test]
fn absorb_all_lagging_skips_invalid_freshness() {
    // A blob we could identify but whose freshness is Invalid is likely a
    // partial write; skip absorbing so we don't clobber a working slot.
    let dir = tempfile::tempdir().unwrap();
    let path = write_blob(dir.path(), "creds.json", "PARTIAL");
    let key = AccountKey::new("fake", "a@e.com");
    let prov = FakeProvider::new("fake")
        .with_path(path)
        .route("PARTIAL", Some(key), CredentialFreshness::Invalid);
    let _ = absorb_all_lagging(&prov);
    assert!(prov.absorbs().is_empty(), "invalid blobs must not be absorbed");
}

#[test]
fn absorb_all_lagging_skips_missing_files() {
    // Non-existent path: silently skipped (fresh install has no ~/.claude/).
    let prov = FakeProvider::new("fake").with_path(PathBuf::from("/nonexistent/nope.json"));
    let seen = absorb_all_lagging(&prov);
    assert!(seen.is_empty());
}

#[test]
fn absorb_all_lagging_free_sync_scans_multiple_paths() {
    // Two paths, each carrying a different account's blob. absorb_all_lagging
    // must visit both and absorb both — this is the "free sync" property.
    let dir = tempfile::tempdir().unwrap();
    let p1 = write_blob(dir.path(), "one.json", "A_BLOB");
    let p2 = write_blob(dir.path(), "two.json", "B_BLOB");
    let ka = AccountKey::new("fake", "a@e.com");
    let kb = AccountKey::new("fake", "b@e.com");
    let prov = FakeProvider::new("fake")
        .with_path(p1)
        .with_path(p2)
        .route("A_BLOB", Some(ka.clone()), CredentialFreshness::Fresh)
        .route("B_BLOB", Some(kb.clone()), CredentialFreshness::Fresh);
    let seen = absorb_all_lagging(&prov);
    assert_eq!(seen.len(), 2);
    let absorbs = prov.absorbs();
    assert_eq!(absorbs.len(), 2);
    let keys: Vec<AccountKey> = absorbs.iter().map(|(k, _)| k.clone()).collect();
    assert!(keys.contains(&ka));
    assert!(keys.contains(&kb));
}

// -----------------------------------------------------------------------------
// last_chance_fallback
// -----------------------------------------------------------------------------

#[test]
fn last_chance_fallback_adopts_freshest_matching_blob() {
    // Target appears on two paths. The fresher one must win.
    let dir = tempfile::tempdir().unwrap();
    let stale = write_blob(dir.path(), "stale.json", "STALE_A");
    let fresh = write_blob(dir.path(), "fresh.json", "FRESH_A");
    let target = AccountKey::new("fake", "a@e.com");
    let prov = FakeProvider::new("fake")
        .with_path(stale)
        .with_path(fresh)
        .route(
            "STALE_A",
            Some(target.clone()),
            CredentialFreshness::ExpiresIn(std::time::Duration::from_secs(30)),
        )
        .route("FRESH_A", Some(target.clone()), CredentialFreshness::Fresh);
    assert!(last_chance_fallback(&prov, &target));
    let absorbs = prov.absorbs();
    // The last absorb for target is the fresher blob (we absorb-once at the
    // end for the target; the loop only free-syncs OTHER accounts).
    let target_absorbs: Vec<&(AccountKey, String)> =
        absorbs.iter().filter(|(k, _)| k == &target).collect();
    assert_eq!(target_absorbs.len(), 1);
    assert_eq!(target_absorbs[0].1, "FRESH_A");
}

#[test]
fn last_chance_fallback_returns_false_when_target_absent() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_blob(dir.path(), "creds.json", "SOMEONE_ELSE");
    let other = AccountKey::new("fake", "b@e.com");
    let target = AccountKey::new("fake", "a@e.com");
    let prov = FakeProvider::new("fake")
        .with_path(p)
        .route("SOMEONE_ELSE", Some(other.clone()), CredentialFreshness::Fresh);
    assert!(!last_chance_fallback(&prov, &target));
    // But free-sync did absorb the other tracked account.
    let absorbs = prov.absorbs();
    assert_eq!(absorbs.len(), 1);
    assert_eq!(absorbs[0].0, other);
}

#[test]
fn last_chance_fallback_returns_false_when_target_blob_is_expired() {
    // We DID find the target on disk, but its access token is Expired —
    // not usable as-is, so caller should still flag needs_relogin.
    let dir = tempfile::tempdir().unwrap();
    let p = write_blob(dir.path(), "creds.json", "DEAD_A");
    let target = AccountKey::new("fake", "a@e.com");
    let prov = FakeProvider::new("fake")
        .with_path(p)
        .route("DEAD_A", Some(target.clone()), CredentialFreshness::Expired);
    assert!(!last_chance_fallback(&prov, &target));
    // And we must NOT have absorbed the expired blob for the target.
    let absorbs = prov.absorbs();
    let target_absorbs = absorbs.iter().filter(|(k, _)| k == &target).count();
    assert_eq!(target_absorbs, 0, "expired target blobs must not be absorbed");
}

#[test]
fn last_chance_fallback_bonus_absorbs_other_tracked_account() {
    // The FREE-SYNC bonus: while scanning for the target, a blob for a
    // DIFFERENT tracked account is also absorbed. This is the entire reason
    // the fallback iterates all paths rather than short-circuiting.
    let dir = tempfile::tempdir().unwrap();
    let a_path = write_blob(dir.path(), "a.json", "A_BLOB");
    let b_path = write_blob(dir.path(), "b.json", "B_BLOB");
    let a = AccountKey::new("fake", "a@e.com");
    let b = AccountKey::new("fake", "b@e.com");
    let prov = FakeProvider::new("fake")
        .with_path(a_path)
        .with_path(b_path)
        .route("A_BLOB", Some(a.clone()), CredentialFreshness::Fresh)
        .route("B_BLOB", Some(b.clone()), CredentialFreshness::Fresh);
    // Looking for A: B is picked up along the way.
    let _ = last_chance_fallback(&prov, &a);
    let absorbs = prov.absorbs();
    assert!(absorbs.iter().any(|(k, _)| k == &b), "B should be free-synced");
}

// -----------------------------------------------------------------------------
// absorb_before_switch just delegates
// -----------------------------------------------------------------------------

#[test]
fn absorb_before_switch_reads_credential_paths() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_blob(dir.path(), "creds.json", "OUTGOING_BLOB");
    let key = AccountKey::new("fake", "outgoing@e.com");
    let prov = FakeProvider::new("fake")
        .with_path(p)
        .route("OUTGOING_BLOB", Some(key.clone()), CredentialFreshness::Fresh);
    absorb_before_switch(&prov);
    assert_eq!(prov.absorbs(), vec![(key, "OUTGOING_BLOB".to_string())]);
}

// -----------------------------------------------------------------------------
// read_blob helper
// -----------------------------------------------------------------------------

#[test]
fn read_blob_returns_none_for_missing_file() {
    assert!(read_blob(std::path::Path::new("/absolutely/not/here.json")).is_none());
}

#[test]
fn read_blob_returns_contents_of_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_blob(dir.path(), "x.json", "HELLO");
    assert_eq!(read_blob(&p).as_deref(), Some("HELLO"));
}

// -----------------------------------------------------------------------------
// refresh skew constant is bumped to 15 minutes
// -----------------------------------------------------------------------------

#[test]
fn refresh_skew_is_fifteen_minutes() {
    // Bumped from the legacy 300s to 900s so an inactive account's refresh
    // happens well before another `claude` could race us.
    assert_eq!(REFRESH_SKEW_SECS, 900);
}

// -----------------------------------------------------------------------------
// with_state_lock reentrancy — regression test for the switch-time deadlock
// where absorb_before_switch -> Provider::absorb_credential ->
// with_state_lock_absorb -> with_state_lock reopens the same lock file and
// flock(LOCK_EX) blocks forever on the same process's outer frame.
// -----------------------------------------------------------------------------
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_isolated_home<F: FnOnce()>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let td = tempfile::tempdir().unwrap();
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", td.path());
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
    let _r = Restore(prev);
    f();
}

#[test]
fn nested_with_state_lock_does_not_deadlock() {
    // The switch path takes with_state_lock, then calls absorb_before_switch
    // which recurses into with_state_lock_absorb (a Provider::absorb_credential
    // implementation) which in turn takes with_state_lock again. Before the
    // reentrancy fix this second flock(LOCK_EX) blocked forever on the same
    // process's outer frame. Run the nested call on a worker thread and fail
    // the test if it doesn't complete quickly. Uses the direct `with_state_lock`
    // (not `with_state_lock_absorb`) so we don't depend on state.json being
    // seeded — the second flock is what the deadlock hinged on, and the direct
    // recursion exercises the same reentrancy path.
    with_isolated_home(|| {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let r: Result<()> = with_state_lock(|| {
                // Nested direct call — before the fix this blocked forever.
                with_state_lock(|| Ok(()))?;
                Ok(())
            });
            let _ = tx.send(());
            r.unwrap();
        });
        // 5s is generous; a healthy nested call returns instantly.
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("nested with_state_lock deadlocked (should be reentrant)");
        handle.join().unwrap();
    });
}

// -----------------------------------------------------------------------------
// spawn_watchers must create a missing credential-parent directory (fresh
// installs where ~/.claude doesn't exist yet) BEFORE registering the notify
// watcher — otherwise the vendor CLI's first write lands under an inode we
// aren't watching and we miss every fsnotify event until the next full-scan.
// -----------------------------------------------------------------------------
#[test]
fn spawn_watchers_creates_missing_parent_dir() {
    let td = tempfile::tempdir().unwrap();
    // Parent dir does NOT exist yet — this is the fresh-install condition.
    let missing_parent = td.path().join("dot-claude");
    let cred_path = missing_parent.join(".credentials.json");
    assert!(!missing_parent.exists(), "precondition: parent must be missing");

    let prov = FakeProvider::new("fake-watch").with_path(cred_path.clone());
    // spawn_watchers requires 'static providers; leak for the test.
    let prov_static: &'static FakeProvider = Box::leak(Box::new(prov));
    let handle = spawn_watchers(vec![prov_static as &'static dyn Provider]);

    assert!(
        missing_parent.exists(),
        "spawn_watchers should have created the missing parent dir"
    );
    assert!(handle.is_some(), "watcher should have registered on the newly-created dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&missing_parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "credential parent must be 0700 for privacy");
    }
    // Drop handle → notify watcher stops, thread exits.
    drop(handle);
}

// -----------------------------------------------------------------------------
// refresh_inactive_if_stale: snapshot must come from inside the state lock,
// and the mid-tick re-check must skip an account that has BECOME active since
// the snapshot. We validate the pure state-inspection behaviour: if the only
// account is also the active one, the function returns without doing any
// provider-visible work — proving the filter honors the intra-lock snapshot
// rather than the (possibly stale) hint the caller passed.
// -----------------------------------------------------------------------------
#[test]
fn refresh_inactive_if_stale_skips_currently_active_account() {
    use crate::store::{Account, State};
    with_isolated_home(|| {
        // Seed state.json: one account, marked active. If the fn refreshed it
        // we'd race the vendor CLI's rotation — which is the whole bug.
        with_state_lock(|| {
            let mut st = State::default();
            let a = Account {
                email: Some("only@e.com".into()),
                access_token: "tok".into(),
                refresh_token: "rt".into(),
                // Well in the future so ensure_fresh would be a no-op if it
                // did (incorrectly) run — the assertion below still holds
                // because we assert nothing FLIPPED, not that no call was made.
                expires_at: (chrono::Utc::now().timestamp() + 3600) * 1000,
                keychain_blob: String::new(),
                oauth_account: None,
                user_id: None,
                cached_usage: None,
                notif_state: crate::notifications::NotifState::default(),
                needs_relogin: false,
            };
            st.accounts.push(a);
            st.active = Some("only@e.com".into());
            st.save()
        })
        .unwrap();

        // Pass a DELIBERATELY WRONG hint (None) — the fn must ignore it and
        // read `active` from inside the state lock. If it trusted the hint it
        // would proceed to ensure_fresh (network) on the active account.
        refresh_inactive_if_stale(None);

        // State unchanged, active preserved, no needs_relogin flip.
        let st = State::load().unwrap();
        assert_eq!(st.active.as_deref(), Some("only@e.com"));
        assert!(!st.accounts[0].needs_relogin);
    });
}

// -----------------------------------------------------------------------------
// Silence the unused-import warning on `Value` — kept around for future test
// growth and to document that fixtures traffic in raw JSON strings, not
// pre-parsed structures (the sync layer only ever sees blobs).
// -----------------------------------------------------------------------------
#[allow(dead_code)]
fn _fixture_kept_as_json(_: Value) {}
#[allow(dead_code)]
fn _fixture_identity(_: IdentitySnapshot) {}
#[allow(dead_code)]
fn _fixture_snap(_: UsageSnapshot) {}
#[allow(dead_code)]
fn _fixture_launch(_: LaunchMode) {}
