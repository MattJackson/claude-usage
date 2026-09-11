// The provider trait surface below is the crate's provider-facing API: several
// items are only consumed by feature-gated per-provider impls (or by callers
// yet to land), so dead-code warnings on the trait/enum surface itself are
// noise rather than signal. R2-CRAFT-01: narrowed from the previous crate-wide
// #![allow(dead_code)] on providers/mod.rs to just this file (and state.rs).
#![allow(dead_code)]

//! Provider trait scaffolding.
//!
//! This is the core-agents-refactor v1 trait definition: one `Box<dyn Provider>`
//! per module, registered by `providers::init()` in `mod.rs`. The trait is
//! live and dispatched in production: `menubar` (account list, capture menu,
//! switch/refresh) and the `main` command handlers resolve providers via
//! `providers::get(slug)` / `providers::all()` and call `capabilities()`,
//! capture, and active-refresh through this surface.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

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

/// Coarse window classification used by analytics modules (burn-rate,
/// cost-tracking) that don't need the full `UsageWindow` metadata but do need
/// to distinguish "the ~5h session bucket" from "the ~7d weekly bucket."
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Window {
    Session,
    Weekly,
}

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
///
/// H3 (v0.5.0 codeaudit): `supports_switching`, `supports_launch`, and
/// `supports_remove` are read directly by the menu builder to decide whether
/// to construct a "Switch to this account" / "Launch client" / "Remove…" row
/// at all — they must stay truthful. Before this fix, `supports_switching`
/// alone gated BOTH the Switch row and the Launch row (a provider whose
/// `write_active_account` was wired but whose `launch_client` was still the
/// `Unsupported` trait default would still get a Launch row that always
/// errored), and the Remove row wasn't gated on anything.
#[derive(Copy, Clone, Debug)]
pub struct Capabilities {
    pub supports_usage: bool,
    pub supports_switching: bool,
    /// True only if `launch_client` is actually wired to spawn the vendor
    /// CLI. Independent of `supports_switching` — a provider can support one
    /// without the other (e.g. `write_active_account` implemented, no client
    /// launcher yet).
    pub supports_launch: bool,
    /// True if removing a captured account (dropping it from state.json) is
    /// safe for this provider. Defaults to `true` for every provider that
    /// sets it explicitly — removing an account is a generic state.json
    /// operation every provider can, in principle, support — but stays an
    /// explicit field so a future provider needing extra cleanup (e.g.
    /// deleting an on-disk auth file) can flip it off until that's wired.
    pub supports_remove: bool,
    pub supports_email_capture: bool,
    pub secret_backend: SecretBackend,
    pub capture_mode: CaptureMode,
}

/// Stable per-provider identity for an account. `provider` is the owning
/// provider's slug (matches `Provider::provider_id`); `key` is the lowercased
/// account identifier (email or UUID) used to locate the account in state.
///
/// Used by the credential-sync subsystem so a provider-agnostic watcher can
/// route a blob it just read from disk back to the right account slot
/// without needing to know per-provider schemas.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountKey {
    pub provider: String,
    pub key: String,
}

impl AccountKey {
    pub fn new(provider: impl Into<String>, key: impl Into<String>) -> Self {
        AccountKey {
            provider: provider.into(),
            key: key.into().to_ascii_lowercase(),
        }
    }
}

/// How fresh a stored credential blob is relative to `now`. Returned by
/// `Provider::credential_freshness` so a provider-agnostic sync layer can
/// compare rotations across paths without knowing per-provider timestamps.
#[derive(Clone, Debug, PartialEq)]
pub enum CredentialFreshness {
    /// Access token has significant remaining life (> refresh skew).
    Fresh,
    /// Access token expires within `Duration` — refresh soon.
    ExpiresIn(Duration),
    /// Access token has already expired (still refreshable if the refresh
    /// token is intact, but no longer usable as a bearer).
    Expired,
    /// The blob could not be parsed as a credential (corrupt / wrong shape).
    Invalid,
    /// Provider did not implement freshness detection.
    Unknown,
}

impl CredentialFreshness {
    /// Order for "pick the freshest of several blobs": Fresh > ExpiresIn(long
    /// remaining) > ExpiresIn(short) > Expired > Invalid > Unknown. Used by
    /// the last-chance fallback when re-reading credential paths.
    pub fn rank(&self) -> i64 {
        match self {
            CredentialFreshness::Fresh => i64::MAX,
            CredentialFreshness::ExpiresIn(d) => d.as_secs() as i64,
            CredentialFreshness::Expired => -1,
            CredentialFreshness::Invalid => -2,
            CredentialFreshness::Unknown => -3,
        }
    }

    /// True if we consider this blob still usable as-is (bearer + refresh).
    /// `Expired` is deliberately not usable — we want to hand out fresh
    /// access tokens only.
    pub fn is_usable(&self) -> bool {
        matches!(
            self,
            CredentialFreshness::Fresh | CredentialFreshness::ExpiresIn(_)
        )
    }
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
            red: 90.0,
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

    /// Whether this provider's refresh grant is fully programmatic (a plain
    /// HTTPS POST `usagio` can issue itself, with no vendor CLI / browser
    /// step required) and therefore safe to drive proactively — including,
    /// for providers whose active-login store is a single file (no separate
    /// keychain/identity-file layer), refreshing the ACTIVE account under a
    /// compare-and-swap on that file (see `codex::oauth::active_refresh_cas`
    /// for the reference implementation and its doc comment for why Codex's
    /// CAS is simpler than a keychain-backed provider's).
    ///
    /// Deliberately a trait method with a `false` default rather than a
    /// `Capabilities` field: `Capabilities` is a plain struct literal
    /// constructed by every provider, so adding a required field there would
    /// force an unrelated edit onto every other provider module. A method
    /// with a default impl lets Codex opt in without touching anyone else.
    fn supports_active_refresh(&self) -> bool {
        false
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
    fn write_active_account(&self, blob: &str, identity: &IdentitySnapshot) -> PResult<()> {
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

    /// Persist `blob` to the vendor CLI's OS-native credential storage for
    /// wherever `read_active_identity` would report as the current login.
    ///
    /// Called ONLY after usagio refreshes an INACTIVE account's token
    /// server-side (see `credentials::refresh_inactive_if_stale`) — never for
    /// the active account, which the vendor CLI itself owns rotation for.
    /// Without this mirror step, state.json would race ahead of whatever
    /// on-disk / keychain blob the vendor CLI reads on its next invocation,
    /// silently reintroducing the "never re-login" regression this exists to
    /// close.
    ///
    /// The default impl delegates to `write_active_account` using the
    /// current identity — providers with no switching support (whose
    /// `read_active_identity` is still the `Unsupported` trait default)
    /// inherit `Unsupported` transitively, which is the correct behavior:
    /// there's nowhere vendor-native to mirror to yet.
    fn mirror_rotated_token(&self, blob: &str) -> PResult<()> {
        match self.read_active_identity()? {
            Some(id) => self.write_active_account(blob, &id),
            None => Err(ProviderError::NotLoggedIn),
        }
    }

    /// Read the vendor CLI's current OS-native credential blob for whichever
    /// account it currently considers active — the same slot
    /// `mirror_rotated_token` writes to (macOS keychain, or the on-disk
    /// credentials file on Linux/Windows).
    ///
    /// This is the read half of the compare-and-swap the ACTIVE account's
    /// refresh cycle uses (`main.rs::active_refresh_cas`): usagio reads this
    /// slot before AND after its own `/token` POST to detect whether the
    /// vendor CLI rotated the same slot concurrently, without introducing any
    /// new OS-specific code outside `platform/*` — providers implement this
    /// purely in terms of `Platform::secrets()` / `credential_paths()`, the
    /// same primitives `mirror_rotated_token` already uses.
    ///
    /// `Ok(None)` means the slot is empty (nothing captured yet). Default is
    /// `Unsupported` — only providers with `mirror_rotated_token` wired
    /// (Claude, Codex) implement this.
    fn read_active_slot(&self) -> PResult<Option<String>> {
        Err(ProviderError::Unsupported)
    }

    // --- Credential sync (fsnotify + proactive refresh + last-chance fallback) ---

    /// On-disk paths where the vendor CLI may write this provider's
    /// credential blob. The credential-sync watcher subscribes to these with
    /// notify(); the last-chance fallback re-reads them on invalid_grant to
    /// adopt whichever rotation the vendor did behind our back. Default is
    /// empty — providers that don't persist to disk don't participate.
    fn credential_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Which account does this raw credential blob belong to? Returns
    /// `Some(AccountKey)` if the blob is recognisable and carries an
    /// identifier this provider tracks. Default is `None` (unknown).
    fn identify_credential(&self, blob: &str) -> Option<AccountKey> {
        let _ = blob;
        None
    }

    /// Report how fresh the blob's access token is right now. Used to pick
    /// the freshest of several observed rotations, and to decide when to
    /// pre-emptively refresh an inactive account.
    fn credential_freshness(&self, blob: &str) -> CredentialFreshness {
        let _ = blob;
        CredentialFreshness::Unknown
    }

    /// Absorb a rotated blob into `account`'s stored credentials. Providers
    /// that own a slot in state.json overwrite it in place; providers with
    /// no first-class account storage yet may no-op. Default is a no-op so
    /// non-tracked providers don't need to opt in.
    fn absorb_credential(&self, account: &AccountKey, blob: &str) -> PResult<()> {
        let _ = (account, blob);
        Ok(())
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
                supports_launch: false,
                supports_remove: true,
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
        fn patch_stored_blob(&self, _blob: &str, _grant: &TokenGrant) -> PResult<String> {
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
    fn credential_defaults_report_nothing() {
        // A provider that opts out of credential sync must return empty
        // paths, unknown identity, and unknown freshness — the sync layer
        // treats these as "nothing to do".
        let p = Dummy;
        assert!(p.credential_paths().is_empty());
        assert!(p.identify_credential("{}").is_none());
        assert_eq!(p.credential_freshness("{}"), CredentialFreshness::Unknown);
        let k = AccountKey::new("dummy", "x");
        assert!(p.absorb_credential(&k, "{}").is_ok());
    }

    #[test]
    fn credential_freshness_rank_ordering_is_sane() {
        // Fresh dominates any ExpiresIn; ExpiresIn(longer) beats ExpiresIn(shorter);
        // Expired beats Invalid beats Unknown. Used by last_chance_fallback.
        let fresh = CredentialFreshness::Fresh.rank();
        let long = CredentialFreshness::ExpiresIn(std::time::Duration::from_secs(600)).rank();
        let short = CredentialFreshness::ExpiresIn(std::time::Duration::from_secs(10)).rank();
        let expired = CredentialFreshness::Expired.rank();
        let invalid = CredentialFreshness::Invalid.rank();
        let unknown = CredentialFreshness::Unknown.rank();
        assert!(fresh > long);
        assert!(long > short);
        assert!(short > expired);
        assert!(expired > invalid);
        assert!(invalid > unknown);
    }

    #[test]
    fn account_key_lowercases_identifier() {
        // AccountKey identifiers are lowercased so a case shift on disk
        // (Foo@Example.com vs foo@example.com) doesn't create a phantom
        // second account slot.
        let k = AccountKey::new("claude", "Foo@Example.COM");
        assert_eq!(k.provider, "claude");
        assert_eq!(k.key, "foo@example.com");
    }

    #[test]
    fn credential_freshness_is_usable_excludes_expired() {
        use std::time::Duration;
        assert!(CredentialFreshness::Fresh.is_usable());
        assert!(CredentialFreshness::ExpiresIn(Duration::from_secs(60)).is_usable());
        assert!(!CredentialFreshness::Expired.is_usable());
        assert!(!CredentialFreshness::Invalid.is_usable());
        assert!(!CredentialFreshness::Unknown.is_usable());
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
