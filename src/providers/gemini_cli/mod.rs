//! Gemini CLI (Google) provider — STUB ONLY.
//!
//! Recon summary:
//!  - OAuth tokens live in the macOS Keychain under service
//!    `gemini-cli-oauth`, account `main-account` (via `@github/keytar`), with a
//!    file-based fallback (`~/.gemini/oauth_creds.json`, mode 0600).
//!  - Email cache: `~/.gemini/google_accounts.json`
//!    (`{active: <email>, old: [<email>, ...]}`); email itself is fetched from
//!    `https://www.googleapis.com/oauth2/v2/userinfo`.
//!  - Usage endpoint: POST `https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist`.
//!  - Single-slot; second Google login overwrites the keychain entry.

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CaptureMode, CapturedAccount, PResult, Provider, ProviderError, SecretBackend,
    TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(GeminiCliProvider)
}

pub struct GeminiCliProvider;

impl Provider for GeminiCliProvider {
    fn provider_id(&self) -> &'static str {
        "gemini-cli"
    }

    fn display_name(&self) -> &'static str {
        "Gemini CLI"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_usage: false,
            supports_switching: false,
            supports_email_capture: false,
            secret_backend: SecretBackend::Keychain,
            capture_mode: CaptureMode::CredsOnDisk,
        }
    }

    fn capture_current_login(&self) -> PResult<Option<CapturedAccount>> {
        // TODO: read the macOS Keychain entry `gemini-cli-oauth`/`main-account`
        // (falling back to `~/.gemini/oauth_creds.json`) and pair the
        // credentials with the email cached in `~/.gemini/google_accounts.json`.
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
        let p = GeminiCliProvider;
        assert_eq!(p.provider_id(), "gemini-cli");
        assert_eq!(p.display_name(), "Gemini CLI");
        let caps = p.capabilities();
        assert!(!caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
    }

    #[test]
    fn capture_returns_none_placeholder() {
        assert!(matches!(
            GeminiCliProvider.capture_current_login(),
            Ok(None)
        ));
    }
}
