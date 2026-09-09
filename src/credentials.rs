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

use std::cell::Cell;
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

/// Run `f` holding the shared advisory lock on state.json. Provider
/// `absorb_credential` implementations use this to commit without importing
/// `main` (which owns the CLI dispatch tree). Reentrant on the current
/// thread (see `with_state_lock`), so it's safe to call from inside another
/// `with_state_lock` critical section.
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

thread_local! {
    /// Depth of nested `with_state_lock` frames on the current thread. The
    /// outermost frame owns the actual fs2 advisory lock; nested frames
    /// short-circuit and just run the closure. Without this a call chain like
    /// `switch_to_guarded` -> `absorb_before_switch` ->
    /// `Provider::absorb_credential` -> `with_state_lock_absorb` -> `with_state_lock`
    /// would `open()` the lock file a second time in the same process and
    /// `flock(LOCK_EX)` on that fresh open file description, which blocks
    /// forever on both macOS and Linux (per-open-fd semantics).
    static STATE_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII guard that decrements `STATE_LOCK_DEPTH` on drop. L1 (round-1
/// codeaudit): with the raw Cell mutation, an unwind between increment and
/// decrement would leave depth stuck at 1+, and every subsequent
/// `with_state_lock` on this thread would silently skip the actual OS lock.
struct DepthGuard;
impl Drop for DepthGuard {
    fn drop(&mut self) {
        STATE_LOCK_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Run `f` holding the shared exclusive advisory lock on the state file.
/// Serialises state read-modify-write across processes; reentrant within a
/// single thread so callers can nest without self-deadlocking.
pub fn with_state_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    use fs2::FileExt;
    // Reentrant fast path: we already hold the OS lock on this thread; just
    // run the closure. The outer frame will unlock when it unwinds.
    if STATE_LOCK_DEPTH.with(|d| d.get()) > 0 {
        STATE_LOCK_DEPTH.with(|d| d.set(d.get() + 1));
        let _g = DepthGuard;
        return f();
    }
    let dir = store::config_dir()?;
    std::fs::create_dir_all(&dir).context("creating ~/.config/usagio")?;
    let lock_path = dir.join("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .context("opening state lock")?;
    file.lock_exclusive().context("acquiring state lock")?;
    STATE_LOCK_DEPTH.with(|d| d.set(1));
    let _g = DepthGuard;
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

/// Shared credential-path-walk skeleton: read each of `provider`'s
/// credential paths, identify the account (if any) it holds, and compute its
/// freshness — then hand the result to `f`. Eliminates the
/// path→read_blob→identify_credential→freshness duplication that used to be
/// copy-pasted between `absorb_all_lagging` and `last_chance_fallback`
/// (simplification-03, v0.5.2 audit).
///
/// `identified` is `None` when the path doesn't exist / isn't readable UTF-8
/// (silently skipped — the common case for optional paths), or `Some((key,
/// freshness))` when `identify_credential` recognised the blob. Note this
/// does NOT distinguish "unreadable" from "readable but unrecognised" — a
/// caller that needs that distinction (e.g. to log unrecognised blobs, as
/// `last_chance_fallback` does) should check `read_blob` itself rather than
/// use this helper for that path. Both current callers only need the
/// "recognised or not" split, which `identified` already gives them.
fn for_each_credential_path<F>(provider: &dyn Provider, mut f: F)
where
    F: FnMut(&Path, &str, Option<(AccountKey, CredentialFreshness)>),
{
    for path in provider.credential_paths() {
        let Some(blob) = read_blob(&path) else {
            continue;
        };
        let identified = provider
            .identify_credential(&blob)
            .map(|key| (key.clone(), provider.credential_freshness(&blob)));
        f(&path, &blob, identified);
    }
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
    for_each_credential_path(provider, |path, blob, identified| {
        let Some((key, freshness)) = identified else {
            return;
        };
        // Only absorb if the blob is at least usable — an Invalid blob is
        // most likely a partial write we caught mid-rename.
        if matches!(freshness, CredentialFreshness::Invalid) {
            return;
        }
        if let Err(e) = provider.absorb_credential(&key, blob) {
            crate::logging::log(&format!(
                "credentials: absorb of {}:{} from {} failed: {e}",
                key.provider,
                key.key,
                path.display()
            ));
        }
        seen.push(key);
    });
    seen
}

/// Proactively refresh any INACTIVE Claude account whose stored access
/// token is within REFRESH_SKEW_SECS of expiry. The active account is
/// deliberately skipped — let the vendor CLI own its own rotation so our
/// refresh can't race the tokens it's about to write.
pub fn refresh_inactive_if_stale(_active_email_hint: Option<&str>) {
    // Snapshot {accounts, active} atomically under the state lock, ONCE.
    // Using the hint the caller computed outside the lock is unsafe: a
    // switch that lands between the caller's read and this function's
    // iteration could make the "inactive" list include what is now the
    // active account, which we'd then proactively refresh — reintroducing
    // the race with `claude`'s own rotation that this whole path is
    // designed to avoid. That's still true; the fix below is just to stop
    // re-reading and re-parsing the WHOLE state.json file twice per
    // inactive account when this one snapshot already has everything
    // needed to decide whether to refresh (efficiency-01, v0.5.1 audit).
    // Re-checking "is this still inactive" against a fresher on-disk state
    // only matters again once we're about to WRITE — a stale in-memory
    // snapshot can't corrupt anything by being read from, only by being
    // written back over a newer value, and the write path below already
    // re-loads and re-checks `active` under the lock immediately before
    // saving.
    let (accounts, active_at_snapshot): (Vec<crate::store::Account>, Option<String>) =
        match with_state_lock(|| {
            let st = State::load()?;
            let active = st.active.clone();
            let accounts = st
                .accounts
                .iter()
                .filter(|a| !a.needs_relogin)
                .filter(|a| Some(a.key()) != active.as_deref())
                .cloned()
                .collect();
            Ok((accounts, active))
        }) {
            Ok(t) => t,
            Err(e) => {
                crate::logging::log(&format!("credentials: snapshot failed: {e:#}"));
                return;
            }
        };
    for mut acct in accounts {
        let email = acct.key().to_string();
        // The snapshot's `active` is authoritative enough to skip on: if a
        // switch promoted this account after the snapshot, the write-back
        // below re-checks `active` under the lock immediately before saving
        // and no-ops if so — so a stale skip decision here can only cause a
        // harmless extra refresh attempt, never a clobbered write.
        if active_at_snapshot.as_deref() == Some(email.as_str()) {
            continue;
        }
        match crate::providers::claude::oauth::ensure_fresh(&mut acct, REFRESH_SKEW_SECS) {
            Ok(true) => {
                let save_result = with_state_lock(|| {
                    let mut st = State::load()?;
                    // Belt-and-braces: don't clobber tokens for what is now
                    // the active account (a switch may have completed while
                    // we were doing the network refresh).
                    if st.active.as_deref() == Some(email.as_str()) {
                        return Ok(());
                    }
                    if let Some(a) = st.find_mut(&email) {
                        a.set_tokens_if_newer(
                            acct.access_token.clone(),
                            acct.refresh_token.clone(),
                            acct.expires_at,
                        );
                    }
                    st.save()
                });
                // Surface persist failures (state.json write refused / lock
                // poisoned / disk full). Same posture as the sibling
                // InvalidGrant branch below — a silent `let _ =` here was
                // exactly the pattern the earlier R2-EH-01 fix targeted.
                if let Err(e) = save_result {
                    crate::logging::log(&format!(
                        "refresh_inactive_if_stale: post-refresh state save \
                         failed for {email}: {e:#}"
                    ));
                }
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
                    crate::logging::log(&format!(
                        "event=needs_relogin account={email} reason=invalid_grant"
                    ));
                    // R2-EH-01 (round-2 codeaudit): mirror flag_needs_relogin's
                    // logging on save-Err so a state.json write failure here is
                    // visible instead of being silently discarded.
                    if let Err(e) = with_state_lock(|| {
                        let mut st = State::load()?;
                        if let Some(a) = st.find_mut(&email) {
                            a.needs_relogin = true;
                        }
                        st.save()
                    }) {
                        crate::logging::log(&format!(
                            "credentials: flag_needs_relogin({email}): save failed: {e:#}"
                        ));
                    }
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
    for_each_credential_path(provider, |path, blob, identified| {
        let Some((key, freshness)) = identified else {
            // L6 (round-1 codeaudit): log unrecognised blobs so a mismatched
            // rotation shape (e.g. vendor CLI schema change) is visible.
            crate::logging::log(&format!(
                "credentials: last_chance_fallback: unrecognised blob at {}",
                path.display()
            ));
            return;
        };
        if key == *target {
            // Track the freshest blob for the target; commit outside the loop.
            let take = match &best_for_target {
                None => true,
                Some((_, cur)) => freshness.rank() > cur.rank(),
            };
            if take {
                best_for_target = Some((blob.to_string(), freshness));
            }
        } else if freshness.is_usable() {
            // Free sync for other tracked accounts we happened to see.
            // M3 (round-1 codeaudit): don't silently drop the Err — log at
            // least the account key + error so a persistent write failure
            // is visible in the daemon log.
            if let Err(e) = provider.absorb_credential(&key, blob) {
                crate::logging::log(&format!(
                    "credentials: free-sync absorb of {}:{} failed: {e}",
                    key.provider, key.key
                ));
            }
        }
    });
    match best_for_target {
        Some((blob, f)) if f.is_usable() => {
            if let Err(e) = provider.absorb_credential(target, &blob) {
                crate::logging::log(&format!(
                    "credentials: last_chance_fallback commit for {}:{} failed: {e}",
                    target.provider, target.key
                ));
                return false;
            }
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
    let watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        match res {
            Ok(ev) => {
                let _ = tx.send(ev);
            }
            // R2-EH-03: log mid-stream notify errors so persistent notify
            // failures on a credential-parent path leave a signal instead of
            // manifesting as "no events ever fire".
            Err(e) => {
                crate::logging::log(&format!("credentials: notify event error: {e}"));
            }
        }
    });
    // R3-EH-01: log watcher-construction failure (previously .ok()? silently
    // dropped the error, contradicting the doc-comment's "logged and swallowed").
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            crate::logging::log(&format!(
                "credentials: recommended_watcher construction failed: {e}"
            ));
            return None;
        }
    };

    // Refuse to register a watcher on a directory this broad — it turns the
    // NonRecursive-on-macOS-emulated watch into a file-descriptor firehose
    // (~/.config/gcloud/logs/*, image caches, etc. all get FDs held open),
    // maxing out RLIMIT_NOFILE (256 on stock macOS) within days. Any
    // credential file whose parent lands here (e.g. `~/.claude.json` under
    // `$HOME`) is picked up by the periodic `absorb_all_lagging` poll every
    // watch cycle instead. Latency goes from ~2s to ~30s for that one file;
    // capture still works.
    let home_dir = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let root_dir = std::path::PathBuf::from("/");

    let mut any_watched = false;
    let mut watched_parents: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    for p in &providers {
        for path in p.credential_paths() {
            // Watch the parent directory (notify's file-level watching is
            // unreliable across atomic-rename rotations, which is exactly
            // how the vendor CLIs write these files).
            let parent = match path.parent() {
                Some(par) if !par.as_os_str().is_empty() => par.to_path_buf(),
                _ => continue,
            };
            // Skip $HOME and /: too broad, would blow past RLIMIT_NOFILE.
            // Skip duplicate parents so `~/.claude/.credentials.json` and any
            // sibling credential file only register one watch each.
            if Some(&parent) == home_dir.as_ref() || parent == root_dir {
                crate::logging::log(&format!(
                    "credentials: skipping fsnotify watch on {} (too broad); \
                     relying on periodic absorb_all_lagging poll for {}",
                    parent.display(),
                    path.display()
                ));
                continue;
            }
            if !watched_parents.insert(parent.clone()) {
                continue;
            }
            // Fresh install: ~/.claude may not exist yet at daemon startup.
            // Create it (0700 on Unix) BEFORE registering the watcher so the
            // vendor CLI's first write lands under an inode we're already
            // watching — otherwise we'd miss every fsnotify event until the
            // next full-scan tick.
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(&parent) {
                    crate::logging::log(&format!(
                        "credentials: mkdir {} failed: {e}",
                        parent.display()
                    ));
                    continue;
                }
                // Best-effort, no-op on Windows — see `Platform::secure_permissions`.
                let _ = crate::platform::current().secure_permissions(&parent);
            }
            // R2-EH-02: log watch failures (EMFILE/ENOSPC/permission/unsupported
            // FS) so a silent watch drop doesn't degrade us to the 150s poll
            // cadence with no diagnostic.
            match watcher.watch(&parent, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    any_watched = true;
                }
                Err(e) => {
                    crate::logging::log(&format!(
                        "credentials: watch({}) failed: {e}",
                        parent.display()
                    ));
                }
            }
        }
    }
    if !any_watched {
        return None;
    }

    let providers_static: Vec<&'static dyn Provider> = providers;
    let thread = std::thread::Builder::new()
        .name("usagio-credentials-watcher".into())
        .spawn(move || fsnotify_watcher_supervisor(rx, providers_static));
    // R3-EH-01: log watcher-thread spawn failure (previously .ok()? silently
    // dropped ENOMEM/EAGAIN/RLIMIT_NPROC without a signal).
    let thread = match thread {
        Ok(t) => t,
        Err(e) => {
            crate::logging::log(&format!("credentials: watcher thread spawn failed: {e}"));
            return None;
        }
    };

    Some(WatcherHandle {
        _watcher: watcher,
        _thread: thread,
    })
}

/// Process-global registry of the live fsnotify watcher plus the provider
/// list it was built from, so the fd-watchdog (see `crate::watchdog`) can drop
/// and rebuild the watcher at runtime to release leaked descriptors.
///
/// Historically the watcher was built once in `main` and `Box::leak`'d, which
/// made it impossible to release its descriptors without exiting the process —
/// exactly the dead end the fd-leak outage hit (the since-fixed kqueue backend
/// held thousands of fds with no way to reclaim them short of a restart). The
/// FSEvents backend fixed the leak's root cause; storing the handle here adds a
/// runtime safety net so any FUTURE descriptor leak in the watcher can be
/// remediated by `respawn_watchers()` without a full process restart.
struct WatcherRegistry {
    handle: Option<WatcherHandle>,
    providers: Vec<&'static dyn Provider>,
}

static WATCHER_REGISTRY: std::sync::Mutex<WatcherRegistry> =
    std::sync::Mutex::new(WatcherRegistry {
        handle: None,
        providers: Vec::new(),
    });

/// Build the fsnotify watcher for `providers` and store it in the process-global
/// registry (replacing the `Box::leak` the daemon used to do). Keeps the
/// provider list around so the watchdog can rebuild the watcher later without
/// re-threading it through from `main`.
pub fn install_watchers(providers: Vec<&'static dyn Provider>) {
    let handle = spawn_watchers(providers.clone());
    let mut reg = WATCHER_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    reg.providers = providers;
    reg.handle = handle;
}

/// Drop the live fsnotify watcher (releasing every descriptor it holds — the
/// historical fd sink) and build a fresh one from the stored provider list.
/// Called by the fd-watchdog as its first remediation step when the process's
/// open-descriptor count goes critical. Best-effort: if the rebuild fails the
/// registry is simply left without a watcher (logged by `spawn_watchers`), and
/// the periodic `absorb_all_lagging` poll still keeps credentials in sync.
///
/// No-op (returns `false`) if no providers were ever installed (e.g. a one-shot
/// CLI command that never called `install_watchers`).
pub fn respawn_watchers() -> bool {
    let mut reg = WATCHER_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if reg.providers.is_empty() {
        return false;
    }
    // Drop the old handle FIRST so its descriptors are released before we open
    // the new watcher — otherwise we'd transiently double the watcher's fd
    // footprint at the exact moment we're trying to shed descriptors.
    reg.handle = None;
    let providers = reg.providers.clone();
    reg.handle = spawn_watchers(providers);
    true
}

/// Backoff after a caught panic in the fsnotify watcher's processing loop,
/// before we resume listening on `rx`. Mirrors the poll loop's supervisor
/// backoff (errors-03, v0.5.2 audit): short enough that real-time credential
/// absorption isn't meaningfully delayed, long enough that a panic which
/// recurs on every event (e.g. a provider bug triggered by a specific blob
/// shape) doesn't spin the CPU logging the same panic thousands of times a
/// second.
const WATCHER_PANIC_BACKOFF: Duration = Duration::from_millis(500);

/// Runs for the lifetime of the fsnotify watcher thread. Blocks on `rx` for
/// the next raw filesystem event, then hands the debounce+absorb+refresh work
/// to a `catch_unwind`-guarded closure so a panic anywhere in a provider's
/// `identify_credential`/`credential_freshness`/`absorb_credential` (or in
/// `refresh_inactive_if_stale`) can't silently kill the whole watcher thread.
/// Without this, a single panicking event permanently degrades real-time
/// credential absorption to the slow (150s) poll path for the rest of the
/// process's life, with no diagnostic (errors-03, v0.5.2 audit).
fn fsnotify_watcher_supervisor(
    rx: mpsc::Receiver<notify::Event>,
    providers: Vec<&'static dyn Provider>,
) {
    // Debounce: coalesce a burst of writes (atomic rename fires multiple
    // events) into one absorb pass.
    let debounce = Duration::from_millis(250);
    loop {
        // Block for the next event outside the panic guard — recv() itself
        // can't meaningfully panic, and keeping it outside means a caught
        // panic below doesn't have to re-enter catch_unwind just to wait.
        if rx.recv().is_err() {
            // Sender (the notify watcher) dropped — WatcherHandle was
            // dropped, shutting down intentionally. Exit quietly.
            return;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            while rx.recv_timeout(debounce).is_ok() {}
            for p in &providers {
                let _ = absorb_all_lagging(*p);
            }
            // efficiency-1 (v0.5.2 audit): `refresh_inactive_if_stale`
            // ignores its hint parameter entirely (it snapshots {accounts,
            // active} itself under the state lock — see that function's
            // doc), so the `State::load()` that used to sit here was a
            // second, fully redundant disk read + JSON parse on every single
            // debounced credential-file-change event, forever, for zero
            // effect on behavior.
            refresh_inactive_if_stale(None);
        }));
        if let Err(payload) = result {
            let reason = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            crate::logging::log(&format!(
                "event=fsnotify_watcher_panic reason={reason} ; respawning"
            ));
            std::thread::sleep(WATCHER_PANIC_BACKOFF);
            // Loop back to rx.recv(): the channel and watcher are untouched
            // by the panic (they live in the caller's stack frame / the
            // WatcherHandle respectively), so simply resuming the read loop
            // is equivalent to "respawning" the processing without losing
            // any state the thread itself doesn't own.
        }
    }
}

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;
