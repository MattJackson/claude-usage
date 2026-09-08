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
//!   the bearer token itself remains valid. `grant_needs_refresh` below
//!   mirrors both conditions.
//! - **Refresh is fully programmatic**: a plain HTTPS POST issued by the CLI
//!   binary itself (or, per community reports, OpenAI's server-side session
//!   also expires refresh tokens outright after on the order of ~10-30 days
//!   of total inactivity — exact refresh_token TTL isn't published, but the
//!   8-day proactive cadence above means a healthy Codex install never gets
//!   close to it). No interactive browser step is required for a refresh
//!   grant, so `Capabilities::supports_active_refresh` (see `trait_def.rs`)
//!   is `true` for Codex.

use serde::{Deserialize, Serialize};

use super::{auth_json_path, jwt_payload_claims, parse_codex_blob, write_auth_json_atomically};
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

/// Vendor's own proactive-refresh cadence: refresh if `last_refresh` in
/// `auth.json` is older than this many days, even if the access token isn't
/// close to expiry yet. See the module doc for the source.
pub const SESSION_STALE_AFTER_DAYS: i64 = 8;

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

/// A refreshed Codex grant, still carrying `id_token` (unlike the generic
/// `TokenGrant`) so the CAS writer below can patch `auth.json`'s `tokens`
/// object completely — including the identity-bearing `id_token` — not just
/// the two fields `TokenGrant` knows about.
#[derive(Debug, Clone)]
pub struct CodexRefreshGrant {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: Option<String>,
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
        id_token: parsed.id_token,
        expires_in_secs,
    })
}

/// Whether `blob` (a full `auth.json` blob) needs a refresh right now: either
/// its access token expires within `skew_secs`, or its `last_refresh` is
/// older than `stale_after_secs` (the vendor's own ~8-day session cadence —
/// see the module doc). A blob with no `last_refresh` at all only triggers
/// the skew check, matching the vendor CLI's own `should_refresh_proactively`
/// (which likewise only applies the staleness check when `last_refresh` is
/// present).
pub fn grant_needs_refresh(blob: &str, skew_secs: i64, stale_after_secs: i64) -> bool {
    let Ok(parsed) = parse_codex_blob(blob) else {
        // Unparseable / no usable access_token: nothing we can refresh our
        // way out of here, but report "doesn't need refresh" so callers don't
        // spin retrying a network call against a blob that isn't ours to fix.
        return false;
    };
    let claims = parsed
        .tokens
        .id_token
        .as_deref()
        .and_then(jwt_payload_claims);
    if let Some(exp) = claims
        .as_ref()
        .and_then(|c| c.get("exp"))
        .and_then(|x| x.as_i64())
    {
        let remaining = exp.saturating_sub(chrono::Utc::now().timestamp());
        if remaining <= skew_secs {
            return true;
        }
    }
    if let Some(last_refresh) = parsed.last_refresh.as_deref() {
        if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_refresh) {
            let age = chrono::Utc::now()
                .signed_duration_since(ts.with_timezone(&chrono::Utc))
                .num_seconds();
            if age >= stale_after_secs {
                return true;
            }
        }
    }
    false
}

/// Outcome of `active_refresh_cas`.
#[derive(Debug, PartialEq)]
pub enum CasOutcome {
    /// The stored access token had plenty of life left; no network call was
    /// made.
    Fresh,
    /// We refreshed and won the compare-and-swap: `auth.json` now holds our
    /// new grant.
    Refreshed,
    /// Someone else (the `codex` CLI, or another `usagio` process) rotated
    /// `auth.json` either before we started or while our POST was in
    /// flight. We discarded our own grant and adopted theirs — the returned
    /// `String` is the winning blob, exactly as read from disk, so the
    /// caller can absorb it into whatever cache it keeps.
    Adopted(String),
}

/// Refresh the single active Codex login under a compare-and-swap on
/// `auth.json`'s raw bytes.
///
/// Unlike Claude (keychain blob + `~/.claude.json` identity file + a
/// state.json copy, three places that can independently drift), Codex has
/// exactly one source of truth: `auth.json` itself (see the module doc on
/// `codex::mod` — "the auth.json write *is* the entire switch"). So the CAS
/// reference point is simply "the last blob we observed", passed in as
/// `last_known_blob` (typically the blob a caller read a few seconds ago
/// while deciding whether a refresh was due); there is no separate cached
/// copy in `state.json` to reconcile against because Codex has no state.json
/// account slot yet (`absorb_credential`'s doc comment on the same TODO).
///
/// Sequence:
/// 1. Read `auth.json`. If `last_known_blob` is `Some` and disagrees with
///    what's on disk right now, someone already rotated it — adopt it and
///    return `Adopted` without ever touching the network.
/// 2. If the (now-confirmed-current) blob doesn't need a refresh yet, return
///    `Fresh`.
/// 3. POST the refresh. This is the only network I/O and the only step that
///    can take arbitrarily long — exactly the window a concurrent `codex`
///    CLI refresh could land in.
/// 4. Re-read `auth.json`. If it's unchanged since step 1, we won: patch in
///    our new tokens and write atomically. If it changed, we lost: discard
///    our (now-guaranteed-stale, about to be `refresh_token_reused`) grant
///    and adopt whatever's there instead.
///
/// concurrency-03 (v0.5.1 audit): step 4's "we won" conclusion is NOT a real
/// atomic compare-and-swap on the filesystem — it's "read, POST, read, then
/// separately write", and nothing excludes a genuinely concurrent writer
/// (most importantly the real `codex` CLI, a separate process that has no
/// knowledge of and cannot participate in any lock usagio takes) from
/// writing `auth.json` in the gap between the confirming re-read and the
/// `rename()` inside `write_auth_json_atomically`. There is no portable
/// "compare bytes and write" filesystem primitive that closes this
/// completely — doing so for real would require an OS-level lock file the
/// vendor CLI would ALSO have to honor, which usagio cannot arrange since it
/// doesn't control that binary. What this function does instead: (a) keep
/// the re-read-to-write gap as small as physically possible (no I/O, no
/// network calls, nothing but pure in-memory patching between the
/// confirming read and the write below), and (b) immediately re-read once
/// more right after the write completes, so if a third writer's rotation
/// landed in that residual gap and our write clobbered it, we detect the
/// mismatch on the very next line and adopt the now-current content instead
/// of trusting our own write blindly. This narrows the blast radius to "we
/// might silently clobber a rotation that lands in a multi-microsecond
/// window" rather than "we might clobber a rotation and never notice."
pub fn active_refresh_cas(
    skew_secs: i64,
    stale_after_secs: i64,
    last_known_blob: Option<&str>,
) -> Result<CasOutcome, RefreshError> {
    let path = auth_json_path()
        .ok_or_else(|| RefreshError::Transient("could not resolve $CODEX_HOME / $HOME".into()))?;
    let before = std::fs::read_to_string(&path)
        .map_err(|e| RefreshError::Transient(format!("reading auth.json: {e}")))?;

    if let Some(known) = last_known_blob {
        if known != before {
            return Ok(CasOutcome::Adopted(before));
        }
    }

    if !grant_needs_refresh(&before, skew_secs, stale_after_secs) {
        return Ok(CasOutcome::Fresh);
    }

    let parsed = parse_codex_blob(&before).map_err(|e| RefreshError::Transient(e.to_string()))?;
    let refresh_token = parsed
        .tokens
        .refresh_token
        .clone()
        .ok_or(RefreshError::InvalidGrant)?;

    let grant = refresh_token_grant(&refresh_token)?;

    let after = std::fs::read_to_string(&path)
        .map_err(|e| RefreshError::Transient(format!("reading auth.json: {e}")))?;
    if after != before {
        // Lost the race: our grant is now stale (the CLI's own refresh just
        // consumed the same refresh_token family), so we throw it away
        // rather than risk a `refresh_token_reused` write that could corrupt
        // the CLI's freshly-written state.
        return Ok(CasOutcome::Adopted(after));
    }

    // Nothing but pure, in-memory patching between the confirming read
    // above and the write below — see the concurrency-03 doc comment on
    // this function for why that gap can't be closed to zero.
    let patched = apply_grant_to_blob(&before, &grant)
        .map_err(|e| RefreshError::Transient(format!("patching auth.json: {e}")))?;
    write_auth_json_atomically(&path, &patched)
        .map_err(|e| RefreshError::Transient(format!("writing auth.json: {e}")))?;

    // Post-write confirmation re-read (concurrency-03): if a concurrent
    // writer's rotation landed in the residual read-to-write gap and our
    // `rename()` clobbered it, this catches the mismatch immediately and
    // adopts what's actually on disk now instead of the caller believing
    // our (possibly already-stale) write stuck.
    match std::fs::read_to_string(&path) {
        Ok(confirm) if confirm == patched => {
            crate::logging::log("event=codex_cas_write_confirmed");
        }
        Ok(confirm) => {
            crate::logging::log(
                "event=codex_cas_post_write_race_detected reason=auth_json_changed_after_our_write",
            );
            return Ok(CasOutcome::Adopted(confirm));
        }
        Err(e) => {
            // Can't confirm, but the write itself already reported success —
            // log and proceed rather than fail an otherwise-successful
            // refresh over a read error on the confirmation step alone.
            crate::logging::log(&format!(
                "event=codex_cas_post_write_confirm_read_failed reason={e}"
            ));
        }
    }
    Ok(CasOutcome::Refreshed)
}

/// Merge a `CodexRefreshGrant` into a full `auth.json` blob, preserving every
/// other top-level field (`OPENAI_API_KEY`, etc.) untouched. Unlike
/// `CodexProvider::patch_stored_blob` (which only knows the generic
/// `TokenGrant` shape), this also updates `tokens.id_token` — needed so the
/// next `capture_current_login` / `identify_credential` sees the refreshed
/// identity claims, not stale ones.
fn apply_grant_to_blob(blob: &str, grant: &CodexRefreshGrant) -> Result<String, String> {
    let mut v: serde_json::Value =
        serde_json::from_str(blob).map_err(|e| format!("auth.json is not valid JSON: {e}"))?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| "auth.json is not a JSON object".to_string())?;
    let tokens = obj
        .entry("tokens".to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let tokens_obj = tokens
        .as_object_mut()
        .ok_or_else(|| "auth.json `tokens` is not an object".to_string())?;
    tokens_obj.insert(
        "access_token".into(),
        serde_json::Value::String(grant.access_token.clone()),
    );
    tokens_obj.insert(
        "refresh_token".into(),
        serde_json::Value::String(grant.refresh_token.clone()),
    );
    if let Some(id_token) = grant.id_token.as_ref() {
        tokens_obj.insert(
            "id_token".into(),
            serde_json::Value::String(id_token.clone()),
        );
    }
    obj.insert(
        "last_refresh".into(),
        serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
    );
    Ok(v.to_string())
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
