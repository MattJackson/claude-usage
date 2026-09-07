//! Provider-agnostic credential sync + proactive refresh + last-chance fallback.
//!
//! The goal is "never re-login": if the real vendor CLI (or another usagio
//! process, or the user manually) rotates a credential on disk, we absorb it
//! into our state before our stored copy expires. If a refresh fails with
//! invalid_grant anyway, we re-read every credential path once, adopt the
//! freshest match, and only flag `needs_relogin` if all paths agree the
//! token is dead.
//!
//! Everything here is provider-agnostic — it drives `Provider::credential_paths`,
//! `identify_credential`, `credential_freshness`, and `absorb_credential`, so
//! adding a new provider just means implementing those four methods on its
//! trait impl.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::providers::trait_def::{
    AccountKey, CredentialFreshness, PResult, Provider, ProviderError,
};
use crate::store::{self, State};

/// Refresh a token if it expires within this many seconds. Bumped from the
/// legacy 300s (5 min) to 900s (15 min) so an inactive account's refresh
/// happens well before another `claude` invocation could race us.
pub const REFRESH_SKEW_SECS: i64 = 900;

/// Run `f` holding the shared advisory lock on state.json. Duplicated from
/// `main::with_state_lock` so provider `absorb_credential` implementations
/// can commit without importing `main` (which owns the CLI dispatch tree).
pub(crate) fn with_state_lock_absorb<F>(f: F) -> PResult<()>
where
    F: FnOnce(&mut State) -> Result<()>,
{
    with_state_lock(|| {
        let mut st = State::load()?;
        f(&mut st)?;
        st.save()
    })
    .map_err(|e| ProviderError::Other(format!("state lock: {e:#}")))
}

fn with_state_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    use fs2::FileExt;
    let dir = store::config_dir()?;
    std::fs::create_dir_all(&dir).context("creating ~/.config/claude-usage")?;
    let lock_path = dir.join("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .context("opening state lock")?;
    file.lock_exclusive().context("acquiring state lock")?;
    let r = f();
    // Fully-qualified to fs2's trait: std 1.89 added an inherent unlock() that
    // would otherwise shadow it and break the 1.88 MSRV.
    let _ = fs2::FileExt::unlock(&file);
    r
}

/// Read a credential file into a string, returning None if it doesn't exist
/// (the common case for optional paths like `~/.claude/.credentials.json`).
pub fn read_blob(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    String::from_utf8(bytes).ok()
}

/// Scan a provider's credential paths and let it absorb any lagging
/// rotations. Returns the set of AccountKeys observed on disk (whether we
/// absorbed them or not — the caller can use this to detect additions).
///
/// This is the FREE-SYNC step: even if we're only looking for one account,
/// while we're reading a path we may see a blob for a DIFFERENT tracked
/// account (e.g. `~/.claude.json`'s active identity shifted while we were
/// polling for another). Absorb those too.
pub fn absorb_all_lagging(provider: &dyn Provider) -> Vec<AccountKey> {
    let mut seen = Vec::new();
    for path in provider.credential_paths() {
        let Some(blob) = read_blob(&path) else {
            continue;
        };
        let Some(key) = provider.identify_credential(&blob) else {
            continue;
        };
        // Only absorb if the blob is at least usable — an Invalid blob is
        // most likely a partial write we caught mid-rename.
        let freshness = provider.credential_freshness(&blob);
        if matches!(freshness, CredentialFreshness::Invalid) {
            continue;
        }
        if let Err(e) = provider.absorb_credential(&key, &blob) {
            crate::logging::log(&format!(
                "credentials: absorb of {}:{} from {} failed: {e}",
                key.provider,
                key.key,
                path.display()
            ));
        }
        seen.push(key);
    }
    seen
}

/// Proactively refresh any INACTIVE Claude account whose stored access
/// token is within REFRESH_SKEW_SECS of expiry. The active account is
/// deliberately skipped — let the vendor CLI own its own rotation so our
/// refresh can't race the tokens it's about to write.
pub fn refresh_inactive_if_stale(active_email: Option<&str>) {
    let state = match State::load() {
        Ok(s) => s,
        Err(e) => {
            crate::logging::log(&format!("credentials: state load failed: {e:#}"));
            return;
        }
    };
    let emails: Vec<String> = state
        .accounts
        .iter()
        .filter(|a| !a.needs_relogin)
        .filter(|a| Some(a.key()) != active_email)
        .map(|a| a.key().to_string())
        .collect();
    for email in emails {
        let Some(mut acct) = state.find(&email).cloned() else {
            continue;
        };
        match crate::providers::claude::oauth::ensure_fresh(&mut acct, REFRESH_SKEW_SECS) {
            Ok(true) => {
                let _ = with_state_lock(|| {
                    let mut st = State::load()?;
                    if let Some(a) = st.find_mut(&email) {
                        a.set_tokens_if_newer(
                            acct.access_token.clone(),
                            acct.refresh_token.clone(),
                            acct.expires_at,
                        );
                    }
                    st.save()
                });
            }
            Ok(false) => {}
            Err(crate::providers::claude::oauth::RefreshError::InvalidGrant) => {
                // Before flagging, run the last-chance fallback: another
                // process may have rotated the credential on disk while we
                // held a stale grant.
                let claude = match crate::providers::get("claude") {
                    Some(p) => p,
                    None => continue,
                };
                let key = AccountKey::new("claude", &email);
                if !last_chance_fallback(claude, &key) {
                    let _ = with_state_lock(|| {
                        let mut st = State::load()?;
                        if let Some(a) = st.find_mut(&email) {
                            a.needs_relogin = true;
                        }
                        st.save()
                    });
                }
            }
            Err(e) => {
                crate::logging::log(&format!(
                    "credentials: inactive refresh for {email} failed: {e}"
                ));
            }
        }
    }
}

/// Absorb any lagging rotations for the outgoing account BEFORE the caller
/// writes the incoming account's blob to path[0]. This closes the window
/// where a running `claude` on the outgoing account would silently lose its
/// last rotation because we blindly overwrote path[0] first.
pub fn absorb_before_switch(provider: &dyn Provider) {
    let _ = absorb_all_lagging(provider);
}

/// Last-chance fallback: re-read every credential path, and if any blob
/// identifies as `target` and is fresher (still usable) than what we hold,
/// absorb it and return true. The caller (usually the refresh loop that
/// just saw invalid_grant) can then retry once with the freshly-adopted
/// tokens instead of flagging needs_relogin.
///
/// Bonus: while we're walking, absorb any blob that matches a DIFFERENT
/// tracked account. This is the "free-sync" behaviour: we already have
/// the data in hand, no reason not to update it.
pub fn last_chance_fallback(provider: &dyn Provider, target: &AccountKey) -> bool {
    let mut best_for_target: Option<(String, CredentialFreshness)> = None;
    for path in provider.credential_paths() {
        let Some(blob) = read_blob(&path) else {
            continue;
        };
        let Some(key) = provider.identify_credential(&blob) else {
            continue;
        };
        let freshness = provider.credential_freshness(&blob);
        if key == *target {
            // Track the freshest blob for the target; commit outside the loop.
            let take = match &best_for_target {
                None => true,
                Some((_, cur)) => freshness.rank() > cur.rank(),
            };
            if take {
                best_for_target = Some((blob, freshness));
            }
        } else if freshness.is_usable() {
            // Free sync for other tracked accounts we happened to see.
            let _ = provider.absorb_credential(&key, &blob);
        }
    }
    match best_for_target {
        Some((blob, f)) if f.is_usable() => {
            let _ = provider.absorb_credential(target, &blob);
            true
        }
        _ => false,
    }
}

/// Handle emitted by `spawn_watchers`. Dropping it drops the underlying
/// notify::RecommendedWatcher and stops the background thread.
#[allow(dead_code)]
pub struct WatcherHandle {
    // The watcher must outlive the thread, so we keep it here. The thread
    // owns the receiver end and shuts down when we drop the sender-carrying
    // watcher (the notify crate closes the channel on drop).
    _watcher: notify::RecommendedWatcher,
    _thread: std::thread::JoinHandle<()>,
}

/// Spawn a background thread that fsnotifies every provider's credential
/// paths and calls `absorb_all_lagging` + `refresh_inactive_if_stale` on
/// change. Returns a `WatcherHandle` whose Drop shuts everything down —
/// callers that want the watcher to live for the process lifetime should
/// `Box::leak` or store the handle in a `OnceLock`.
///
/// Errors from watch setup are logged and swallowed: the daemon must not
/// crash if `~/.claude/` doesn't exist yet on a fresh install.
pub fn spawn_watchers(providers: Vec<&'static dyn Provider>) -> Option<WatcherHandle> {
    use notify::{Event, RecursiveMode, Watcher};

    let (tx, rx) = mpsc::channel::<Event>();
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(ev) = res {
            let _ = tx.send(ev);
        }
    })
    .ok()?;

    let mut any_watched = false;
    for p in &providers {
        for path in p.credential_paths() {
            // Watch the parent directory (notify's file-level watching is
            // unreliable across atomic-rename rotations, which is exactly
            // how the vendor CLIs write these files).
            let parent = match path.parent() {
                Some(par) if !par.as_os_str().is_empty() => par.to_path_buf(),
                _ => continue,
            };
            if !parent.exists() {
                continue;
            }
            if watcher.watch(&parent, RecursiveMode::NonRecursive).is_ok() {
                any_watched = true;
            }
        }
    }
    if !any_watched {
        return None;
    }

    let providers_static: Vec<&'static dyn Provider> = providers;
    let thread = std::thread::Builder::new()
        .name("usagio-credentials-watcher".into())
        .spawn(move || {
            // Debounce: coalesce a burst of writes (atomic rename fires
            // multiple events) into one absorb pass.
            let debounce = Duration::from_millis(250);
            loop {
                let Ok(_first) = rx.recv() else {
                    break;
                };
                while rx.recv_timeout(debounce).is_ok() {}
                for p in &providers_static {
                    let _ = absorb_all_lagging(*p);
                }
                let active = State::load().ok().and_then(|s| s.active.clone());
                refresh_inactive_if_stale(active.as_deref());
            }
        })
        .ok()?;

    Some(WatcherHandle {
        _watcher: watcher,
        _thread: thread,
    })
}

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;
