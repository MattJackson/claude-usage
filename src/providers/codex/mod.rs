//! Codex (OpenAI) provider.
//!
//! Codex CLI stores its login in `$CODEX_HOME/auth.json` (default
//! `~/.codex/auth.json`, mode 0600). The blob is an `AuthDotJson` object:
//!
//! ```json
//! {
//!   "OPENAI_API_KEY": null,
//!   "tokens": {
//!     "id_token": "<jwt>",
//!     "access_token": "<jwt>",
//!     "refresh_token": "<opaque>",
//!     "account_id": "<uuid>"
//!   },
//!   "last_refresh": "<rfc3339>"
//! }
//! ```
//!
//! The `id_token` JWT payload carries `email` plus the ChatGPT plan/account
//! identifiers — no network call is needed to attach an email to the account.
//!
//! Unlike Claude, Codex doesn't use the macOS keychain at all — the `codex`
//! CLI itself signs in by writing this file directly. Switching therefore has
//! no keychain half: `write_active_account` rewrites `auth.json` atomically
//! (tmp file + rename) with parent-dir/file permissions matching what the
//! vendor CLI itself would produce (`0700`/`0600`). There is no separate
//! identity file to touch (unlike `~/.claude.json`), so the auth.json write
//! *is* the entire switch. `launch_client` remains unimplemented
//! (`ProviderError::Unsupported`) — a future phase may shell out to `codex`.
//!
//! TODO(v0.5.x, H3 — v0.5.0 codeaudit): `write_active_account` above is a
//! complete, tested implementation of the auth.json-rewrite half of
//! switching. `capabilities().supports_switching` is nonetheless `false`,
//! because v1's `State` (see `crate::store`) has no bucket for non-Claude
//! accounts at all — there is nowhere to persist a *second* Codex account's
//! secret blob to switch *to*, so `main.rs::switch_to` / `menubar.rs`'s
//! click dispatchers are hardcoded to the Claude slug and have no code path
//! that would ever call `write_active_account` today. Advertising
//! `supports_switching: true` here (as v0.5.0 originally shipped) built a
//! "Switch to this account" row that could never succeed. Flip this back to
//! `true` only once state v2 gives Codex accounts a real slot AND
//! `main.rs`/`menubar.rs` dispatch switches by provider instead of assuming
//! Claude.

#![allow(dead_code)]

pub mod oauth;

use std::path::PathBuf;

use base64::Engine;
use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;

use std::time::Duration;

use crate::providers::trait_def::{
    AccountKey, Capabilities, CaptureMode, CapturedAccount, CredentialFreshness, IdentitySnapshot,
    PResult, Provider, ProviderError, SecretBackend, TokenGrant, UsageSnapshot, UsageWindow,
};

/// Constructor called from `providers::build()` behind the `codex` feature.
pub fn new() -> Box<dyn Provider> {
    Box::new(CodexProvider)
}

pub struct CodexProvider;

/// Codex rate-limit / usage endpoint. The ChatGPT backend variant works for
/// tokens issued by the standard ChatGPT-plan OAuth flow the CLI uses.
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

impl Provider for CodexProvider {
    fn provider_id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_usage: true,
            // See the module doc TODO: `write_active_account` below is fully
            // implemented and tested, but nothing can call it yet because v1
            // state has no slot to store a second Codex account in. Keep
            // this `false` (and therefore the "Switch to this account" menu
            // row hidden) until that changes (H3, v0.5.0 codeaudit).
            supports_switching: false,
            supports_launch: false,
            supports_remove: true,
            supports_email_capture: true,
            secret_backend: SecretBackend::File,
            capture_mode: CaptureMode::CredsOnDisk,
        }
    }

    fn window_order(&self) -> &'static [&'static str] {
        &["primary", "secondary"]
    }

    /// Codex's refresh grant is a plain, programmatic HTTPS POST (no vendor
    /// CLI / browser step) — see `oauth.rs`'s module doc for the endpoint
    /// research. `oauth::active_refresh_cas` is the CAS-safe entry point a
    /// future proactive-refresh caller should use for the single active
    /// login; nothing in the credential-sync loop wires it up yet (that loop
    /// lives in `credentials.rs`, out of this provider's scope).
    fn supports_active_refresh(&self) -> bool {
        true
    }

    // --- Capture -----------------------------------------------------------

    fn capture_current_login(&self) -> PResult<Option<CapturedAccount>> {
        let Some(path) = auth_json_path() else {
            return Ok(None);
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(ProviderError::Io(e)),
        };
        let blob = String::from_utf8(bytes)
            .map_err(|e| ProviderError::Other(format!("auth.json is not UTF-8: {e}")))?;
        let parsed = parse_codex_blob(&blob)?;

        // The id_token JWT carries email + ChatGPT identifiers. Parsing is
        // pure-local: no network call.
        let id_claims = parsed
            .tokens
            .id_token
            .as_deref()
            .and_then(jwt_payload_claims)
            .unwrap_or_default();

        let email = id_claims
            .get("email")
            .and_then(|x| x.as_str())
            .map(String::from)
            .or_else(|| {
                id_claims
                    .get("https://api.openai.com/profile")
                    .and_then(|p| p.get("email"))
                    .and_then(|x| x.as_str())
                    .map(String::from)
            });
        let uuid = parsed.tokens.account_id.clone().or_else(|| {
            id_claims
                .get("https://api.openai.com/auth")
                .and_then(|p| p.get("chatgpt_account_id"))
                .and_then(|x| x.as_str())
                .map(String::from)
        });

        let native_blob = serde_json::json!({
            "account_id": parsed.tokens.account_id,
            "id_token_claims": id_claims,
        });

        let identity = IdentitySnapshot {
            email,
            uuid,
            display_name: None,
            native_blob,
        };
        let tokens = TokenGrant {
            access: parsed.tokens.access_token.clone(),
            refresh: parsed.tokens.refresh_token.clone(),
            // The vendor blob doesn't record `expires_in` directly; use the
            // JWT's `exp` claim minus `now` when present, else default to
            // "expired" so callers refresh eagerly.
            expires_in_secs: id_claims
                .get("exp")
                .and_then(|x| x.as_i64())
                .map(|exp| exp.saturating_sub(Utc::now().timestamp()))
                .unwrap_or(0),
        };
        Ok(Some(CapturedAccount {
            identity,
            secret_blob: blob,
            tokens,
        }))
    }

    // --- Token lifecycle ---------------------------------------------------

    fn parse_stored_blob(&self, blob: &str) -> PResult<TokenGrant> {
        let parsed = parse_codex_blob(blob)?;
        let id_claims = parsed
            .tokens
            .id_token
            .as_deref()
            .and_then(jwt_payload_claims)
            .unwrap_or_default();
        Ok(TokenGrant {
            access: parsed.tokens.access_token,
            refresh: parsed.tokens.refresh_token,
            expires_in_secs: id_claims
                .get("exp")
                .and_then(|x| x.as_i64())
                .map(|exp| exp.saturating_sub(Utc::now().timestamp()))
                .unwrap_or(0),
        })
    }

    fn patch_stored_blob(&self, blob: &str, grant: &TokenGrant) -> PResult<String> {
        let mut v: Value = serde_json::from_str(blob)
            .map_err(|e| ProviderError::Other(format!("auth.json is not valid JSON: {e}")))?;
        let obj = v
            .as_object_mut()
            .ok_or_else(|| ProviderError::Other("auth.json is not a JSON object".into()))?;
        // Ensure `tokens` exists as an object even if the caller handed us a
        // freshly-minted API-key-only blob.
        let tokens = obj
            .entry("tokens".to_string())
            .or_insert_with(|| Value::Object(Default::default()));
        let tokens_obj = tokens
            .as_object_mut()
            .ok_or_else(|| ProviderError::Other("auth.json `tokens` is not an object".into()))?;
        tokens_obj.insert("access_token".into(), Value::String(grant.access.clone()));
        // RFC 6749 §6: keep the caller-supplied refresh token if the caller
        // didn't rotate one. Mirrors Claude's behaviour.
        if let Some(rt) = grant.refresh.as_ref() {
            tokens_obj.insert("refresh_token".into(), Value::String(rt.clone()));
        }
        obj.insert(
            "last_refresh".into(),
            Value::String(Utc::now().to_rfc3339()),
        );
        Ok(v.to_string())
    }

    /// Refresh the access token via Codex's `/oauth/token` endpoint. See
    /// `oauth.rs`'s module doc for the endpoint, client_id, and single-use
    /// rotation semantics (mirrors Anthropic's: a stale refresh_token comes
    /// back `refresh_token_reused`/`invalid_grant`, never retryable).
    fn refresh_token(&self, refresh: &str) -> PResult<TokenGrant> {
        oauth::refresh_token_grant(refresh)
            .map(|g| g.into())
            .map_err(|e| match e {
                oauth::RefreshError::InvalidGrant => ProviderError::Auth,
                oauth::RefreshError::RateLimited => ProviderError::RateLimited {
                    retry_after_secs: None,
                },
                oauth::RefreshError::Transient(s) => ProviderError::Transient(s),
            })
    }

    // --- Usage -------------------------------------------------------------

    fn fetch_usage(&self, access_token: &str) -> PResult<UsageSnapshot> {
        let resp = ureq::get(USAGE_URL)
            .set("Authorization", &format!("Bearer {access_token}"))
            .set("Content-Type", "application/json")
            .call();
        let body: Value = match resp {
            Ok(r) => r
                .into_json()
                .map_err(|e| ProviderError::Other(format!("parsing codex usage: {e}")))?,
            Err(ureq::Error::Status(401, _)) => return Err(ProviderError::Auth),
            Err(ureq::Error::Status(429, _)) => {
                return Err(ProviderError::RateLimited {
                    retry_after_secs: None,
                })
            }
            Err(ureq::Error::Status(code, _)) => {
                return Err(ProviderError::Other(format!(
                    "codex usage endpoint returned HTTP {code}"
                )))
            }
            Err(e) => return Err(ProviderError::Transient(e.to_string())),
        };

        let mut windows: Vec<UsageWindow> = Vec::new();
        if let Some(rl) = body.get("rate_limits").and_then(|v| v.as_object()) {
            for id in ["primary", "secondary"] {
                if let Some(w) = rl.get(id).and_then(|v| v.as_object()) {
                    let utilization = w
                        .get("used_percent")
                        .and_then(|x| x.as_f64())
                        .or_else(|| w.get("used").and_then(|x| x.as_f64()));
                    // R2-P1 (round-2 codeaudit): use checked_add_signed so a
                    // hostile / buggy vendor JSON with resets_in_seconds near
                    // i64::MAX (or otherwise producing an out-of-range year)
                    // doesn't panic the refresh loop. Also clamp to ~400d,
                    // matching notifications::evaluate_pace.
                    let resets_at = w
                        .get("resets_in_seconds")
                        .and_then(|x| x.as_i64())
                        .and_then(|s| {
                            let capped = s.clamp(0, 60 * 60 * 24 * 400);
                            Utc::now().checked_add_signed(chrono::Duration::seconds(capped))
                        });
                    windows.push(UsageWindow {
                        id: id.to_string(),
                        label: if id == "primary" {
                            "5h".to_string()
                        } else {
                            "7d".to_string()
                        },
                        utilization,
                        resets_at,
                    });
                }
            }
        }
        Ok(UsageSnapshot {
            windows,
            fetched_at: Utc::now(),
        })
    }

    // --- Switching -----------------------------------------------------------

    /// Make `blob` the active Codex login by rewriting `auth.json` in place.
    /// `identity` is accepted for trait-signature parity with Claude (which
    /// needs it to rebuild `~/.claude.json`) but Codex has no analogous
    /// identity file, so it's unused here — the id_token JWT embedded in
    /// `blob` already carries everything `capture_current_login` needs to
    /// re-derive the identity on the next read.
    ///
    /// Validates that `blob` is a well-formed Codex auth blob (JSON, non-empty
    /// `tokens.access_token`) before writing, so a caller can't corrupt
    /// `auth.json` with garbage. Writes atomically (tmp file + rename) so a
    /// concurrent reader (the `codex` CLI, or our own credential-sync watcher)
    /// never observes a partially-written file.
    fn write_active_account(&self, blob: &str, _identity: &IdentitySnapshot) -> PResult<()> {
        // Defense in depth: an empty blob means the caller couldn't find a
        // stored secret for this account (shouldn't happen — state should
        // never hand us an account with no secret bytes — but distinguish it
        // from "malformed JSON" so callers can prompt a fresh login instead
        // of reporting a generic parse error).
        if blob.trim().is_empty() {
            return Err(ProviderError::NotLoggedIn);
        }
        // Sanity: refuse a blob that isn't a usable Codex auth.json rather
        // than corrupt the file the vendor CLI reads on every invocation.
        parse_codex_blob(blob)?;

        let path = auth_json_path().ok_or_else(|| {
            ProviderError::Other("could not resolve $CODEX_HOME / $HOME for auth.json".into())
        })?;
        write_auth_json_atomically(&path, blob)?;
        Ok(())
    }

    // `read_active_identity` and `launch_client` inherit the trait defaults,
    // which return `ProviderError::Unsupported`. Codex has no separate
    // "currently active identity" file to read independently of auth.json
    // (capture_current_login already covers that), and no wired-up client
    // launch yet.

    // --- Credential sync ---------------------------------------------------

    fn credential_paths(&self) -> Vec<PathBuf> {
        match auth_json_path() {
            Some(p) => vec![p],
            None => Vec::new(),
        }
    }

    /// Identify a Codex account from its auth.json blob by decoding the
    /// id_token JWT and reading the `email` claim.
    fn identify_credential(&self, blob: &str) -> Option<AccountKey> {
        let parsed: AuthDotJson = serde_json::from_str(blob).ok()?;
        let claims = parsed
            .tokens
            .id_token
            .as_deref()
            .and_then(jwt_payload_claims)?;
        let email = claims
            .get("email")
            .and_then(|x| x.as_str())
            .map(String::from)
            .or_else(|| {
                claims
                    .get("https://api.openai.com/profile")
                    .and_then(|p| p.get("email"))
                    .and_then(|x| x.as_str())
                    .map(String::from)
            })?;
        Some(AccountKey::new(self.provider_id(), email))
    }

    fn credential_freshness(&self, blob: &str) -> CredentialFreshness {
        let Ok(parsed) = serde_json::from_str::<AuthDotJson>(blob) else {
            return CredentialFreshness::Invalid;
        };
        let Some(claims) = parsed
            .tokens
            .id_token
            .as_deref()
            .and_then(jwt_payload_claims)
        else {
            return CredentialFreshness::Invalid;
        };
        let Some(exp) = claims.get("exp").and_then(|x| x.as_i64()) else {
            return CredentialFreshness::Unknown;
        };
        let now = Utc::now().timestamp();
        let remaining = exp.saturating_sub(now);
        if remaining <= 0 {
            CredentialFreshness::Expired
        } else if remaining < crate::credentials::REFRESH_SKEW_SECS {
            CredentialFreshness::ExpiresIn(Duration::from_secs(remaining as u64))
        } else {
            CredentialFreshness::Fresh
        }
    }

    /// Codex has no state.json slot yet (v1 stores only Claude accounts), so
    /// absorb is a no-op — the trait contract of "overwrite the account's
    /// slot" is a future-proof default we implement now for consistency.
    fn absorb_credential(&self, account: &AccountKey, _blob: &str) -> PResult<()> {
        if account.provider != self.provider_id() {
            return Ok(());
        }
        // Nothing to do until Codex accounts are first-class in state v2.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parsed shape of `auth.json` — only the fields this provider reads.
///
/// `pub(super)` (not private): `oauth.rs`'s CAS refresh needs to parse the
/// same shape (to pull `refresh_token` and check `last_refresh` staleness)
/// without duplicating this struct.
#[derive(Debug, Deserialize)]
pub(super) struct AuthDotJson {
    #[serde(default)]
    pub(super) tokens: TokenData,
    /// RFC 3339 timestamp of the last successful refresh. Absent on a blob
    /// that has never been refreshed since login. Used by `oauth.rs` to
    /// enforce the same ~8-day proactive-refresh cadence the vendor CLI uses
    /// independently of the access token's own expiry.
    #[serde(default)]
    pub(super) last_refresh: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct TokenData {
    #[serde(default)]
    pub(super) id_token: Option<String>,
    #[serde(default)]
    pub(super) access_token: String,
    #[serde(default)]
    pub(super) refresh_token: Option<String>,
    #[serde(default)]
    pub(super) account_id: Option<String>,
}

pub(super) fn parse_codex_blob(blob: &str) -> PResult<AuthDotJson> {
    let v: AuthDotJson = serde_json::from_str(blob)
        .map_err(|e| ProviderError::Other(format!("parsing codex auth.json: {e}")))?;
    if v.tokens.access_token.is_empty() {
        return Err(ProviderError::Other(
            "codex auth.json has no tokens.access_token (API-key-only blob)".into(),
        ));
    }
    Ok(v)
}

/// Resolve the Codex auth file path, honoring `$CODEX_HOME`. `pub(super)`
/// so `oauth.rs`'s CAS refresh resolves the same path this module uses for
/// capture/switching — a single source of truth for "where is auth.json".
pub(super) fn auth_json_path() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(h).join("auth.json"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".codex").join("auth.json"))
}

/// Map a filesystem `io::Result` into `PResult`, naming the operation and
/// path on failure. A bare `?` on `create_dir_all` / `set_permissions` /
/// `write` / `rename` collapses into `ProviderError::Io`, which `Display`s as
/// just `"io: <message>"` — no indication of which of the 4 operations, or
/// which path, actually failed. `ProviderError::Other` carries the full
/// annotated string instead.
fn fs_ctx<T>(r: std::io::Result<T>, op: &str, path: &std::path::Path) -> PResult<T> {
    r.map_err(|e| ProviderError::Other(format!("while {op} on {}: {e}", path.display())))
}

/// Write `contents` to `path` atomically (tmp file in the same directory +
/// rename), matching the permissions the `codex` CLI itself uses: parent
/// directory `0700` (created if missing), file `0600`. Rename is atomic on
/// the same filesystem, so a concurrent reader never observes a partial
/// write.
///
/// H1 (v0.5.0 codeaudit): the tmp file is created with `create_new` +
/// `mode(0o600)` in a single `open()` call — never `std::fs::write` followed
/// by a separate `set_permissions`. That two-step sequence has a TOCTOU
/// window where the file exists at the umask-default mode (typically 0644)
/// before the chmod lands, during which another local user could read the
/// OAuth tokens. Mirrors `crate::store::write_private`.
///
/// Any failure between creating the tmp file and the final rename removes
/// the tmp file so a secret-bearing orphan never survives (H4 finding from
/// the same audit).
#[cfg(unix)]
pub(super) fn write_auth_json_atomically(path: &std::path::Path, contents: &str) -> PResult<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let dir = path
        .parent()
        .ok_or_else(|| ProviderError::Other("auth.json path has no parent directory".into()))?;
    fs_ctx(std::fs::create_dir_all(dir), "create_dir_all", dir)?;
    fs_ctx(
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)),
        "set_permissions",
        dir,
    )?;

    // Tmp file lives in the same directory as the target so the rename below
    // is guaranteed to be on the same filesystem (atomic).
    let tmp_path = dir.join(format!(
        ".auth.json.tmp.{}.{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));

    // H1 (security codeaudit) + M9 (errors codeaudit) combined: create the
    // tmp file with mode 0600 via `OpenOptions::create_new(true).mode(0o600)`
    // in one syscall so no TOCTOU window exposes tokens to other local
    // users, and wrap the fs ops with `fs_ctx` so a failure reports which
    // operation on which path (not just "io: <msg>"). Clean up the tmp
    // file on any failure so a partial write doesn't leave a
    // secret-bearing orphan.
    let create_and_write = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)?;
        f.write_all(contents.as_bytes())
    })();
    if let Err(e) = fs_ctx(create_and_write, "open/write", &tmp_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Err(e) = fs_ctx(std::fs::rename(&tmp_path, path), "rename", &tmp_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn write_auth_json_atomically(path: &std::path::Path, contents: &str) -> PResult<()> {
    let dir = path
        .parent()
        .ok_or_else(|| ProviderError::Other("auth.json path has no parent directory".into()))?;
    fs_ctx(std::fs::create_dir_all(dir), "create_dir_all", dir)?;
    let tmp_path = dir.join(format!(
        ".auth.json.tmp.{}.{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    fs_ctx(std::fs::write(&tmp_path, contents), "write", &tmp_path)?;
    fs_ctx(std::fs::rename(&tmp_path, path), "rename", &tmp_path)?;
    Ok(())
}

/// Decode a JWT and return its payload claims as a JSON map. Signature is not
/// verified — Codex's id_token is bearer-signed by OpenAI; we're only reading
/// claims we would trust the provider to have written. `pub(super)` so
/// `oauth.rs` can compute `expires_in_secs` from a freshly-refreshed
/// `access_token`/`id_token` the same way capture does.
pub(super) fn jwt_payload_claims(jwt: &str) -> Option<serde_json::Map<String, Value>> {
    let mut parts = jwt.split('.');
    let _hdr = parts.next()?;
    let payload = parts.next()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .or_else(|| {
            // Some producers pad; try the padded variant too.
            base64::engine::general_purpose::URL_SAFE
                .decode(payload)
                .ok()
        })?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    match v {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_capabilities_are_locked() {
        let p = CodexProvider;
        assert_eq!(p.provider_id(), "codex");
        assert_eq!(p.display_name(), "Codex");
        let caps = p.capabilities();
        assert!(caps.supports_usage);
        // H3 (v0.5.0 codeaudit): `write_active_account` below is a real,
        // tested implementation, but `supports_switching` stays `false`
        // until state v2 gives Codex a place to store a second account to
        // switch to — see the module doc TODO. Advertising `true` here
        // built a menu row that could never succeed.
        assert!(!caps.supports_switching);
        assert!(!caps.supports_launch);
        assert!(caps.supports_remove);
        assert!(caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::File);
        assert_eq!(caps.capture_mode, CaptureMode::CredsOnDisk);
        assert_eq!(p.window_order(), &["primary", "secondary"]);
    }

    #[test]
    fn launch_client_is_still_unsupported() {
        // No wired-up client launch yet; only `write_active_account` (the
        // auth.json rewrite) implements switching so far.
        let p = CodexProvider;
        assert!(matches!(
            p.launch_client(crate::providers::trait_def::LaunchMode::Fresh),
            Err(ProviderError::Unsupported)
        ));
    }

    #[test]
    fn write_active_account_rejects_malformed_blob() {
        // A blob missing tokens.access_token must not be written to disk —
        // parse_codex_blob's validation runs before any filesystem write.
        let p = CodexProvider;
        let id = IdentitySnapshot {
            email: None,
            uuid: None,
            display_name: None,
            native_blob: Value::Null,
        };
        let dir = tempfile::tempdir().unwrap();
        crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
            let bad_blob = serde_json::json!({ "tokens": { "refresh_token": "rt" } }).to_string();
            assert!(p.write_active_account(&bad_blob, &id).is_err());
            assert!(!dir.path().join("auth.json").exists());
        });
    }

    #[test]
    fn switch_account_writes_auth_json_atomically() {
        let p = CodexProvider;
        let id = IdentitySnapshot {
            email: Some("switcher@example.com".into()),
            uuid: None,
            display_name: None,
            native_blob: Value::Null,
        };
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join("codex-home-nonexistent");
        crate::env_lock::scoped_env_var("CODEX_HOME", Some(codex_home.to_str().unwrap()), || {
            let blob = make_blob("switcher@example.com", Utc::now().timestamp() + 3600);
            p.write_active_account(&blob, &id).unwrap();

            let auth_path = codex_home.join("auth.json");
            assert!(auth_path.exists());
            let on_disk = std::fs::read_to_string(&auth_path).unwrap();
            assert_eq!(on_disk, blob);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&auth_path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
                let dir_mode = std::fs::metadata(&codex_home).unwrap().permissions().mode();
                assert_eq!(dir_mode & 0o777, 0o700);
            }
        });
    }

    #[test]
    #[cfg(unix)]
    fn write_auth_json_atomically_names_the_failing_operation() {
        // Skip under root (e.g. some CI containers): root ignores the 0o000
        // permission bit this test relies on to force create_dir_all to fail.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, permission bits are not enforced");
            return;
        }
        use std::os::unix::fs::PermissionsExt;

        let base = tempfile::tempdir().unwrap();
        let readonly = base.path().join("readonly-parent");
        std::fs::create_dir(&readonly).unwrap();
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o000)).unwrap();

        // `dir` (readonly-parent/subdir) doesn't exist yet, and its parent
        // has no write permission, so `create_dir_all` must fail here.
        let target = readonly.join("subdir").join("auth.json");
        let err = write_auth_json_atomically(&target, "{}")
            .expect_err("create_dir_all under a read-only parent must fail");
        let msg = err.to_string();

        // Restore permissions so the tempdir can be cleaned up regardless of
        // the assertion outcome below.
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            msg.contains("create_dir_all"),
            "error should name the failing operation: {msg}"
        );
        assert!(
            msg.contains("subdir"),
            "error should include the failing path: {msg}"
        );
    }

    #[test]
    fn switch_account_round_trips_through_absorb() {
        // Switch to A, absorb() the file we just wrote → identify_credential
        // reports A. Switch to B, absorb again → reports B. This is the
        // guarantee `credentials::absorb_before_switch` / the fsnotify
        // watcher depend on: whatever we just wrote is exactly what a fresh
        // read (and identity extraction) produces.
        let p = CodexProvider;
        let dir = tempfile::tempdir().unwrap();
        crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
            let id_a = IdentitySnapshot {
                email: Some("a@example.com".into()),
                uuid: None,
                display_name: None,
                native_blob: Value::Null,
            };
            let blob_a = make_blob("a@example.com", Utc::now().timestamp() + 3600);
            p.write_active_account(&blob_a, &id_a).unwrap();
            let on_disk_a = std::fs::read_to_string(dir.path().join("auth.json")).unwrap();
            let key_a = p.identify_credential(&on_disk_a).unwrap();
            assert_eq!(key_a, AccountKey::new("codex", "a@example.com"));

            let id_b = IdentitySnapshot {
                email: Some("b@example.com".into()),
                uuid: None,
                display_name: None,
                native_blob: Value::Null,
            };
            let blob_b = make_blob("b@example.com", Utc::now().timestamp() + 3600);
            p.write_active_account(&blob_b, &id_b).unwrap();
            let on_disk_b = std::fs::read_to_string(dir.path().join("auth.json")).unwrap();
            let key_b = p.identify_credential(&on_disk_b).unwrap();
            assert_eq!(key_b, AccountKey::new("codex", "b@example.com"));
        });
    }

    #[test]
    #[cfg(unix)]
    fn write_auth_json_creates_with_mode_0600_atomically() {
        // H1: the file must never be observable at the umask-default mode —
        // it must be created 0600 in the same syscall that creates it, not
        // written then chmod'd afterwards.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_json_atomically(&path, r#"{"tokens":{"access_token":"at"}}"#).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn write_auth_json_cleans_up_tmp_file_on_rename_failure() {
        // H1/H4: if the final rename fails (simulated here by pointing the
        // target at a path whose parent doesn't exist so create_dir_all
        // succeeds but we then swap the target to a directory, forcing
        // rename() to fail with EISDIR), the tmp file must not survive.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("auth.json");
        // Make the rename destination itself a non-empty directory so
        // std::fs::rename(file -> dir) fails.
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keepme"), b"x").unwrap();

        let result = write_auth_json_atomically(&target, r#"{"tokens":{"access_token":"at"}}"#);
        assert!(result.is_err());

        // No leftover `.auth.json.tmp.*` files in the directory.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".auth.json.tmp.")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "orphaned tmp file(s) left behind: {leftovers:?}"
        );
    }

    #[test]
    fn switch_account_missing_secret_errors() {
        // Defense in depth: an empty/absent secret blob (the caller couldn't
        // find one in state) must surface a clean error, not panic or write
        // garbage to auth.json.
        let p = CodexProvider;
        let id = IdentitySnapshot {
            email: None,
            uuid: None,
            display_name: None,
            native_blob: Value::Null,
        };
        let dir = tempfile::tempdir().unwrap();
        crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
            let err = p.write_active_account("", &id).unwrap_err();
            assert!(matches!(err, ProviderError::NotLoggedIn));
            assert!(!dir.path().join("auth.json").exists());
        });
    }

    #[test]
    fn parse_stored_blob_extracts_tokens() {
        let blob = serde_json::json!({
            "tokens": {
                "access_token": "at",
                "refresh_token": "rt",
                "account_id": "acc-1",
            }
        })
        .to_string();
        let g = CodexProvider.parse_stored_blob(&blob).unwrap();
        assert_eq!(g.access, "at");
        assert_eq!(g.refresh.as_deref(), Some("rt"));
    }

    #[test]
    fn parse_stored_blob_rejects_missing_access_token() {
        let blob = serde_json::json!({
            "tokens": { "refresh_token": "rt" }
        })
        .to_string();
        assert!(matches!(
            CodexProvider.parse_stored_blob(&blob),
            Err(ProviderError::Other(_))
        ));
    }

    #[test]
    fn patch_stored_blob_updates_access_and_preserves_refresh() {
        let blob = serde_json::json!({
            "tokens": {
                "access_token": "old",
                "refresh_token": "keep-me",
            }
        })
        .to_string();
        let grant = TokenGrant {
            access: "new".into(),
            refresh: None,
            expires_in_secs: 60,
        };
        let out = CodexProvider.patch_stored_blob(&blob, &grant).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let t = v.get("tokens").unwrap();
        assert_eq!(t.get("access_token").and_then(|x| x.as_str()), Some("new"));
        assert_eq!(
            t.get("refresh_token").and_then(|x| x.as_str()),
            Some("keep-me")
        );
    }

    fn make_id_token(email: &str, exp: i64) -> String {
        let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::json!({ "email": email, "exp": exp })
                .to_string()
                .as_bytes(),
        );
        format!("{hdr}.{payload}.sig")
    }

    fn make_blob(email: &str, exp: i64) -> String {
        let id_token = make_id_token(email, exp);
        serde_json::json!({
            "tokens": {
                "id_token": id_token,
                "access_token": "at",
                "refresh_token": "rt"
            }
        })
        .to_string()
    }

    #[test]
    fn identify_credential_extracts_email_from_id_token() {
        let blob = make_blob("user@example.com", 4_000_000_000);
        let k = CodexProvider.identify_credential(&blob).unwrap();
        assert_eq!(k.provider, "codex");
        assert_eq!(k.key, "user@example.com");
    }

    #[test]
    fn identify_credential_returns_none_without_id_token() {
        // No id_token in tokens => no way to identify.
        let blob = serde_json::json!({
            "tokens": { "access_token": "at", "refresh_token": "rt" }
        })
        .to_string();
        assert!(CodexProvider.identify_credential(&blob).is_none());
    }

    #[test]
    fn credential_freshness_reports_fresh_for_far_future_exp() {
        let blob = make_blob("u@e.com", Utc::now().timestamp() + 3600);
        assert_eq!(
            CodexProvider.credential_freshness(&blob),
            CredentialFreshness::Fresh
        );
    }

    #[test]
    fn credential_freshness_reports_expired_for_past_exp() {
        let blob = make_blob("u@e.com", 1);
        assert_eq!(
            CodexProvider.credential_freshness(&blob),
            CredentialFreshness::Expired
        );
    }

    #[test]
    fn credential_paths_uses_codex_home_when_set() {
        // Serialised across the crate via `env_lock::ENV_LOCK` — no other
        // test can flip `$CODEX_HOME` mid-assertion.
        crate::env_lock::scoped_env_var("CODEX_HOME", Some("/tmp/codex-fixture"), || {
            let paths = CodexProvider.credential_paths();
            assert_eq!(paths.len(), 1);
            assert!(paths[0].ends_with("auth.json"));
            assert!(paths[0].starts_with("/tmp/codex-fixture"));
        });
    }

    #[test]
    fn absorb_credential_for_wrong_provider_is_noop() {
        // absorb_credential must ignore keys belonging to a different
        // provider — never touch Claude's slot from Codex or vice versa.
        let k = AccountKey::new("claude", "x@e.com");
        assert!(CodexProvider.absorb_credential(&k, "irrelevant").is_ok());
    }

    #[test]
    fn jwt_payload_claims_decodes_email() {
        // {"alg":"none"}.{"email":"user@example.com","exp":1234567890}
        let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"email":"user@example.com","exp":1234567890}"#);
        let jwt = format!("{hdr}.{payload}.sig");
        let claims = jwt_payload_claims(&jwt).unwrap();
        assert_eq!(
            claims.get("email").and_then(|x| x.as_str()),
            Some("user@example.com")
        );
        assert_eq!(claims.get("exp").and_then(|x| x.as_i64()), Some(1234567890));
    }
}
