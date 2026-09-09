//! Codex OAuth refresh-token grant + active-account CAS refresh.
//!
//! ## Phase 1 research (recorded here so the evidence travels with the code)
//!
//! Source: `openai/codex` on GitHub, `codex-rs/login/src/auth/manager.rs`
//! (`request_chatgpt_token_refresh`, `RefreshRequest`, `RefreshResponse`,
//! `should_refresh_proactively`, `classify_refresh_token_failure`), fetched
//! via `gh api repos/openai/codex/contents/...` on 2026-09-07.
//!
//! - **Refresh endpoint**: `https://auth.openai.com/oauth/token` (the vendor
//!   CLI's own override env var is `CODEX_REFRESH_TOKEN_URL_OVERRIDE`; we
//!   honor the same name so a staging/mock auth service can be pointed at
//!   without patching a constant, and so tests never touch the real network).
//! - **client_id**: `app_EMoamEEZ73f0CkXaXp7hrann` (override env var
//!   `CODEX_APP_SERVER_LOGIN_CLIENT_ID`, again matching the vendor CLI).
//! - **Grant body**: `{ "grant_type": "refresh_token", "refresh_token", "client_id" }`.
//!   No `scope` parameter — the vendor's `RefreshRequest` struct has none.
//! - **Refresh-token model: single-use / rotated**, exactly like Anthropic's.
//!   Reusing an already-consumed refresh_token comes back as an error body
//!   with `error.code == "refresh_token_reused"` ("Your refresh token has
//!   already been used to generate a new access token. Please try signing in
//!   again."). A stale copy — e.g. ours, racing a `codex` CLI invocation that
//!   refreshed first — is therefore permanently dead, not retryable. The
//!   vendor CLI also distinguishes `refresh_token_expired` and
//!   `refresh_token_invalidated` (revoked); we fold all three, plus a bare
//!   RFC 6749 `invalid_grant` on HTTP 400/401, into `RefreshError::InvalidGrant`
//!   — callers need "dead, don't retry" vs. "retry later", not the subtype.
//! - **No `expires_in` in the response.** Like `parse_stored_blob`, expiry is
//!   read from the `exp` claim of the returned `id_token` (falling back to
//!   the `access_token` JWT if no `id_token` came back).
//! - **Server-side session TTL independent of the access token's own expiry**:
//!   the vendor CLI's `should_refresh_proactively` refreshes whenever EITHER
//!   (a) the access token expires within 5 minutes, OR (b) `last_refresh` is
//!   more than `TOKEN_REFRESH_INTERVAL = 8` days old — i.e. OpenAI enforces
//!   roughly an 8-day rotation cadence on the session regardless of how long
//!   the bearer token itself remains valid. usagio leaves that cadence to the
//!   Codex CLI for the active login (see `active_refresh_cas` — usagio never
//!   POSTs `/token` for the account the CLI owns).
//! - **Refresh is fully programmatic**: a plain HTTPS POST issued by the CLI
//!   binary itself (or, per community reports, OpenAI's server-side session
//!   also expires refresh tokens outright after on the order of ~10-30 days
//!   of total inactivity — exact refresh_token TTL isn't published, but the
//!   8-day proactive cadence above means a healthy Codex install never gets
//!   close to it). No interactive browser step is required for a refresh
//!   grant, so `Capabilities::supports_active_refresh` (see `trait_def.rs`)
//!   is `true` for Codex.

use serde::{Deserialize, Serialize};

use super::{auth_json_path, jwt_payload_claims};
use crate::providers::trait_def::TokenGrant;

/// Codex's OAuth token endpoint. Override via `CODEX_REFRESH_TOKEN_URL_OVERRIDE`
/// (same env var name the vendor CLI itself honors) — tests use this to point
/// at a local mock server instead of the real network.
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Public OAuth client id the `codex` CLI itself uses for the refresh grant.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[cfg(test)]
const TOKEN_URL_OVERRIDE_ENV: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";
#[cfg(test)]
const CLIENT_ID_OVERRIDE_ENV: &str = "CODEX_APP_SERVER_LOGIN_CLIENT_ID";

// Prod: always hardcoded HTTPS endpoint + client id. The env-var overrides
// (used by tests to point at a local mock server) are `#[cfg(test)]`-only —
// mirroring Claude's oauth.rs pattern — so a compromised shell / LaunchAgent
// env cannot redirect the refresh-token POST to an attacker-controlled URL.
// (Codex's own CLI honors these env vars in prod, but usagio is a separate
// binary; a user who genuinely needs an alternate auth server would rebuild
// from source or file a request for a persistent config setting.)
#[cfg(not(test))]
fn token_url() -> String {
    TOKEN_URL.to_string()
}

#[cfg(not(test))]
fn oauth_client_id() -> String {
    CLIENT_ID.to_string()
}

#[cfg(test)]
fn token_url() -> String {
    std::env::var(TOKEN_URL_OVERRIDE_ENV).unwrap_or_else(|_| TOKEN_URL.to_string())
}

#[cfg(test)]
fn oauth_client_id() -> String {
    std::env::var(CLIENT_ID_OVERRIDE_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| CLIENT_ID.to_string())
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: String,
    grant_type: &'static str,
    refresh_token: &'a str,
}

#[derive(Debug, Default, Deserialize)]
struct RefreshResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Refresh outcomes, mirroring `claude::oauth::RefreshError`'s shape so a
/// future provider-agnostic caller (e.g. a shared active-refresh loop) can
/// treat both the same way.
#[derive(Debug)]
pub enum RefreshError {
    /// Terminal: `refresh_token_expired` / `refresh_token_reused` /
    /// `refresh_token_invalidated` / bare `invalid_grant`, or no refresh
    /// token to send. The account needs a fresh `codex login`.
    InvalidGrant,
    /// Codex's token endpoint asked us to back off (HTTP 429).
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

/// A refreshed Codex grant produced by the INACTIVE-account refresh path
/// (`CodexProvider::refresh_token`). The active login is never refreshed by
/// usagio (see `active_refresh_cas`), so this only ever feeds the generic
/// `TokenGrant` conversion below — it carries just the fields that conversion
/// needs.
#[derive(Debug, Clone)]
pub struct CodexRefreshGrant {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in_secs: i64,
}

impl From<CodexRefreshGrant> for TokenGrant {
    fn from(g: CodexRefreshGrant) -> Self {
        TokenGrant {
            access: g.access_token,
            refresh: Some(g.refresh_token),
            expires_in_secs: g.expires_in_secs,
        }
    }
}

/// POST a `refresh_token` grant to Codex's token endpoint. Pure network call
/// — no filesystem access, no CAS. See `active_refresh_cas` for the
/// file-safe wrapper used for the single active Codex login.
pub fn refresh_token_grant(refresh_token: &str) -> Result<CodexRefreshGrant, RefreshError> {
    if refresh_token.trim().is_empty() {
        // Nothing to send; treat identically to a rejected grant so the
        // caller's policy (flag needs_relogin, stop retrying) is uniform.
        return Err(RefreshError::InvalidGrant);
    }
    let body = RefreshRequest {
        client_id: oauth_client_id(),
        grant_type: "refresh_token",
        refresh_token,
    };
    let resp = super::http_agent()
        .post(&token_url())
        .set("Content-Type", "application/json")
        .send_json(
            serde_json::to_value(&body).map_err(|e| RefreshError::Transient(e.to_string()))?,
        );
    let parsed: RefreshResponse = match resp {
        Ok(r) => r
            .into_json()
            .map_err(|e| RefreshError::Transient(format!("parsing codex refresh response: {e}")))?,
        Err(ureq::Error::Status(400, _)) => return Err(RefreshError::InvalidGrant),
        Err(ureq::Error::Status(401, _)) => return Err(RefreshError::InvalidGrant),
        Err(ureq::Error::Status(403, _)) => return Err(RefreshError::InvalidGrant),
        Err(ureq::Error::Status(429, _)) => return Err(RefreshError::RateLimited),
        Err(ureq::Error::Status(code, _)) => {
            return Err(RefreshError::Transient(format!(
                "codex token endpoint returned HTTP {code}"
            )))
        }
        Err(e) => {
            return Err(RefreshError::Transient(format!(
                "token request failed: {e}"
            )))
        }
    };
    let access_token = parsed.access_token.ok_or_else(|| {
        RefreshError::Transient("codex refresh response missing access_token".into())
    })?;
    // RFC 6749 §6: keep the caller-supplied refresh token if the server
    // didn't rotate one (Codex always does today, but don't assume).
    let refresh_token_out = parsed
        .refresh_token
        .unwrap_or_else(|| refresh_token.to_string());
    let expires_in_secs = parsed
        .id_token
        .as_deref()
        .or(Some(access_token.as_str()))
        .and_then(jwt_payload_claims)
        .and_then(|c| c.get("exp").and_then(|x| x.as_i64()))
        .map(|exp| exp.saturating_sub(chrono::Utc::now().timestamp()))
        .unwrap_or(0);
    Ok(CodexRefreshGrant {
        access_token,
        refresh_token: refresh_token_out,
        expires_in_secs,
    })
}

/// Outcome of `active_refresh_cas`.
#[derive(Debug, PartialEq)]
pub enum CasOutcome {
    /// The on-disk `auth.json` still matches the blob the caller last observed
    /// (`last_known_blob`): the Codex CLI hasn't rotated it, so there is
    /// nothing new to adopt and no state update is needed.
    Fresh,
    /// The active `auth.json` differs from (or was never compared against) the
    /// caller's last-known blob — the Codex CLI has rotated it since. usagio
    /// adopts whatever the CLI now holds; the returned `String` is the winning
    /// blob, exactly as read from disk, so the caller can absorb it into
    /// whatever cache it keeps.
    Adopted(String),
}

/// Adopt the single active Codex login from the slot the Codex CLI owns
/// (`~/.codex/auth.json`), **never POSTing `/token` for it**.
///
/// ## Why usagio must never refresh the active Codex account
///
/// Codex's refresh tokens are single-use / rotated server-side (see the module
/// doc: reuse comes back `refresh_token_reused`). The Codex CLI reads and
/// rewrites `auth.json` on its own cadence. If usagio POSTed `/token` for the
/// active account it would rotate the token family server-side and permanently
/// invalidate the copy the CLI holds — the CLI's next refresh would then fail
/// and force an interactive `codex login`. The destructive act is the POST
/// itself, and it is irreversible by the time we read the slot back, so no
/// after-the-fact compare-and-swap can prevent it. This is the exact class of
/// bug fixed for Claude in `main.rs::active_refresh_cas`.
///
/// So for the active account usagio is a pure follower: it reads `auth.json`
/// and adopts whatever the Codex CLI currently holds, keeping usagio's state in
/// sync without ever minting a token. The CLI refreshes the active login on its
/// own cadence; usagio only needs the current tokens (from the slot) to query
/// usage. The INACTIVE-account refresh path (`CodexProvider::refresh_token` →
/// `refresh_token_grant`) is unaffected — the CLI isn't using those logins, so
/// refreshing them is safe.
///
/// `last_known_blob` is the blob the caller last observed (typically the
/// account's cached `secret_blob`). If the on-disk blob still equals it, the
/// CLI hasn't rotated anything and we return `Fresh` (no state update needed);
/// otherwise we return `Adopted(blob)` with the current on-disk bytes for the
/// caller to absorb.
pub fn active_refresh_cas(last_known_blob: Option<&str>) -> Result<CasOutcome, RefreshError> {
    let path = auth_json_path()
        .ok_or_else(|| RefreshError::Transient("could not resolve $CODEX_HOME / $HOME".into()))?;
    let before = std::fs::read_to_string(&path)
        .map_err(|e| RefreshError::Transient(format!("reading auth.json: {e}")))?;

    match last_known_blob {
        Some(known) if known == before => Ok(CasOutcome::Fresh),
        _ => Ok(CasOutcome::Adopted(before)),
    }
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
