//! OAuth refresh-token grant. We only ever refresh here; new accounts are
//! onboarded by capturing a real `claude` login from the keychain.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::config;
use crate::store::Account;

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
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
    acct.set_tokens(
        tok.access_token,
        tok.refresh_token,
        now_millis() + tok.expires_in * 1000,
    );
    Ok(true)
}

/// Refresh only if the token expires within `skew_secs`.
pub fn ensure_fresh(acct: &mut Account, skew_secs: i64) -> Result<bool> {
    if acct.expires_at - now_millis() <= skew_secs * 1000 {
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
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            Err(anyhow!("token endpoint returned HTTP {code}: {text}"))
        }
        Err(e) => Err(anyhow!("token request failed: {e}")),
    }
}
