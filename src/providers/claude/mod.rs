//! Claude provider — first real `Provider` implementation.
//!
//! This module ports the existing Claude-specific logic that today lives as
//! free functions in `oauth`, `usage`, `crate::store`, and `crate::main`,
//! wrapping it behind the `Provider` trait so later phases can route through
//! `providers::get("claude")` uniformly.
//!
//! The CLI and menubar still call the free functions in `oauth` / `usage`
//! (re-exported at the crate root for backwards compatibility with `main.rs`);
//! deletion of that call path happens in a later phase.
//!
//! All existing tests keep passing because nothing here is on the pre-existing
//! call graph; the only new callers are the trait-level unit tests below.

// The trait implementation is only reachable through the registry, which is
// not yet wired into any command handler. Silence dead-code warnings until
// later phases start routing through `providers::get`.
#![allow(dead_code)]

pub mod config;
pub mod oauth;
pub mod usage;

use std::process::ExitStatus;

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::providers::trait_def::{
    Capabilities, CaptureMode, CapturedAccount, IdentitySnapshot, LaunchMode, PResult, Provider,
    ProviderError, SecretBackend, TokenGrant, UsageSnapshot, UsageWindow,
};

/// Keychain generic-password service Claude Code writes to. Duplicated from
/// `main.rs` so this module has no dependency on the legacy CLI internals;
/// the two constants MUST stay in lock-step until the legacy path is deleted.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Constructor called from `providers::build()` behind the `claude` feature.
pub fn new() -> Box<dyn Provider> {
    Box::new(ClaudeProvider)
}

pub struct ClaudeProvider;

impl Provider for ClaudeProvider {
    fn provider_id(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_usage: true,
            supports_switching: true,
            supports_email_capture: true,
            secret_backend: SecretBackend::Keychain,
            capture_mode: CaptureMode::CredsOnDisk,
        }
    }

    fn window_order(&self) -> &'static [&'static str] {
        &["session", "weekly", "opus"]
    }

    // --- Capture / listing --------------------------------------------------

    fn capture_current_login(&self) -> PResult<Option<CapturedAccount>> {
        let Some(blob) = keychain_read() else {
            return Ok(None);
        };
        let (access, refresh, expires_at) = parse_claude_blob(&blob)?;

        // Snapshot the identity Claude Code stores alongside the keychain
        // token in ~/.claude.json (used to fully restore an account on a
        // later switch — the token alone doesn't set who is signed in).
        let (oauth_account, user_id) = read_claude_identity();

        // Resolve the email: profile endpoint if online, else fall back to
        // the identity we just read from ~/.claude.json.
        let email = usage::fetch_email(&access).or_else(|| {
            oauth_account
                .as_ref()
                .and_then(|o| o.get("emailAddress"))
                .and_then(|x| x.as_str())
                .map(String::from)
        });
        let uuid = oauth_account
            .as_ref()
            .and_then(|o| o.get("accountUuid"))
            .and_then(|x| x.as_str())
            .map(String::from);
        let display_name = oauth_account
            .as_ref()
            .and_then(|o| o.get("displayName"))
            .and_then(|x| x.as_str())
            .map(String::from);

        // Pack both the vendor oauthAccount blob and the userID into one
        // native_blob so `write_active_account` can fully restore
        // ~/.claude.json without needing state fields the trait doesn't
        // expose. The `oauthAccount` field mirrors Claude Code's own shape.
        let native_blob = serde_json::json!({
            "oauthAccount": oauth_account.unwrap_or(Value::Null),
            "userID": user_id,
        });

        let identity = IdentitySnapshot {
            email,
            uuid,
            display_name,
            native_blob,
        };
        let tokens = TokenGrant {
            access,
            refresh: Some(refresh),
            expires_in_secs: expires_in_secs_from_epoch_millis(expires_at),
        };
        Ok(Some(CapturedAccount {
            identity,
            secret_blob: blob,
            tokens,
        }))
    }

    // --- Token lifecycle ----------------------------------------------------

    fn parse_stored_blob(&self, blob: &str) -> PResult<TokenGrant> {
        let (access, refresh, expires_at) = parse_claude_blob(blob)?;
        Ok(TokenGrant {
            access,
            refresh: Some(refresh),
            expires_in_secs: expires_in_secs_from_epoch_millis(expires_at),
        })
    }

    fn patch_stored_blob(&self, blob: &str, grant: &TokenGrant) -> PResult<String> {
        let mut v: Value = serde_json::from_str(blob)
            .map_err(|e| ProviderError::Other(format!("keychain value is not valid JSON: {e}")))?;
        let obj = v
            .get_mut("claudeAiOauth")
            .and_then(|x| x.as_object_mut())
            .ok_or_else(|| {
                ProviderError::Other(
                    "keychain value has no claudeAiOauth object (not a claude.ai login)".into(),
                )
            })?;
        let now_ms = Utc::now().timestamp_millis();
        let expires_at = now_ms.saturating_add(grant.expires_in_secs.saturating_mul(1000));
        obj.insert(
            "accessToken".into(),
            Value::String(grant.access.clone()),
        );
        // Preserve the existing refresh token if the server didn't rotate a
        // fresh one — mirrors `oauth::refresh`'s RFC 6749 §6 behaviour.
        if let Some(rt) = grant.refresh.as_ref() {
            obj.insert("refreshToken".into(), Value::String(rt.clone()));
        }
        obj.insert("expiresAt".into(), Value::from(expires_at));
        Ok(v.to_string())
    }

    fn refresh_token(&self, refresh: &str) -> PResult<TokenGrant> {
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            #[serde(default)]
            refresh_token: Option<String>,
            expires_in: i64,
        }

        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh,
            "client_id": config::CLIENT_ID,
        });
        let resp = ureq::post(config::TOKEN_URL)
            .set("Content-Type", "application/json")
            .set("anthropic-beta", config::OAUTH_BETA)
            .send_json(body);
        let tok: TokenResponse = match resp {
            Ok(r) => r
                .into_json()
                .map_err(|e| ProviderError::Other(format!("parsing token response: {e}")))?,
            Err(ureq::Error::Status(401, _)) => return Err(ProviderError::Auth),
            Err(ureq::Error::Status(429, _)) => {
                return Err(ProviderError::RateLimited {
                    retry_after_secs: None,
                })
            }
            Err(ureq::Error::Status(code, _)) => {
                return Err(ProviderError::Other(format!(
                    "token endpoint returned HTTP {code}"
                )))
            }
            Err(e) => return Err(ProviderError::Transient(e.to_string())),
        };
        Ok(TokenGrant {
            access: tok.access_token,
            // RFC 6749 §6: keep the caller-supplied refresh token if the
            // server didn't rotate one.
            refresh: Some(
                tok.refresh_token
                    .unwrap_or_else(|| refresh.to_string()),
            ),
            expires_in_secs: tok.expires_in,
        })
    }

    // --- Usage --------------------------------------------------------------

    fn fetch_usage(&self, access_token: &str) -> PResult<UsageSnapshot> {
        let u = usage::fetch(access_token).map_err(map_usage_error)?;
        let mut windows: Vec<UsageWindow> = Vec::new();
        if let Some(w) = u.five_hour {
            windows.push(claude_window("session", "5h", &w));
        }
        if let Some(w) = u.seven_day {
            windows.push(claude_window("weekly", "7d", &w));
        }
        if let Some(w) = u.seven_day_opus {
            windows.push(claude_window("opus", "Opus 7d", &w));
        }
        Ok(UsageSnapshot {
            windows,
            fetched_at: Utc::now(),
        })
    }

    // --- Switching ----------------------------------------------------------

    fn write_active_account(
        &self,
        blob: &str,
        identity: &IdentitySnapshot,
    ) -> PResult<()> {
        // Sanity: refuse a non-Claude blob rather than corrupt the keychain.
        parse_claude_blob(blob)?;

        // Unpack the paired {oauthAccount, userID} we stashed at capture time.
        // Both fields are optional; missing ones fall through to the same
        // remove-key semantics the legacy path uses.
        let oauth_account = identity
            .native_blob
            .get("oauthAccount")
            .cloned()
            .filter(|x| !x.is_null());
        let user_id = identity
            .native_blob
            .get("userID")
            .and_then(|x| x.as_str())
            .map(String::from);

        let oauth_account = oauth_account.ok_or_else(|| {
            ProviderError::Other(
                "identity native_blob has no oauthAccount object; recapture the account".into(),
            )
        })?;

        // ~/.claude.json identity first, keychain last (the flaky commit
        // point). If the keychain write fails, roll ~/.claude.json back.
        let prior = read_claude_json_raw();
        write_claude_identity(&oauth_account, user_id.as_deref())
            .map_err(|e| ProviderError::Other(e.to_string()))?;
        if let Err(e) = keychain_write(blob) {
            if let Some((bytes, mode)) = &prior {
                let _ = restore_claude_json_raw(bytes, *mode);
            }
            return Err(e);
        }
        Ok(())
    }

    fn read_active_identity(&self) -> PResult<Option<IdentitySnapshot>> {
        let (oauth_account, user_id) = read_claude_identity();
        let Some(oa) = oauth_account else {
            return Ok(None);
        };
        let email = oa
            .get("emailAddress")
            .and_then(|x| x.as_str())
            .map(String::from);
        let uuid = oa
            .get("accountUuid")
            .and_then(|x| x.as_str())
            .map(String::from);
        let display_name = oa
            .get("displayName")
            .and_then(|x| x.as_str())
            .map(String::from);
        let native_blob = serde_json::json!({
            "oauthAccount": oa,
            "userID": user_id,
        });
        Ok(Some(IdentitySnapshot {
            email,
            uuid,
            display_name,
            native_blob,
        }))
    }

    fn launch_client(&self, mode: LaunchMode) -> PResult<ExitStatus> {
        let mut cmd = std::process::Command::new("claude");
        if matches!(mode, LaunchMode::Continue) {
            cmd.arg("--continue");
        }
        cmd.status().map_err(ProviderError::Io)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (duplicated from main.rs / store.rs / usage.rs so this
// module is self-contained until the legacy free-function path is deleted).
// ---------------------------------------------------------------------------

/// Decode a Claude keychain blob (`{"claudeAiOauth": {...}}`) into
/// `(access_token, refresh_token, expires_at_epoch_millis)`.
fn parse_claude_blob(blob: &str) -> PResult<(String, String, i64)> {
    let v: Value = serde_json::from_str(blob)
        .map_err(|e| ProviderError::Other(format!("keychain value is not valid JSON: {e}")))?;
    let o = v.get("claudeAiOauth").ok_or_else(|| {
        ProviderError::Other(
            "keychain value has no claudeAiOauth object (not a claude.ai login)".into(),
        )
    })?;
    let access = o
        .get("accessToken")
        .and_then(|x| x.as_str())
        .ok_or_else(|| ProviderError::Other("no accessToken in keychain value".into()))?
        .to_string();
    let refresh = o
        .get("refreshToken")
        .and_then(|x| x.as_str())
        .ok_or_else(|| ProviderError::Other("no refreshToken in keychain value".into()))?
        .to_string();
    let expires_at = o.get("expiresAt").and_then(|x| x.as_i64()).unwrap_or(0);
    Ok((access, refresh, expires_at))
}

/// Convert an absolute expiry (epoch millis, as Claude stores it) into the
/// relative `expires_in_secs` shape the trait uses. Saturating so a corrupt
/// value near i64::MIN reads as "expired" rather than overflowing.
fn expires_in_secs_from_epoch_millis(expires_at: i64) -> i64 {
    let now_ms = Utc::now().timestamp_millis();
    expires_at.saturating_sub(now_ms) / 1000
}

fn claude_window(id: &'static str, label: &'static str, w: &usage::Window) -> UsageWindow {
    UsageWindow {
        id: id.to_string(),
        label: label.to_string(),
        utilization: w.utilization,
        resets_at: w
            .resets_at
            .as_deref()
            .and_then(parse_rfc3339_utc),
    }
}

fn parse_rfc3339_utc(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            // Fall back to a permissive integer-seconds parse for legacy
            // callers; not exercised by Claude today but harmless.
            s.parse::<i64>()
                .ok()
                .and_then(|secs| Utc.timestamp_opt(secs, 0).single())
        })
}

fn map_usage_error(e: usage::FetchError) -> ProviderError {
    match e {
        usage::FetchError::RateLimited => ProviderError::RateLimited {
            retry_after_secs: None,
        },
        usage::FetchError::Auth => ProviderError::Auth,
        usage::FetchError::Transient(s) => ProviderError::Transient(s),
        usage::FetchError::Other(s) => ProviderError::Other(s),
    }
}

// ---- Keychain (macOS: `security` CLI; other OSes: unsupported) -----------

fn keychain_account() -> String {
    std::env::var("USER").unwrap_or_else(|_| "claude".to_string())
}

#[cfg(target_os = "macos")]
fn keychain_read() -> Option<String> {
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &keychain_account(),
            "-w",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(target_os = "macos")]
fn keychain_write(blob: &str) -> PResult<()> {
    let status = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &keychain_account(),
            "-w",
            blob,
        ])
        .status()
        .map_err(ProviderError::Io)?;
    if !status.success() {
        return Err(ProviderError::Other(
            "`security add-generic-password` failed".into(),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn keychain_read() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
fn keychain_write(_blob: &str) -> PResult<()> {
    Err(ProviderError::Other(
        "keychain is only supported on macOS".into(),
    ))
}

// ---- ~/.claude.json identity read/write (mirrors main.rs helpers) --------

fn claude_json_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".claude.json"))
}

fn read_claude_identity() -> (Option<Value>, Option<String>) {
    let Some(path) = claude_json_path() else {
        return (None, None);
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return (None, None);
    };
    let oauth = v.get("oauthAccount").cloned().filter(|x| !x.is_null());
    let uid = v.get("userID").and_then(|u| u.as_str()).map(String::from);
    (oauth, uid)
}

fn read_claude_json_raw() -> Option<(Vec<u8>, u32)> {
    let path = claude_json_path()?;
    let bytes = std::fs::read(&path).ok()?;
    let mode = claude_json_mode(&path);
    Some((bytes, mode))
}

#[cfg(unix)]
fn claude_json_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o600)
}

#[cfg(not(unix))]
fn claude_json_mode(_path: &std::path::Path) -> u32 {
    0o600
}

fn restore_claude_json_raw(bytes: &[u8], mode: u32) -> PResult<()> {
    let path = claude_json_path()
        .ok_or_else(|| ProviderError::Other("HOME is not set".into()))?;
    write_bytes_atomic_mode(&path, bytes, mode)
}

fn write_bytes_atomic_mode(
    path: &std::path::Path,
    bytes: &[u8],
    mode: u32,
) -> PResult<()> {
    let tmp = path.with_extension("json.claude-usage.tmp");
    if let Err(e) = crate::store::write_private(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ProviderError::Io(e));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = mode;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ProviderError::Io(e));
    }
    Ok(())
}

fn write_claude_identity(oauth_account: &Value, user_id: Option<&str>) -> PResult<()> {
    let path = claude_json_path()
        .ok_or_else(|| ProviderError::Other("HOME is not set".into()))?;
    let bytes = std::fs::read(&path).map_err(ProviderError::Io)?;
    let mut v: Value = serde_json::from_slice(&bytes)
        .map_err(|e| ProviderError::Other(format!("parsing ~/.claude.json: {e}")))?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| ProviderError::Other("~/.claude.json is not a JSON object".into()))?;
    obj.insert("oauthAccount".into(), oauth_account.clone());
    match user_id {
        Some(uid) => {
            obj.insert("userID".into(), Value::String(uid.to_string()));
        }
        None => {
            obj.remove("userID");
        }
    }
    obj.remove("cachedUsageUtilization");
    let json = serde_json::to_vec_pretty(&v)
        .map_err(|e| ProviderError::Other(format!("serializing ~/.claude.json: {e}")))?;
    let mode = claude_json_mode(&path);
    write_bytes_atomic_mode(&path, &json, mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_capabilities_are_locked() {
        let p = ClaudeProvider;
        assert_eq!(p.provider_id(), "claude");
        assert_eq!(p.display_name(), "Claude");
        let caps = p.capabilities();
        assert!(caps.supports_usage);
        assert!(caps.supports_switching);
        assert!(caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
        assert_eq!(p.window_order(), &["session", "weekly", "opus"]);
    }

    #[test]
    fn parse_stored_blob_extracts_tokens() {
        let blob = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "at",
                "refreshToken": "rt",
                "expiresAt": Utc::now().timestamp_millis() + 3_600_000,
            }
        })
        .to_string();
        let g = ClaudeProvider.parse_stored_blob(&blob).unwrap();
        assert_eq!(g.access, "at");
        assert_eq!(g.refresh.as_deref(), Some("rt"));
        assert!(g.expires_in_secs > 3500 && g.expires_in_secs <= 3600);
    }

    #[test]
    fn parse_stored_blob_rejects_non_claude_json() {
        assert!(matches!(
            ClaudeProvider.parse_stored_blob("{}"),
            Err(ProviderError::Other(_))
        ));
    }

    #[test]
    fn patch_stored_blob_updates_all_three_fields() {
        let blob = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "old",
                "refreshToken": "old-rt",
                "expiresAt": 0i64,
            }
        })
        .to_string();
        let grant = TokenGrant {
            access: "new".into(),
            refresh: Some("new-rt".into()),
            expires_in_secs: 1800,
        };
        let out = ClaudeProvider.patch_stored_blob(&blob, &grant).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let o = v.get("claudeAiOauth").unwrap();
        assert_eq!(o.get("accessToken").and_then(|x| x.as_str()), Some("new"));
        assert_eq!(
            o.get("refreshToken").and_then(|x| x.as_str()),
            Some("new-rt")
        );
        let expires_at = o.get("expiresAt").and_then(|x| x.as_i64()).unwrap();
        let now_ms = Utc::now().timestamp_millis();
        // ~30 minutes from now, allowing a generous window for slow CI.
        assert!(expires_at >= now_ms + 1_700_000 && expires_at <= now_ms + 1_900_000);
    }

    #[test]
    fn patch_stored_blob_preserves_refresh_when_grant_omits_it() {
        let blob = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "old",
                "refreshToken": "keep-me",
                "expiresAt": 0i64,
            }
        })
        .to_string();
        let grant = TokenGrant {
            access: "new".into(),
            refresh: None,
            expires_in_secs: 60,
        };
        let out = ClaudeProvider.patch_stored_blob(&blob, &grant).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v.get("claudeAiOauth")
                .and_then(|o| o.get("refreshToken"))
                .and_then(|x| x.as_str()),
            Some("keep-me")
        );
    }
}
