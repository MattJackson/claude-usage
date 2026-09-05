//! Fetch and model the /api/oauth/usage response.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::config;

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

pub fn fetch(access_token: &str) -> Result<Usage> {
    let resp = ureq::get(config::USAGE_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", config::OAUTH_BETA)
        .set("anthropic-version", "2023-06-01")
        .set("Content-Type", "application/json")
        .call();
    match resp {
        Ok(r) => r.into_json::<Usage>().context("parsing usage response"),
        Err(ureq::Error::Status(401, _)) => Err(anyhow!("unauthorized (token expired or revoked)")),
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            Err(anyhow!("usage endpoint returned HTTP {code}: {text}"))
        }
        Err(e) => Err(anyhow!("usage request failed: {e}")),
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
