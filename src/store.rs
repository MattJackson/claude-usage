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
use std::path::PathBuf;

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
}

pub fn config_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config").join("claude-usage"))
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
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir).context("creating ~/.config/claude-usage")?;
        let path = state_path()?;
        let json = serde_json::to_vec_pretty(self)?;
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

    pub fn remove(&mut self, email: &str) -> bool {
        let before = self.accounts.len();
        self.accounts
            .retain(|a| !a.key().eq_ignore_ascii_case(email));
        self.accounts.len() != before
    }
}

/// Write `bytes` to `path`, creating the file owner-only (0600) from the start.
#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
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
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
