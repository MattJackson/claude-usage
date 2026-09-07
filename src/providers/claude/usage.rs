//! Fetch and model the /api/oauth/usage response.

use serde::Deserialize;

use super::config;

/// Why a usage fetch failed. Lets callers keep the last-known cache on transient
/// failures (and back off on rate limits) instead of surfacing a hard error.
#[derive(Debug)]
pub enum FetchError {
    /// HTTP 429 — we're being rate limited; back off and reuse the cache.
    RateLimited,
    /// HTTP 401 — token expired or revoked.
    Auth,
    /// Network / transport failure (no HTTP status).
    Transient(String),
    /// Any other non-success status or a parse failure.
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Auth => write!(f, "unauthorized (token expired or revoked)"),
            FetchError::Transient(e) => write!(f, "transient error: {e}"),
            FetchError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FetchError {}

#[derive(Debug, Clone, Deserialize)]
pub struct Window {
    /// Percent of the limit used (e.g. 9.0 == 9%).
    #[serde(default)]
    pub utilization: Option<f64>,
    /// ISO-8601 timestamp when this window resets.
    #[serde(default)]
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    /// Rolling session limit (the ~5-hour window).
    pub five_hour: Option<Window>,
    /// Weekly (7-day) all-models limit.
    pub seven_day: Option<Window>,
    /// Weekly Opus-scoped limit, when present.
    pub seven_day_opus: Option<Window>,
}

pub fn fetch(access_token: &str) -> std::result::Result<Usage, FetchError> {
    let resp = ureq::get(config::USAGE_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", config::OAUTH_BETA)
        .set("anthropic-version", "2023-06-01")
        .set("Content-Type", "application/json")
        .call();
    match resp {
        Ok(r) => r
            .into_json::<Usage>()
            .map_err(|e| FetchError::Other(format!("parsing usage response: {e}"))),
        Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
        Err(ureq::Error::Status(401, _)) => Err(FetchError::Auth),
        Err(ureq::Error::Status(code, _r)) => {
            // Don't fold the raw response body into the error — it can echo
            // account/request detail and ends up in the debug log (same hygiene
            // rule as oauth::post_token). The status code is enough.
            Err(FetchError::Other(format!(
                "usage endpoint returned HTTP {code}"
            )))
        }
        Err(e) => Err(FetchError::Transient(e.to_string())),
    }
}

/// Best-effort account email from the profile endpoint.
pub fn fetch_email(access_token: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Account {
        email: Option<String>,
        email_address: Option<String>,
    }
    #[derive(Deserialize)]
    struct Profile {
        account: Option<Account>,
    }
    let resp = ureq::get(config::PROFILE_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", config::OAUTH_BETA)
        .set("anthropic-version", "2023-06-01")
        .call()
        .ok()?;
    let p: Profile = resp.into_json().ok()?;
    let a = p.account?;
    a.email.or(a.email_address)
}

/// Fetch the raw profile JSON (`account`, `organization`, ...).
pub fn fetch_profile(access_token: &str) -> Option<serde_json::Value> {
    ureq::get(config::PROFILE_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", config::OAUTH_BETA)
        .set("anthropic-version", "2023-06-01")
        .call()
        .ok()?
        .into_json()
        .ok()
}

/// Build an `oauthAccount` object (the shape Claude Code stores in
/// `~/.claude.json`) from a profile response. Used to backfill the identity for
/// accounts captured before we started snapshotting it. Claude refreshes the
/// remaining fields (e.g. `profileFetchedAt`) on its next profile fetch.
pub fn oauth_account_from_profile(profile: &serde_json::Value) -> Option<serde_json::Value> {
    let acct = profile.get("account")?;
    let org = profile.get("organization");
    let get = |v: Option<&serde_json::Value>, k: &str| {
        v.and_then(|o| o.get(k))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    Some(serde_json::json!({
        "accountUuid": get(Some(acct), "uuid"),
        "emailAddress": get(Some(acct), "email"),
        "displayName": get(Some(acct), "display_name"),
        "fullName": get(Some(acct), "full_name"),
        "accountCreatedAt": get(Some(acct), "created_at"),
        "organizationUuid": get(org, "uuid"),
        "organizationName": get(org, "name"),
        "organizationType": get(org, "organization_type"),
        "organizationRateLimitTier": get(org, "rate_limit_tier"),
        "billingType": get(org, "billing_type"),
        "hasExtraUsageEnabled": get(org, "has_extra_usage_enabled"),
        "subscriptionCreatedAt": get(org, "subscription_created_at"),
    }))
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
