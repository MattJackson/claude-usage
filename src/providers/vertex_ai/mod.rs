//! Google Vertex AI provider — API-KEY STUB.
//!
//! Registered so the "Paste API key" submenu can list it, but the real
//! `capture_api_key` flow is deferred. Nothing here reads credentials or
//! hits the network yet.
//!
//! Onboarding plan (locked, see workflow context):
//!  - Capture mode: `ApiKey`. Account identifier is
//!    `<nickname>-<last-4-of-key>` — Vertex AI keys are project-scoped
//!    but the pasted-key flow doesn't do a project-info round-trip, so
//!    the nickname + last-4 disambiguator carries.
//!  - Usage: Cloud Billing / quota wiring lands in a later phase.

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CaptureMode, CapturedAccount, PResult, Provider, ProviderError, SecretBackend,
    TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(VertexAiProvider)
}

pub struct VertexAiProvider;

impl Provider for VertexAiProvider {
    fn provider_id(&self) -> &'static str {
        "vertex-ai"
    }

    fn display_name(&self) -> &'static str {
        "Vertex AI"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_usage: true,
            supports_switching: false,
            supports_email_capture: false,
            secret_backend: SecretBackend::Keychain,
            capture_mode: CaptureMode::ApiKey,
        }
    }

    fn capture_current_login(&self) -> PResult<Option<CapturedAccount>> {
        Err(ProviderError::Unsupported)
    }

    fn capture_api_key(&self, _nickname: String, _key: String) -> PResult<CapturedAccount> {
        // TODO: derive account_id = `<nickname>-<last-4-of-key>`, persist
        // the key to the keychain under `vertex-ai/<account_id>`.
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
        let p = VertexAiProvider;
        assert_eq!(p.provider_id(), "vertex-ai");
        assert_eq!(p.display_name(), "Vertex AI");
    }

    #[test]
    fn capabilities_are_locked() {
        let caps = VertexAiProvider.capabilities();
        assert!(caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
        assert_eq!(caps.capture_mode, CaptureMode::ApiKey);
    }

    #[test]
    fn capture_paths_return_unsupported() {
        let p = VertexAiProvider;
        assert!(matches!(
            p.capture_current_login(),
            Err(ProviderError::Unsupported)
        ));
        assert!(matches!(
            p.capture_api_key("prod".into(), "vx-x".into()),
            Err(ProviderError::Unsupported)
        ));
    }
}
