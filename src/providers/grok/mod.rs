//! grok (xAI Grok CLI) provider — STUB ONLY.
//!
//! Registered so the "Capture current login" submenu can list it, but every
//! real operation is deferred. Nothing here reads credentials or hits the
//! network yet.
//!
//! Recon summary (upstream: github.com/xai-org/grok-build):
//!  - Creds path: `~/.grok/auth.json` plus `~/.grok/mcp_credentials.json`.
//!  - Format: plaintext JSON, 0600, single-slot per host. OIDC ID token
//!    carries the account email in its payload claims.
//!  - No macOS Keychain use.
//!  - No dedicated usage/quota endpoint (headers-only quota telemetry).
//!  - Switching is "clean" (single-slot swap of the two files) but not wired
//!    yet.

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CaptureMode, CapturedAccount, PResult, Provider, ProviderError, SecretBackend,
    TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(GrokProvider)
}

pub struct GrokProvider;

impl Provider for GrokProvider {
    fn provider_id(&self) -> &'static str {
        "grok"
    }

    fn display_name(&self) -> &'static str {
        "Grok"
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
        // TODO: read `~/.grok/auth.json` and `~/.grok/mcp_credentials.json`,
        // decode the OIDC ID token payload to pull the account email, and
        // emit a single CapturedAccount for the currently-active slot.
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
    fn identity_is_locked() {
        let p = GrokProvider;
        assert_eq!(p.provider_id(), "grok");
        assert_eq!(p.display_name(), "Grok");
    }

    #[test]
    fn capabilities_are_locked() {
        let caps = GrokProvider.capabilities();
        assert!(!caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::File);
    }

    #[test]
    fn capture_returns_none_placeholder() {
        assert!(matches!(GrokProvider.capture_current_login(), Ok(None)));
    }
}
