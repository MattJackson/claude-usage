//! Persistent, owner-only token store for all captured accounts.
//!
//! Each account keeps the *exact* keychain blob captured from a real
//! `claude` login (`{"claudeAiOauth":{...}}`), so writing it back on a
//! switch always produces a login Claude Code accepts. The access/refresh
//! tokens are also mirrored as plain fields for API calls, and patched back
//! into the blob whenever we refresh.

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
    /// Friendly name chosen by the user (unique).
    pub name: String,
    /// Label discovered from the profile endpoint (email), if known.
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
    /// Build an Account from a raw keychain blob string.
    pub fn from_keychain_blob(name: String, blob: &str) -> Result<Account> {
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
            name,
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

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Name of the account currently written to the keychain, if known.
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
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("state.json is corrupt; edit or remove it")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).context("reading state.json"),
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir).context("creating ~/.config/claude-usage")?;
        let path = state_path()?;
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).context("writing state.json.tmp")?;
        set_owner_only(&tmp)?;
        std::fs::rename(&tmp, &path).context("renaming state.json")?;
        set_owner_only(&path)?;
        Ok(())
    }

    pub fn find(&self, name: &str) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name))
    }

    pub fn find_mut(&mut self, name: &str) -> Option<&mut Account> {
        self.accounts
            .iter_mut()
            .find(|a| a.name.eq_ignore_ascii_case(name))
    }

    pub fn upsert(&mut self, acct: Account) {
        if let Some(existing) = self.find_mut(&acct.name) {
            let name = existing.name.clone();
            *existing = acct;
            existing.name = name;
        } else {
            self.accounts.push(acct);
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.accounts.len();
        self.accounts.retain(|a| !a.name.eq_ignore_ascii_case(name));
        self.accounts.len() != before
    }

    /// Rename an account (case-insensitive lookup of `old`). Updates `active` if
    /// it pointed at the renamed account. Errors if `old` is missing, `new` is
    /// empty, or `new` already names a different account.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<()> {
        let new = new.trim();
        if new.is_empty() {
            return Err(anyhow!("the new name cannot be empty"));
        }
        if self.find(old).is_none() {
            return Err(anyhow!("no account named '{old}'"));
        }
        // Allow a pure case change of the same account, but not colliding with a
        // different one.
        if self
            .accounts
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(new) && !a.name.eq_ignore_ascii_case(old))
        {
            return Err(anyhow!("an account named '{new}' already exists"));
        }
        let was_active = self
            .active
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(old));
        if let Some(a) = self.find_mut(old) {
            a.name = new.to_string();
        }
        if was_active {
            self.active = Some(new.to_string());
        }
        Ok(())
    }
}

#[cfg(unix)]
fn set_owner_only(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .context("chmod 600 on state file")
}

#[cfg(not(unix))]
fn set_owner_only(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
