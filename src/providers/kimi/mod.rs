//! kimi (Moonshot AI Kimi CLI) provider — STUB ONLY.
//!
//! Registered so the "Capture current login" submenu can list it, but every
//! real operation is deferred. Nothing here reads credentials or hits the
//! network yet.
//!
//! Recon summary (upstream: github.com/MoonshotAI/kimi-cli):
//!  - Creds path: `~/.kimi/` (TOML config) plus a macOS Keychain entry under
//!    service name `kimi-code` for the OAuth refresh material.
//!  - OAuth client_id: `17e5f671-d194-4dfb-9706-5516cb48c098`, IdP host
//!    `api.kimi.com`.
//!  - Single-slot per host; switching is "awkward" (Keychain rewrite plus
//!    TOML rewrite) and not wired yet.
//!  - No dedicated usage/quota endpoint (headers-only quota telemetry).

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CaptureMode, CapturedAccount, PResult, Provider, ProviderError, SecretBackend,
    TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(KimiProvider)
}

pub struct KimiProvider;

impl Provider for KimiProvider {
    fn provider_id(&self) -> &'static str {
        "kimi"
    }

    fn display_name(&self) -> &'static str {
        "Kimi"
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
        // TODO: read `~/.kimi/` TOML plus the macOS Keychain entry under
        // service `kimi-code`, decode the OAuth refresh material, and emit a
        // single CapturedAccount for the currently-active slot.
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
        let p = KimiProvider;
        assert_eq!(p.provider_id(), "kimi");
        assert_eq!(p.display_name(), "Kimi");
    }

    #[test]
    fn capabilities_are_locked() {
        let caps = KimiProvider.capabilities();
        assert!(!caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::File);
    }

    #[test]
    fn capture_returns_none_placeholder() {
        assert!(matches!(KimiProvider.capture_current_login(), Ok(None)));
    }
}
