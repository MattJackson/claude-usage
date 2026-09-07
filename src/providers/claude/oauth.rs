//! OAuth refresh-token grant. We only ever refresh here; new accounts are
//! onboarded by capturing a real `claude` login from the keychain.

use serde::Deserialize;

use super::config;
use crate::store::Account;

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// OPTIONAL in a refresh-grant response (RFC 6749 §6): when the server does
    /// not rotate the refresh token it may omit this, and the old one stays valid.
    #[serde(default)]
    refresh_token: Option<String>,
    /// Lifetime of the access token, in seconds.
    expires_in: i64,
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Refresh outcomes distinguished so callers can react correctly.
///
/// The critical signal is `InvalidGrant`: Anthropic's OAuth server rotates
/// the refresh token on every successful refresh and single-uses the old
/// one, so a stale copy — most often produced when the user runs the real
/// `claude` CLI, which refreshes and rotates behind our back — comes back
/// as HTTP 400 invalid_grant and is permanently dead for that grant family.
/// The account needs a fresh `/login`; no amount of retrying will help.
#[derive(Debug)]
pub enum RefreshError {
    /// Anthropic said 400 (or the account has no refresh token to send).
    /// The stored refresh token is now permanently dead; user must re-login.
    InvalidGrant,
    /// Anthropic said 429. Back off; retry later.
    RateLimited,
    /// Network / 5xx / parse error. Retry with backoff is fine.
    Transient(String),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::InvalidGrant => write!(f, "refresh token rejected (invalid_grant)"),
            RefreshError::RateLimited => write!(f, "token endpoint rate-limited (429)"),
            RefreshError::Transient(s) => write!(f, "transient: {s}"),
        }
    }
}

impl std::error::Error for RefreshError {}

/// Refresh an account's access token in place, keeping the keychain blob in
/// sync. Returns true (it always changes the tokens on success).
///
/// Errors are typed so the caller can decide policy: `InvalidGrant` means the
/// account needs a fresh `/login` (flag `needs_relogin` in state); anything
/// else is worth another try on the next tick.
pub fn refresh(acct: &mut Account) -> Result<bool, RefreshError> {
    if acct.refresh_token.is_empty() {
        // Nothing to send. Treat identically to a rejected grant so the
        // account gets flagged and skipped consistently.
        return Err(RefreshError::InvalidGrant);
    }
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": acct.refresh_token,
        "client_id": config::CLIENT_ID,
    });
    let tok = post_token(&body)?;
    let expires_at = now_millis().saturating_add(tok.expires_in.saturating_mul(1000));
    // Keep the existing refresh token if the server didn't rotate one.
    let refresh_token = tok
        .refresh_token
        .unwrap_or_else(|| acct.refresh_token.clone());
    acct.set_tokens(tok.access_token, refresh_token, expires_at);
    Ok(true)
}

/// Refresh only if the token expires within `skew_secs`.
pub fn ensure_fresh(acct: &mut Account, skew_secs: i64) -> Result<bool, RefreshError> {
    if needs_refresh(acct.expires_at, now_millis(), skew_secs) {
        refresh(acct)
    } else {
        Ok(false)
    }
}

/// Whether a token expiring at `expires_at` (unix millis) is within `skew_secs`
/// of `now_millis`. Saturating so a corrupt `expires_at` (e.g. near i64::MIN from
/// a malformed state.json) reads as "expired" instead of overflowing — a plain
/// subtraction would panic in debug and wrap to "fresh" in release. Pure, tested.
fn needs_refresh(expires_at: i64, now_millis: i64, skew_secs: i64) -> bool {
    expires_at.saturating_sub(now_millis) <= skew_secs.saturating_mul(1000)
}

fn post_token(body: &serde_json::Value) -> Result<TokenResponse, RefreshError> {
    let resp = ureq::post(config::TOKEN_URL)
        .set("Content-Type", "application/json")
        .set("anthropic-beta", config::OAUTH_BETA)
        .send_json(body.clone());
    match resp {
        Ok(r) => r
            .into_json::<TokenResponse>()
            .map_err(|e| RefreshError::Transient(format!("parsing token response: {e}"))),
        Err(ureq::Error::Status(400, _)) => Err(RefreshError::InvalidGrant),
        Err(ureq::Error::Status(401, _)) => Err(RefreshError::InvalidGrant),
        Err(ureq::Error::Status(429, _)) => Err(RefreshError::RateLimited),
        Err(ureq::Error::Status(code, _)) => {
            // Don't include the raw response body — it can echo submitted request
            // data and ends up in the debug log. The status code is enough.
            Err(RefreshError::Transient(format!(
                "token endpoint returned HTTP {code}"
            )))
        }
        Err(e) => Err(RefreshError::Transient(format!(
            "token request failed: {e}"
        ))),
    }
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
