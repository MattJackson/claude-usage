//! Fireworks AI provider — API-KEY STUB.
//!
//! Registered so the "Paste API key" submenu can list it, but the real
//! `capture_api_key` flow is deferred. Nothing here reads credentials or
//! hits the network yet.
//!
//! Onboarding plan (locked, see workflow context):
//!  - Capture mode: `ApiKey`.
//!  - Account identifier: **email** — Fireworks exposes
//!    `GET /v1/accounts` which returns the account owner's email; the
//!    real capture flow calls it once, stores the email in
//!    `IdentitySnapshot`, and keys the keychain entry off it. The
//!    user-supplied nickname is preserved as `display_name` for the row
//!    label but not used as the identifier.
//!  - Usage: `/v1/accounts/<id>/usage` wiring lands in a later phase.

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CaptureMode, CapturedAccount, PResult, Provider, ProviderError, SecretBackend,
    TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(FireworksProvider)
}

pub struct FireworksProvider;

impl Provider for FireworksProvider {
    fn provider_id(&self) -> &'static str {
        "fireworks"
    }

    fn display_name(&self) -> &'static str {
        "Fireworks"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_usage: true,
            supports_switching: false,
            supports_email_capture: true,
            secret_backend: SecretBackend::Keychain,
            capture_mode: CaptureMode::ApiKey,
        }
    }

    fn capture_current_login(&self) -> PResult<Option<CapturedAccount>> {
        Err(ProviderError::Unsupported)
    }

    fn capture_api_key(&self, _nickname: String, _key: String) -> PResult<CapturedAccount> {
        // TODO: call GET /v1/accounts with the pasted key, use the returned
        // account owner email as the identifier, persist the key to the
        // keychain under `fireworks/<email>`.
        Err(ProviderError::Unsupported)
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
        let p = FireworksProvider;
        assert_eq!(p.provider_id(), "fireworks");
        assert_eq!(p.display_name(), "Fireworks");
    }

    #[test]
    fn capabilities_are_locked() {
        let caps = FireworksProvider.capabilities();
        assert!(caps.supports_usage);
        assert!(!caps.supports_switching);
        // Fireworks is the sole API-key provider that keys accounts by email
        // (via GET /v1/accounts) — the flag must stay on so the menu wires
        // the email-badge decoration.
        assert!(caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
        assert_eq!(caps.capture_mode, CaptureMode::ApiKey);
    }

    #[test]
    fn capture_paths_return_unsupported() {
        let p = FireworksProvider;
        assert!(matches!(
            p.capture_current_login(),
            Err(ProviderError::Unsupported)
        ));
        assert!(matches!(
            p.capture_api_key("prod".into(), "fw-x".into()),
            Err(ProviderError::Unsupported)
        ));
    }
}
