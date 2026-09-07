//! Provider trait scaffolding.
//!
//! This is the core-agents-refactor v1 trait definition: one `Box<dyn Provider>`
//! per module, registered by `providers::init()` in `mod.rs`. Nothing calls
//! into these types yet — the trait, error, and shared value types simply
//! exist so the per-provider modules can start landing in later phases.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::ExitStatus;

/// Errors returned from any `Provider` method. Kept small and structured so
/// menu code can decide how to surface each variant (silence, retry, warn,
/// disable auto-swap) without string-matching.
#[derive(Debug)]
pub enum ProviderError {
    /// The provider does not implement this operation. Default trait methods
    /// return this so callers can distinguish "not yet built" from "failed".
    Unsupported,
    /// No captured account / stored token for this provider on this host.
    NotLoggedIn,
    /// Provider quota endpoint told us to back off.
    RateLimited { retry_after_secs: Option<u64> },
    /// Credentials refused / expired / revoked upstream.
    Auth,
    /// Transient network / server error worth retrying with backoff.
    Transient(String),
    /// Local filesystem / keychain / sqlite IO error.
    Io(std::io::Error),
    /// Anything else; carries a short human string for logs.
    Other(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::Unsupported => write!(f, "unsupported"),
            ProviderError::NotLoggedIn => write!(f, "not logged in"),
            ProviderError::RateLimited { retry_after_secs } => match retry_after_secs {
                Some(s) => write!(f, "rate limited (retry after {}s)", s),
                None => write!(f, "rate limited"),
            },
            ProviderError::Auth => write!(f, "auth error"),
            ProviderError::Transient(s) => write!(f, "transient: {}", s),
            ProviderError::Io(e) => write!(f, "io: {}", e),
            ProviderError::Other(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for ProviderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProviderError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ProviderError {
    fn from(e: std::io::Error) -> Self {
        ProviderError::Io(e)
    }
}

pub type PResult<T> = std::result::Result<T, ProviderError>;

/// OAuth-style token grant, decoded to the shape the core needs for refresh
/// scheduling. Providers translate their own on-disk blob to/from this shape
/// via `parse_stored_blob` / `patch_stored_blob`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenGrant {
    pub access: String,
    pub refresh: Option<String>,
    /// Seconds from issuance until the access token expires. Providers whose
    /// blob only stores an absolute expiry compute this at parse time.
    pub expires_in_secs: i64,
}

/// One rate-limit / quota window as displayed in the menu. `id` is a stable
/// per-provider slug ("session", "weekly", "opus", ...); `label` is what the
/// menu prints. `utilization` is a percentage in `0.0..=100.0`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub id: String,
    pub label: String,
    pub utilization: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
}

/// Snapshot of every usage window a provider reports for a single account.
/// `Vec`, not a map — order is provider-defined and deterministic without
/// depending on serde's `preserve_order` feature.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub windows: Vec<UsageWindow>,
    pub fetched_at: DateTime<Utc>,
}

/// Identity fields we know about an account. All optional so a provider with
/// only a UUID (opencode, qwen-code) can still populate what it has; the raw
/// vendor blob rides along in `native_blob` for round-tripping.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IdentitySnapshot {
    pub email: Option<String>,
    pub uuid: Option<String>,
    pub display_name: Option<String>,
    pub native_blob: Value,
}

/// Where the provider stores its secret bytes. state.json holds only a
/// pointer (`secret_ref`) to one of these backends; the token itself never
/// lives inside state.json.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SecretBackend {
    Keychain,
    File,
    Sqlite,
}

/// Result of `capture_current_login`: identity + the exact secret bytes we
/// need to persist plus the decoded token grant for refresh math.
#[derive(Clone, Debug)]
pub struct CapturedAccount {
    pub identity: IdentitySnapshot,
    pub secret_blob: String,
    pub tokens: TokenGrant,
}

/// How to launch the vendor CLI after a swap. Providers that don't shell out
/// (reporting-only) may ignore this.
#[derive(Copy, Clone, Debug)]
pub enum LaunchMode {
    Fresh,
    Continue,
}

/// Amber / red thresholds for menu row coloring. Providers may override in
/// `Provider::severity_bands` if their quota semantics differ.
#[derive(Copy, Clone, Debug)]
pub struct SeverityBands {
    pub amber: f64,
    pub red: f64,
}

/// How a provider onboards a new account. The menu's "Add account" flow uses
/// this to pick between the on-disk credential capture UX (read whatever the
/// vendor CLI already persisted) and the paste-an-API-key UX (prompt the
/// user for a raw key + optional nickname).
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureMode {
    /// Read credentials that the vendor CLI wrote to disk / keychain during
    /// its own `login` flow (Claude, Codex, ...).
    CredsOnDisk,
    /// Accept a raw API key pasted by the user (OpenRouter, DeepSeek, ...).
    ApiKey,
}

/// Runtime feature flags a provider exposes so menu code can decide what UI
/// elements to render (Switch row, usage rows, capture entry, ...).
#[derive(Copy, Clone, Debug)]
pub struct Capabilities {
    pub supports_usage: bool,
    pub supports_switching: bool,
    pub supports_email_capture: bool,
    pub secret_backend: SecretBackend,
    pub capture_mode: CaptureMode,
}

/// One provider (Claude, Codex, opencode, ...). Registered once at startup
/// via `providers::init()`; every method is called through `&dyn Provider`.
pub trait Provider: Send + Sync + 'static {
    // --- Identity of the provider itself ---

    /// Stable machine-readable slug: "claude", "codex", "opencode", ...
    fn provider_id(&self) -> &'static str;

    /// Human-facing name for menu headers and status rows.
    fn display_name(&self) -> &'static str;

    /// Feature flags — see `Capabilities`.
    fn capabilities(&self) -> Capabilities;

    /// Amber / red utilization thresholds for menu coloring.
    fn severity_bands(&self) -> SeverityBands {
        SeverityBands {
            amber: 80.0,
            red: 95.0,
        }
    }

    /// Auto-swap trigger ladder shown in the menu (percentages).
    fn trigger_options(&self) -> &'static [f64] {
        &[90.0, 95.0, 98.0]
    }

    /// Order in which `UsageWindow::id`s should render in the menu.
    fn window_order(&self) -> &'static [&'static str] {
        &[]
    }

    // --- Capture / listing ---

    /// Read whatever the vendor CLI persisted on this host. `Ok(None)` means
    /// "nothing captured / not logged in"; `Err(Unsupported)` means the
    /// provider is a pure stub with no capture path wired yet.
    fn capture_current_login(&self) -> PResult<Option<CapturedAccount>>;

    /// Enumerate captured accounts on this host. Default implementation lifts
    /// `capture_current_login` into a `Vec`, which is right for single-slot
    /// stores (all of the v1 providers).
    fn list_accounts(&self) -> PResult<Vec<CapturedAccount>> {
        Ok(self.capture_current_login()?.into_iter().collect())
    }

    /// Register an account onboarded via a pasted API key. `nickname` is the
    /// user-supplied label (used for menu display and, for providers that
    /// can't derive an email from the key alone, for `account_identifier`);
    /// `key` is the raw API key. Default is `Unsupported` for providers whose
    /// `capture_mode` is `CredsOnDisk`.
    fn capture_api_key(&self, nickname: String, key: String) -> PResult<CapturedAccount> {
        let _ = (nickname, key);
        Err(ProviderError::Unsupported)
    }

    // --- Token lifecycle ---

    /// Decode the vendor's on-disk credential bytes to a `TokenGrant`.
    fn parse_stored_blob(&self, blob: &str) -> PResult<TokenGrant>;

    /// Merge a fresh `TokenGrant` back into the vendor blob (typically after
    /// a refresh); returns the bytes to persist.
    fn patch_stored_blob(&self, blob: &str, grant: &TokenGrant) -> PResult<String>;

    /// Refresh an access token using the stored refresh token. Default is
    /// `Unsupported` for providers that don't do refresh math themselves.
    fn refresh_token(&self, refresh: &str) -> PResult<TokenGrant> {
        let _ = refresh;
        Err(ProviderError::Unsupported)
    }

    // --- Usage ---

    /// Call the provider's usage endpoint with the given access token.
    /// Default is `Unsupported` for reporting-only providers without one.
    fn fetch_usage(&self, access_token: &str) -> PResult<UsageSnapshot> {
        let _ = access_token;
        Err(ProviderError::Unsupported)
    }

    // --- Switching ---

    /// Persist `blob` as the active login for this provider and update any
    /// identity-adjacent files the vendor CLI expects to read.
    fn write_active_account(
        &self,
        blob: &str,
        identity: &IdentitySnapshot,
    ) -> PResult<()> {
        let _ = (blob, identity);
        Err(ProviderError::Unsupported)
    }

    /// Read whatever the vendor CLI currently considers the active identity.
    fn read_active_identity(&self) -> PResult<Option<IdentitySnapshot>> {
        Err(ProviderError::Unsupported)
    }

    /// Launch the vendor CLI in the given mode (fresh session vs continue).
    fn launch_client(&self, mode: LaunchMode) -> PResult<ExitStatus> {
        let _ = mode;
        Err(ProviderError::Unsupported)
    }

    // --- Account identity helpers ---

    /// Stable per-provider key used inside state.json's `providers.<slug>
    /// .accounts.<key>` namespace. Default: lowercased email, then UUID,
    /// then a short hash of `native_blob`.
    fn account_identifier(&self, id: &IdentitySnapshot) -> String {
        id.email
            .as_ref()
            .map(|e| e.to_ascii_lowercase())
            .or_else(|| id.uuid.clone())
            .unwrap_or_else(|| format!("anon-{}", short_hash(&id.native_blob)))
    }

    /// Human-facing label for an account row in the menu.
    fn account_display(&self, id: &IdentitySnapshot) -> String {
        id.email
            .clone()
            .or_else(|| id.display_name.clone())
            .unwrap_or_else(|| format!("{} account", self.display_name()))
    }
}

/// Short deterministic id derived from a JSON blob. Used only as an
/// account-identifier fallback when neither email nor UUID is available.
fn short_hash(v: &Value) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    let mut out = String::with_capacity(16);
    for b in digest.iter().take(8) {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A minimal in-test provider used to check the default trait method
    /// implementations behave as documented.
    struct Dummy;

    impl Provider for Dummy {
        fn provider_id(&self) -> &'static str {
            "dummy"
        }
        fn display_name(&self) -> &'static str {
            "Dummy"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_usage: false,
                supports_switching: false,
                supports_email_capture: false,
                secret_backend: SecretBackend::File,
                capture_mode: CaptureMode::CredsOnDisk,
            }
        }
        fn capture_current_login(&self) -> PResult<Option<CapturedAccount>> {
            Ok(None)
        }
        fn parse_stored_blob(&self, _blob: &str) -> PResult<TokenGrant> {
            Err(ProviderError::Unsupported)
        }
        fn patch_stored_blob(
            &self,
            _blob: &str,
            _grant: &TokenGrant,
        ) -> PResult<String> {
            Err(ProviderError::Unsupported)
        }
    }

    #[test]
    fn defaults_return_unsupported() {
        let p = Dummy;
        assert!(matches!(
            p.refresh_token("r"),
            Err(ProviderError::Unsupported)
        ));
        assert!(matches!(
            p.fetch_usage("a"),
            Err(ProviderError::Unsupported)
        ));
        assert!(matches!(
            p.launch_client(LaunchMode::Fresh),
            Err(ProviderError::Unsupported)
        ));
        assert!(matches!(
            p.capture_api_key("n".into(), "k".into()),
            Err(ProviderError::Unsupported)
        ));
    }

    #[test]
    fn capture_mode_default_is_creds_on_disk() {
        let caps = Dummy.capabilities();
        assert_eq!(caps.capture_mode, CaptureMode::CredsOnDisk);
    }

    #[test]
    fn account_identifier_prefers_email() {
        let p = Dummy;
        let id = IdentitySnapshot {
            email: Some("Foo@Example.com".into()),
            uuid: Some("uuid-1".into()),
            display_name: None,
            native_blob: json!({}),
        };
        assert_eq!(p.account_identifier(&id), "foo@example.com");
    }

    #[test]
    fn account_identifier_falls_back_to_uuid_then_hash() {
        let p = Dummy;
        let id_uuid = IdentitySnapshot {
            email: None,
            uuid: Some("uuid-1".into()),
            display_name: None,
            native_blob: json!({"a":1}),
        };
        assert_eq!(p.account_identifier(&id_uuid), "uuid-1");

        let id_anon = IdentitySnapshot {
            email: None,
            uuid: None,
            display_name: None,
            native_blob: json!({"a":1}),
        };
        assert!(p.account_identifier(&id_anon).starts_with("anon-"));
    }
}
