//! DeepSeek provider — API-KEY STUB.
//!
//! Registered so the "Paste API key" submenu can list it, but the real
//! `capture_api_key` flow is deferred. Nothing here reads credentials or
//! hits the network yet.
//!
//! Onboarding plan (locked, see workflow context):
//!  - Capture mode: `ApiKey`. Account identifier is
//!    `<nickname>-<last-4-of-key>` — DeepSeek has no public account-info
//!    endpoint that returns an email or label, so the user's nickname
//!    plus a stable key-suffix disambiguator is the best we can do.
//!  - Usage: DeepSeek's balance / usage endpoint wiring lands in a later
//!    phase.

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CaptureMode, CapturedAccount, PResult, Provider, ProviderError, SecretBackend,
    TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(DeepSeekProvider)
}

pub struct DeepSeekProvider;

impl Provider for DeepSeekProvider {
    fn provider_id(&self) -> &'static str {
        "deepseek"
    }

    fn display_name(&self) -> &'static str {
        "DeepSeek"
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
        // the key to the keychain under `deepseek/<account_id>`.
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
        let p = DeepSeekProvider;
        assert_eq!(p.provider_id(), "deepseek");
        assert_eq!(p.display_name(), "DeepSeek");
    }

    #[test]
    fn capabilities_are_locked() {
        let caps = DeepSeekProvider.capabilities();
        assert!(caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
        assert_eq!(caps.capture_mode, CaptureMode::ApiKey);
    }

    #[test]
    fn capture_paths_return_unsupported() {
        let p = DeepSeekProvider;
        assert!(matches!(
            p.capture_current_login(),
            Err(ProviderError::Unsupported)
        ));
        assert!(matches!(
            p.capture_api_key("prod".into(), "sk-ds-x".into()),
            Err(ProviderError::Unsupported)
        ));
    }
}
