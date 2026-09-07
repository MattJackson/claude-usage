//! Persistent, owner-only token store for all captured accounts.
//!
//! Accounts are keyed by their Claude account **email** — the stable identity
//! Claude Code itself uses. Each account keeps the *exact* keychain blob
//! captured from a real `claude` login (`{"claudeAiOauth":{...}}`), so writing
//! it back on a switch always produces a login Claude Code accepts. The
//! access/refresh tokens are also mirrored as plain fields for API calls, and
//! patched back into the blob whenever we refresh.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Per-thread config-dir override (used by tests to redirect state.json into a
// tempdir without racing against sibling threads mutating the process HOME).
// ---------------------------------------------------------------------------

thread_local! {
    /// Thread-local override for the "$HOME" root that `config_dir()` uses.
    /// When `Some(p)`, every `config_dir()` call on THIS thread resolves to
    /// `p/.config/<APP_SLUG>` instead of consulting the platform Paths
    /// backend (which would otherwise read the process-wide `HOME`).
    ///
    /// Two properties matter:
    ///
    /// 1. **Thread-local** — so parallel tests can hold different overrides
    ///    without racing each other or the process's real `HOME`.
    /// 2. **Explicit** — every unit test MUST install one (`ScopedConfigDir`
    ///    in-crate, or `TestConfigDir` in `tests/common/mod.rs`). When
    ///    compiled with `cfg(test)`, `config_dir()` panics if no override
    ///    is set, so a stray test that forgets the guard can never write
    ///    to the developer's real `~/.config/usagio/state.json`.
    static HOME_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Current thread's `HOME_OVERRIDE`, or `None` if unset.
pub(crate) fn home_override() -> Option<PathBuf> {
    HOME_OVERRIDE.with(|c| c.borrow().clone())
}

/// Install (or clear) the current thread's `HOME_OVERRIDE`. Public within the
/// crate so tests in any module can build their own guard on top of it —
/// `ScopedConfigDir` (in `store_tests`) is currently the only caller, but
/// keeping the helper `pub(crate)` avoids a new caller having to duplicate
/// the thread-local API on top of it.
#[allow(dead_code)]
pub(crate) fn set_home_override(p: Option<PathBuf>) {
    HOME_OVERRIDE.with(|c| *c.borrow_mut() = p);
}

/// The last usage snapshot fetched for an account. Written only by the
/// scheduler's poll; read (never fetched) by `list`, `switch`, and the menu bar
/// so ad-hoc commands never hit the usage API (and never trigger HTTP 429).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedUsage {
    #[serde(default)]
    pub session_pct: Option<f64>,
    #[serde(default)]
    pub weekly_pct: Option<f64>,
    #[serde(default)]
    pub session_reset: Option<String>,
    #[serde(default)]
    pub weekly_reset: Option<String>,
    #[serde(default)]
    pub opus_pct: Option<f64>,
    #[serde(default)]
    pub opus_reset: Option<String>,
    /// Unix epoch seconds when this snapshot was fetched.
    pub fetched_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// The account's email — the identity key. Always set for a captured
    /// account; `None` only transiently while building one from a keychain blob
    /// before its email is resolved.
    #[serde(default)]
    pub email: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix epoch millis when the access token expires.
    pub expires_at: i64,
    /// The verbatim keychain value: `{"claudeAiOauth":{...}}`.
    pub keychain_blob: String,
    /// The `oauthAccount` object from `~/.claude.json` at capture time. This is
    /// the identity Claude Code actually uses for the active account, so a switch
    /// must restore it alongside the keychain token.
    #[serde(default)]
    pub oauth_account: Option<serde_json::Value>,
    /// The `userID` from `~/.claude.json` at capture time.
    #[serde(default)]
    pub user_id: Option<String>,
    /// Last usage snapshot; populated only by the scheduler poll.
    #[serde(default)]
    pub cached_usage: Option<CachedUsage>,
    /// Per-account notification bookkeeping (crossings we've already fired,
    /// pace-fired-this-window bit). Persisted alongside the account so a
    /// menu-bar restart doesn't re-fire the same "70% crossed" notification.
    #[serde(default)]
    pub notif_state: crate::notifications::NotifState,
    /// True after the OAuth token endpoint has rejected this account's
    /// refresh token as invalid_grant. Anthropic single-uses refresh tokens
    /// and rotates on every refresh, so any stale grant (most commonly one
    /// the real `claude` CLI rotated behind our back while we weren't
    /// running) is permanently dead for its family — no retry recovers it.
    /// When set: menu shows a re-login row, auto-swap skips the account,
    /// switching to it launches `claude /login` instead of failing silently.
    /// Cleared on any successful refresh or fresh capture.
    #[serde(default)]
    pub needs_relogin: bool,
}

impl Account {
    /// Build an Account from a raw keychain blob string. The email is resolved
    /// separately by the caller (it is not present in the keychain blob).
    pub fn from_keychain_blob(blob: &str) -> Result<Account> {
        let v: serde_json::Value =
            serde_json::from_str(blob).context("keychain value is not valid JSON")?;
        let o = v.get("claudeAiOauth").ok_or_else(|| {
            anyhow!("keychain value has no claudeAiOauth object (not a claude.ai login)")
        })?;
        let access_token = o
            .get("accessToken")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("no accessToken in keychain value"))?
            .to_string();
        let refresh_token = o
            .get("refreshToken")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("no refreshToken in keychain value"))?
            .to_string();
        let expires_at = o.get("expiresAt").and_then(|x| x.as_i64()).unwrap_or(0);
        Ok(Account {
            email: None,
            access_token,
            refresh_token,
            expires_at,
            keychain_blob: blob.trim().to_string(),
            oauth_account: None,
            user_id: None,
            cached_usage: None,
            notif_state: crate::notifications::NotifState::default(),
            needs_relogin: false,
        })
    }

    /// The identity key for this account (its email, or "" if unresolved).
    pub fn key(&self) -> &str {
        self.email.as_deref().unwrap_or("")
    }

    /// The account's stable identity from its captured `oauthAccount`, preferring
    /// the accountUuid and falling back to the email.
    pub fn identity_uuid(&self) -> Option<String> {
        self.oauth_account
            .as_ref()
            .and_then(|o| o.get("accountUuid"))
            .and_then(|x| x.as_str())
            .map(String::from)
    }

    /// Update tokens only if `expires_at` is at least as new as what we already
    /// hold. Prevents a stale phase-1 snapshot (captured before a lock) from
    /// clobbering a fresher token a concurrent refresh rotated in the meantime —
    /// a single-use refresh token, once superseded, would otherwise be lost.
    /// Returns whether the update was applied.
    pub fn set_tokens_if_newer(
        &mut self,
        access: String,
        refresh: String,
        expires_at: i64,
    ) -> bool {
        if expires_at >= self.expires_at {
            self.set_tokens(access, refresh, expires_at);
            true
        } else {
            false
        }
    }

    /// Update the tokens after a refresh, keeping the blob in sync.
    pub fn set_tokens(&mut self, access: String, refresh: String, expires_at: i64) {
        self.access_token = access.clone();
        self.refresh_token = refresh.clone();
        self.expires_at = expires_at;
        // Any successful token write means we do have a working grant again,
        // so clear the re-login flag. Recovers automatically after a manual
        // `claude /login` (the next capture / sync overwrites the tokens).
        self.needs_relogin = false;
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&self.keychain_blob) {
            if let Some(o) = v.get_mut("claudeAiOauth").and_then(|x| x.as_object_mut()) {
                o.insert("accessToken".into(), serde_json::Value::String(access));
                o.insert("refreshToken".into(), serde_json::Value::String(refresh));
                o.insert("expiresAt".into(), serde_json::Value::from(expires_at));
                self.keychain_blob = v.to_string();
            }
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Email of the account currently written to the keychain, if known.
    #[serde(default)]
    pub active: Option<String>,
    /// Menu-bar: auto-swap is on unless this is set (defaults to enabled).
    #[serde(default)]
    pub autoswap_disabled: bool,
    /// Menu-bar: swap trigger threshold percent (defaults to 95).
    #[serde(default)]
    pub trigger_pct: Option<f64>,
    /// Menu-bar: Settings ▸ Notifications ▸ per-trigger enable checkboxes
    /// (threshold crossings / reset-back / weekly-pace projection). Read by
    /// `watch_cycle` on every poll so a menu toggle takes effect on the next
    /// cycle without a restart.
    #[serde(default)]
    pub notification_config: crate::notifications::NotificationConfig,
    /// Emails this in-memory state has explicitly requested be dropped via
    /// `remove()`. NOT serialized — transient authorization consumed by
    /// `save_state_safe`, so `save()` can distinguish "the caller meant to
    /// drop this account" from "a stale/blank load is about to wipe the
    /// entire config". Any other diff between old-on-disk and new-in-memory
    /// accounts (i.e. an account on disk but missing from `accounts` without
    /// being in this set) causes the save to be refused.
    #[serde(skip)]
    pub(crate) pending_removals: HashSet<String>,
}

/// Per-app config directory. Delegates to the platform Paths backend so the
/// path resolves to the OS-appropriate location (`~/.config/usagio` on
/// macOS, XDG on Linux, `%APPDATA%\usagio` on Windows). Kept as a
/// `Result` for callsite stability; the underlying trait call is infallible
/// today.
pub fn config_dir() -> Result<PathBuf> {
    // Test-only tripwire: every unit test that lands here MUST have installed
    // a `ScopedConfigDir` / `TestConfigDir` first. Without one, a stray
    // `State::load()` or `state.save()` inside a test would resolve to the
    // developer's real `~/.config/usagio`. That's how one prior test
    // wiped a live account list. Panic loudly instead of silently corrupting
    // real data.
    #[cfg(test)]
    {
        assert!(
            home_override().is_some(),
            "store::config_dir() called from a unit test with no HOME_OVERRIDE \
             installed — wrap the test in ScopedConfigDir (in-crate) or \
             TestConfigDir (integration) so state.json writes stay in a tempdir."
        );
    }

    if let Some(home) = home_override() {
        return Ok(home.join(".config").join(crate::APP_SLUG));
    }

    let p = crate::platform().paths().config_dir(crate::APP_SLUG);
    // Guard against a platform Paths impl returning an empty or relative path
    // because e.g. HOME is unset. Every downstream call to `config_dir()`
    // joins onto this, so a bad root would silently create state under CWD —
    // fail loudly with actionable context instead.
    if p.as_os_str().is_empty() || p.is_relative() {
        anyhow::bail!(
            "cannot resolve config directory: platform returned {p:?} \
             (is $HOME set?)"
        );
    }
    Ok(p)
}

fn state_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("state.json"))
}

impl State {
    pub fn load() -> Result<State> {
        let path = state_path()?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(e).context("reading state.json"),
        };
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).context("state.json is corrupt; edit or remove it")?;
        Ok(State::from_value(&v))
    }

    /// Build a State from parsed JSON, migrating the old name-keyed shape
    /// (`Account.name`, `active` = a name) to the email-keyed shape.
    pub fn from_value(v: &serde_json::Value) -> State {
        let mut accounts = Vec::new();
        // Map an old `name` -> resolved email so a legacy `active` (a name) can be
        // migrated to the new email key.
        let mut name_to_email: Vec<(String, String)> = Vec::new();

        if let Some(arr) = v.get("accounts").and_then(|a| a.as_array()) {
            for obj in arr {
                let access_token = obj.get("access_token").and_then(|x| x.as_str());
                let refresh_token = obj.get("refresh_token").and_then(|x| x.as_str());
                let (Some(access_token), Some(refresh_token)) = (access_token, refresh_token)
                else {
                    continue; // not a usable account entry
                };
                let legacy_name = obj.get("name").and_then(|x| x.as_str());
                let email = obj
                    .get("email")
                    .and_then(|x| x.as_str())
                    .map(String::from)
                    .or_else(|| {
                        obj.get("oauth_account")
                            .and_then(|o| o.get("emailAddress"))
                            .and_then(|x| x.as_str())
                            .map(String::from)
                    })
                    // Last resort so nothing is lost: fall back to the old name.
                    .or_else(|| legacy_name.map(String::from));
                if let (Some(name), Some(em)) = (legacy_name, email.as_deref()) {
                    name_to_email.push((name.to_string(), em.to_string()));
                }
                accounts.push(Account {
                    email,
                    access_token: access_token.to_string(),
                    refresh_token: refresh_token.to_string(),
                    expires_at: obj.get("expires_at").and_then(|x| x.as_i64()).unwrap_or(0),
                    keychain_blob: obj
                        .get("keychain_blob")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    oauth_account: obj.get("oauth_account").cloned().filter(|x| !x.is_null()),
                    user_id: obj
                        .get("user_id")
                        .and_then(|x| x.as_str())
                        .map(String::from),
                    cached_usage: obj
                        .get("cached_usage")
                        .and_then(|x| serde_json::from_value(x.clone()).ok()),
                    notif_state: obj
                        .get("notif_state")
                        .and_then(|x| serde_json::from_value(x.clone()).ok())
                        .unwrap_or_default(),
                    needs_relogin: obj
                        .get("needs_relogin")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false),
                });
            }
        }

        let active = v.get("active").and_then(|x| x.as_str()).map(|a| {
            // If it already matches an account email, keep it; else migrate a
            // legacy active-name to that account's email.
            if accounts.iter().any(|acc| acc.key().eq_ignore_ascii_case(a)) {
                a.to_string()
            } else if let Some((_, em)) = name_to_email
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(a))
            {
                em.clone()
            } else {
                a.to_string()
            }
        });

        State {
            accounts,
            active,
            autoswap_disabled: v
                .get("autoswap_disabled")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            trigger_pct: v.get("trigger_pct").and_then(|x| x.as_f64()),
            notification_config: v
                .get("notification_config")
                .and_then(|x| serde_json::from_value(x.clone()).ok())
                .unwrap_or_default(),
            pending_removals: HashSet::new(),
        }
    }

    /// Persist this state to `~/.config/usagio/state.json`.
    ///
    /// Delegates to [`save_state_safe`], which:
    /// * refuses to write if the new state DROPS any account not marked in
    ///   `pending_removals` (guards against a stale/blank load wiping the
    ///   config), and
    /// * writes a rolling backup of the previous `state.json` under
    ///   `backups/state-YYYYMMDD-HHMMSS.json` (keeping the last 20) before
    ///   overwriting.
    ///
    /// The atomic tmp+rename semantics of the underlying writer are preserved.
    pub fn save(&self) -> Result<()> {
        save_state_safe(self)
    }

    /// Look up an account by exact email (case-insensitive).
    pub fn find(&self, email: &str) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|a| a.key().eq_ignore_ascii_case(email))
    }

    pub fn find_mut(&mut self, email: &str) -> Option<&mut Account> {
        self.accounts
            .iter_mut()
            .find(|a| a.key().eq_ignore_ascii_case(email))
    }

    /// Resolve a user-supplied selector (a full email or a unique prefix) to an
    /// account's email key. Ambiguous prefixes and misses are errors.
    pub fn resolve(&self, selector: &str) -> Result<String> {
        let sel = selector.trim();
        if sel.is_empty() {
            return Err(anyhow!("no account specified"));
        }
        // Exact (case-insensitive) email match wins outright.
        if let Some(a) = self.find(sel) {
            return Ok(a.key().to_string());
        }
        let matches: Vec<&str> = self
            .accounts
            .iter()
            .map(|a| a.key())
            .filter(|k| k.to_lowercase().starts_with(&sel.to_lowercase()))
            .collect();
        match matches.as_slice() {
            [one] => Ok((*one).to_string()),
            [] => Err(anyhow!("no account matches '{sel}'")),
            many => Err(anyhow!(
                "'{sel}' is ambiguous — matches: {}",
                many.join(", ")
            )),
        }
    }

    pub fn upsert(&mut self, acct: Account) {
        let key = acct.key().to_string();
        if let Some(existing) = self.find_mut(&key) {
            *existing = acct;
        } else {
            self.accounts.push(acct);
        }
    }

    /// Drop the account matching `email` (case-insensitive) AND authorise the
    /// drop for the next `save()` by recording its key in `pending_removals`.
    /// The authorization is what lets `save_state_safe` distinguish this
    /// intentional removal from an accidental account-list wipe.
    pub fn remove(&mut self, email: &str) -> bool {
        let before = self.accounts.len();
        // Snapshot the keys we're about to drop so pending_removals can carry
        // their canonical lowercase form (matches the diff below in
        // `save_state_safe`).
        let removed_keys: Vec<String> = self
            .accounts
            .iter()
            .filter(|a| a.key().eq_ignore_ascii_case(email))
            .map(|a| a.key().to_lowercase())
            .collect();
        self.accounts
            .retain(|a| !a.key().eq_ignore_ascii_case(email));
        let changed = self.accounts.len() != before;
        if changed {
            for k in removed_keys {
                self.pending_removals.insert(k);
            }
        }
        changed
    }
}

// ---------------------------------------------------------------------------
// save_state_safe: overwrite protection + rolling backups
// ---------------------------------------------------------------------------

/// Cap on rolling `state.json` backups kept under `backups/`. 20 is a middle
/// ground between "one botched save can't erase everything" and disk cost —
/// each backup is a few KB even with a dozen accounts.
pub(crate) const BACKUP_KEEP_COUNT: usize = 20;

/// Persist `state` to `state.json` with two guards on top of the atomic
/// tmp+rename write path:
///
/// 1. **Overwrite protection.** Reads the existing `state.json` from disk,
///    diffs its account emails against `state.accounts`, and REFUSES the
///    write if any old email is missing without being marked in
///    `state.pending_removals`. On refusal, dumps the rejected in-memory
///    state to `/tmp/usagio-state-rejected-<unix_ts>.json` so the user has
///    both the "before" (on disk) and the "would-have-been-written" (dumped)
///    to reason about.
/// 2. **Rolling backups.** Before overwriting, copies the current
///    `state.json` to `backups/state-YYYYMMDD-HHMMSS.json` (mode 0600 under
///    a 0700 dir), then prunes so at most [`BACKUP_KEEP_COUNT`] survive.
pub fn save_state_safe(state: &State) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir).context("creating ~/.config/usagio")?;
    let path = state_path()?;

    // (1) Overwrite protection.
    if let Ok(old_bytes) = std::fs::read(&path) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&old_bytes) {
            let old = State::from_value(&v);
            let new_keys: HashSet<String> = state
                .accounts
                .iter()
                .map(|a| a.key().to_lowercase())
                .collect();
            let unauthorized: Vec<String> = old
                .accounts
                .iter()
                .map(|a| a.key().to_lowercase())
                .filter(|k| !new_keys.contains(k) && !state.pending_removals.contains(k))
                .collect();
            if !unauthorized.is_empty() {
                // Dump a REDACTED diagnostic (account keys/emails only, no
                // tokens) into config_dir/backups/ with owner-only permissions.
                // The old path wrote plaintext OAuth tokens to /tmp (shared,
                // default umask, world-readable on macOS multi-user systems).
                // See H2 in the round-1 codeaudit findings.
                let ts = chrono::Utc::now().timestamp();
                let backups_dir = dir.join("backups");
                let _ = ensure_dir_0700(&backups_dir);
                // R2-2 (round-2 codeaudit): uniquify with `-N` if two refusals
                // land in the same second, mirroring write_rolling_backup, so
                // the second dump doesn't silently overwrite the first.
                let mut dump_path = backups_dir.join(format!("state-rejected-{ts}.json"));
                let mut n: u32 = 1;
                while dump_path.exists() {
                    dump_path = backups_dir.join(format!("state-rejected-{ts}-{n}.json"));
                    n += 1;
                }
                let redacted = redact_state_for_dump(state);
                let dump_bytes = serde_json::to_vec_pretty(&redacted).unwrap_or_default();
                let _ = write_private(&dump_path, &dump_bytes);
                // R2-1: cap the rejected-dump family separately so a run of
                // consecutive refusals cannot grow unbounded, and cannot
                // dilute the BACKUP_KEEP_COUNT cap shared with real backups
                // (prune_backups matches both `state-*` prefixes).
                let _ = prune_rejected_dumps(&backups_dir, BACKUP_KEEP_COUNT);
                let msg = format!(
                    "REFUSED save_state: would drop {} account(s) without an explicit \
                     remove(): {:?}. Redacted (token-free) diagnostic written to {}. \
                     On-disk state.json is UNCHANGED.",
                    unauthorized.len(),
                    unauthorized,
                    dump_path.display(),
                );
                crate::logging::log(&msg);
                return Err(anyhow!("{msg}"));
            }
        }

        // (2) Rolling backup of the (still-current) state.json. Best-effort:
        // a backup failure logs but does NOT block the save — losing a backup
        // is far less bad than blocking a legitimate write.
        if let Err(e) = write_rolling_backup(&dir, &path, &old_bytes) {
            crate::logging::log(&format!("backup: rolling copy failed: {e:#}"));
        }
    }

    let json = serde_json::to_vec_pretty(state)?;
    let tmp = path.with_extension("json.tmp");
    // Create the temp file owner-only from the start (no umask window), then
    // rename it into place; clean up the temp file on any failure.
    if let Err(e) = write_private(&tmp, &json) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("writing state.json.tmp");
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("renaming state.json");
    }
    Ok(())
}

/// Build a token-free serialisation of `state` suitable for a diagnostic
/// dump when `save_state_safe` refuses a write. Preserves account keys,
/// emails, `active`, and `pending_removals` so the developer can reason
/// about what was rejected — replaces every secret string with "<redacted>".
pub(crate) fn redact_state_for_dump(state: &State) -> serde_json::Value {
    let accounts: Vec<serde_json::Value> = state
        .accounts
        .iter()
        .map(|a| {
            serde_json::json!({
                "email": a.email,
                "expires_at": a.expires_at,
                "access_token": "<redacted>",
                "refresh_token": "<redacted>",
                "keychain_blob": "<redacted>",
                "user_id": a.user_id,
                "oauth_account": a.oauth_account,
                "needs_relogin": a.needs_relogin,
            })
        })
        .collect();
    serde_json::json!({
        "accounts": accounts,
        "active": state.active,
        "autoswap_disabled": state.autoswap_disabled,
        "trigger_pct": state.trigger_pct,
        "pending_removals": state.pending_removals.iter().collect::<Vec<_>>(),
    })
}

/// Copy the bytes we just read for `state_path` into a timestamped file under
/// `<dir>/backups/`, then prune so at most `BACKUP_KEEP_COUNT` survive.
fn write_rolling_backup(dir: &Path, _state_path: &Path, current_bytes: &[u8]) -> Result<()> {
    let backups_dir = dir.join("backups");
    ensure_dir_0700(&backups_dir).context("preparing backups directory")?;
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    // Uniquify with a per-boot counter suffix if two saves land in the same
    // second — otherwise the second save silently overwrites the first
    // (defeating the "20 rolling backups" guarantee).
    let mut dst = backups_dir.join(format!("state-{ts}.json"));
    let mut n: u32 = 1;
    while dst.exists() {
        dst = backups_dir.join(format!("state-{ts}-{n}.json"));
        n += 1;
    }
    write_private(&dst, current_bytes).context("writing backup file")?;
    prune_backups(&backups_dir, BACKUP_KEEP_COUNT)?;
    Ok(())
}

/// Enumerate `state-*.json` under `dir`, sort by mtime ascending, and delete
/// the oldest until at most `keep` survive.
pub(crate) fn prune_backups(dir: &Path, keep: usize) -> Result<()> {
    let mut entries: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for e in std::fs::read_dir(dir).context("read backups dir")? {
        let e = match e {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("state-") || !name.ends_with(".json") {
            continue;
        }
        // R2-1: rolling-backup cap must not evict/count rejected dumps —
        // they have their own cap via `prune_rejected_dumps`.
        if name.starts_with("state-rejected-") {
            continue;
        }
        let mtime = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        entries.push((e.path(), mtime));
    }
    entries.sort_by_key(|(_, m)| *m);
    while entries.len() > keep {
        let (p, _) = entries.remove(0);
        let _ = std::fs::remove_file(&p);
    }
    Ok(())
}

/// R2-1: cap `state-rejected-*.json` diagnostic dumps at `keep` so a run of
/// consecutive save refusals cannot grow unbounded. Oldest are evicted first.
pub(crate) fn prune_rejected_dumps(dir: &Path, keep: usize) -> Result<()> {
    let mut entries: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for e in std::fs::read_dir(dir).context("read backups dir")? {
        let e = match e {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("state-rejected-") || !name.ends_with(".json") {
            continue;
        }
        let mtime = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        entries.push((e.path(), mtime));
    }
    entries.sort_by_key(|(_, m)| *m);
    while entries.len() > keep {
        let (p, _) = entries.remove(0);
        let _ = std::fs::remove_file(&p);
    }
    Ok(())
}

/// List backup files (name, mtime) newest-first. v0.5.0 dropped the menu-bar
/// "Restore from backup ▸ <list>" submenu that used to render these directly
/// (replaced by a native file panel defaulting to the backups directory —
/// see `menubar::handle_backup_restore_dialog`), but this stays a public,
/// tested entry point for anything that wants to enumerate rolling backups
/// (a future `usagio backups list` CLI command, or a test).
#[allow(dead_code)]
pub fn list_backups() -> Result<Vec<(PathBuf, std::time::SystemTime)>> {
    let dir = config_dir()?.join("backups");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for e in std::fs::read_dir(&dir).context("read backups dir")? {
        let e = match e {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("state-") || !name.ends_with(".json") {
            continue;
        }
        // Redacted diagnostic dumps live alongside rolling backups but must
        // NOT appear as a restore candidate — restoring one would clobber the
        // live state with `<redacted>` tokens.
        if name.starts_with("state-rejected-") {
            continue;
        }
        let mtime = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        out.push((e.path(), mtime));
    }
    out.sort_by_key(|(_, mt)| std::cmp::Reverse(*mt));
    Ok(out)
}

/// Path of the live state.json for callers that need to render or open it
/// (menu bar "Copy current state" / "Restore" flows).
pub fn state_json_path() -> Result<PathBuf> {
    state_path()
}

fn ensure_dir_0700(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).context("creating dir")?;
    // Best-effort, matching the prior `#[cfg(unix)]` behavior: a chmod
    // failure here shouldn't fail the caller, only the mkdir above should.
    // No-op on Windows — see `Platform::secure_permissions`.
    let _ = crate::platform::current().secure_permissions(p);
    Ok(())
}

/// Write `bytes` to `path`, creating the file owner-only (0600) from the start
/// (no umask window). Shared by every writer of token-bearing files.
#[cfg(unix)]
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)
}

#[cfg(not(unix))]
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

// ---------------------------------------------------------------------------
// In-crate test guard. Every unit test that ends up calling `config_dir()`
// (directly or via `State::load()` / `state.save()` / `with_state_lock`) MUST
// install one of these first — the tripwire in `config_dir()` panics
// otherwise, so a bug can never wipe the developer's real state.json.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) struct ScopedConfigDir {
    _home: tempfile::TempDir,
    prev: Option<PathBuf>,
}

#[cfg(test)]
impl ScopedConfigDir {
    /// Allocate a fresh tempdir and repoint the thread-local `HOME_OVERRIDE`
    /// at it. Old override (if any) is restored on drop.
    pub(crate) fn new() -> Self {
        let home = tempfile::tempdir().expect("tempdir for ScopedConfigDir");
        let prev = home_override();
        set_home_override(Some(home.path().to_path_buf()));
        Self { _home: home, prev }
    }

    /// Path of the tempdir this guard is pinning as `$HOME`.
    pub(crate) fn home(&self) -> PathBuf {
        self._home.path().to_path_buf()
    }
}

#[cfg(test)]
impl Drop for ScopedConfigDir {
    fn drop(&mut self) {
        set_home_override(self.prev.take());
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
