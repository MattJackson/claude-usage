//! OAuth refresh-token grant. We only ever refresh here; new accounts are
//! onboarded by capturing a real `claude` login from the keychain.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::config;
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

/// Refresh an account's access token in place, keeping the keychain blob in
/// sync. Returns true (it always changes the tokens on success).
pub fn refresh(acct: &mut Account) -> Result<bool> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": acct.refresh_token,
        "client_id": config::CLIENT_ID,
    });
    let tok = post_token(&body).context("refreshing access token")?;
    let expires_at = now_millis().saturating_add(tok.expires_in.saturating_mul(1000));
    // Keep the existing refresh token if the server didn't rotate one.
    let refresh_token = tok
        .refresh_token
        .unwrap_or_else(|| acct.refresh_token.clone());
    acct.set_tokens(tok.access_token, refresh_token, expires_at);
    Ok(true)
}

/// Refresh only if the token expires within `skew_secs`.
pub fn ensure_fresh(acct: &mut Account, skew_secs: i64) -> Result<bool> {
    // Saturating so a corrupt `expires_at` (e.g. near i64::MIN loaded from a
    // malformed state.json) can't overflow — it just reads as "expired" and
    // triggers a refresh instead of panicking (debug) or wrapping (release).
    if acct.expires_at.saturating_sub(now_millis()) <= skew_secs.saturating_mul(1000) {
        refresh(acct)
    } else {
        Ok(false)
    }
}

fn post_token(body: &serde_json::Value) -> Result<TokenResponse> {
    let resp = ureq::post(config::TOKEN_URL)
        .set("Content-Type", "application/json")
        .set("anthropic-beta", config::OAUTH_BETA)
        .send_json(body.clone());
    match resp {
        Ok(r) => r
            .into_json::<TokenResponse>()
            .context("parsing token response"),
        Err(ureq::Error::Status(code, _r)) => {
            // Don't include the raw response body — it can echo submitted request
            // data and ends up in the debug log. The status code is enough.
            Err(anyhow!("token endpoint returned HTTP {code}"))
        }
        Err(e) => Err(anyhow!("token request failed: {e}")),
    }
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
