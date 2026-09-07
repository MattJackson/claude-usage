//! Codex (OpenAI) provider — USAGE ONLY, NO SWITCHING.
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
//! Per the refactor's locked decisions, this provider implements capture +
//! usage but explicitly refuses `write_active_account` / `launch_client` with
//! `ProviderError::Unsupported`. A future phase adds switching (auth.json
//! rewrite + optional `CODEX_HOME` multiplex).

#![allow(dead_code)]

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
            supports_switching: false,
            supports_email_capture: true,
            secret_backend: SecretBackend::File,
            capture_mode: CaptureMode::CredsOnDisk,
        }
    }

    fn window_order(&self) -> &'static [&'static str] {
        &["primary", "secondary"]
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
                    let resets_at = w
                        .get("resets_in_seconds")
                        .and_then(|x| x.as_i64())
                        .map(|s| Utc::now() + chrono::Duration::seconds(s));
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

    // --- Switching (explicitly Unsupported for v1) -------------------------
    // `write_active_account`, `read_active_identity`, and `launch_client`
    // inherit the trait defaults, which return `ProviderError::Unsupported`.
    // Do not override them until the switching phase lands.

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
        let claims = parsed.tokens.id_token.as_deref().and_then(jwt_payload_claims)?;
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
        let Some(claims) = parsed.tokens.id_token.as_deref().and_then(jwt_payload_claims) else {
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
#[derive(Debug, Deserialize)]
struct AuthDotJson {
    #[serde(default)]
    tokens: TokenData,
}

#[derive(Debug, Default, Deserialize)]
struct TokenData {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

fn parse_codex_blob(blob: &str) -> PResult<AuthDotJson> {
    let v: AuthDotJson = serde_json::from_str(blob)
        .map_err(|e| ProviderError::Other(format!("parsing codex auth.json: {e}")))?;
    if v.tokens.access_token.is_empty() {
        return Err(ProviderError::Other(
            "codex auth.json has no tokens.access_token (API-key-only blob)".into(),
        ));
    }
    Ok(v)
}

/// Resolve the Codex auth file path, honoring `$CODEX_HOME`.
fn auth_json_path() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(h).join("auth.json"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".codex").join("auth.json"))
}

/// Decode a JWT and return its payload claims as a JSON map. Signature is not
/// verified — Codex's id_token is bearer-signed by OpenAI; we're only reading
/// claims we would trust the provider to have written.
fn jwt_payload_claims(jwt: &str) -> Option<serde_json::Map<String, Value>> {
    let mut parts = jwt.split('.');
    let _hdr = parts.next()?;
    let payload = parts.next()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .or_else(|| {
            // Some producers pad; try the padded variant too.
            base64::engine::general_purpose::URL_SAFE.decode(payload).ok()
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
        assert!(!caps.supports_switching);
        assert!(caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::File);
        assert_eq!(p.window_order(), &["primary", "secondary"]);
    }

    #[test]
    fn switching_methods_return_unsupported() {
        let p = CodexProvider;
        let id = IdentitySnapshot {
            email: None,
            uuid: None,
            display_name: None,
            native_blob: Value::Null,
        };
        assert!(matches!(
            p.write_active_account("{}", &id),
            Err(ProviderError::Unsupported)
        ));
        assert!(matches!(
            p.launch_client(crate::providers::trait_def::LaunchMode::Fresh),
            Err(ProviderError::Unsupported)
        ));
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
        // Save + restore any existing value so we don't leak into other tests.
        // std::env is process-global — this test is #[test] not #[test(serial)],
        // but the assertion checks the joined path shape regardless of ordering.
        let prior = std::env::var_os("CODEX_HOME");
        std::env::set_var("CODEX_HOME", "/tmp/codex-fixture");
        let paths = CodexProvider.credential_paths();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("auth.json"));
        assert!(paths[0].starts_with("/tmp/codex-fixture"));
        match prior {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }
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
