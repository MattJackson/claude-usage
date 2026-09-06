//! Amazon Q Developer CLI provider — STUB ONLY.
//!
//! Recon summary:
//!  - Storage: SQLite DB at
//!    `~/Library/Application Support/amazon-q/data.sqlite3`, table `auth_kv`.
//!  - Well-known keys: `codewhisperer:odic:device-registration` (JSON
//!    `DeviceRegistration`), `codewhisperer:odic:token` (JSON `BuilderIdToken`
//!    with `access_token`, `refresh_token`, `expires_at`, `start_url`, ...).
//!  - No macOS Keychain use despite legacy comments; no email persisted on
//!    disk (must fetch via SSO / IdC APIs with the stored token).
//!  - Usage endpoint: `GetUsageLimits` on the Q Developer / CodeWhisperer
//!    service.
//!  - Note: new signups end 2026-05-15, full end-of-support 2027-04-30
//!    (Kiro migration) — flag as sunsetting.

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CapturedAccount, PResult, Provider, ProviderError, SecretBackend, TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(AmazonQProvider)
}

pub struct AmazonQProvider;

impl Provider for AmazonQProvider {
    fn provider_id(&self) -> &'static str {
        "amazon-q"
    }

    fn display_name(&self) -> &'static str {
        "Amazon Q"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_usage: false,
            supports_switching: false,
            supports_email_capture: false,
            secret_backend: SecretBackend::Sqlite,
        }
    }

    fn capture_current_login(&self) -> PResult<Option<CapturedAccount>> {
        // TODO: open the SQLite DB at
        // `~/Library/Application Support/amazon-q/data.sqlite3`, read the
        // `auth_kv` row with key `codewhisperer:odic:token`, and parse the
        // JSON `BuilderIdToken` for `access_token` / `refresh_token`. Email
        // is not on disk — an SSO/IdC API call would be needed to attach it.
        Ok(None)
    }

    fn parse_stored_blob(&self, _blob: &str) -> PResult<TokenGrant> {
        Err(ProviderError::Unsupported)
    }

    fn patch_stored_blob(&self, _blob: &str, _grant: &TokenGrant) -> PResult<String> {
        Err(ProviderError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_capabilities_are_locked() {
        let p = AmazonQProvider;
        assert_eq!(p.provider_id(), "amazon-q");
        assert_eq!(p.display_name(), "Amazon Q");
        let caps = p.capabilities();
        assert!(!caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Sqlite);
    }

    #[test]
    fn capture_returns_none_placeholder() {
        assert!(matches!(AmazonQProvider.capture_current_login(), Ok(None)));
    }
}
