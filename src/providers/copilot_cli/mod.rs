//! GitHub Copilot CLI provider — STUB ONLY.
//!
//! Recon summary:
//!  - macOS default: keychain generic-password service `copilot-cli`.
//!  - Fallback: `~/.copilot/config.json` (plaintext) when keychain unavailable.
//!  - Env-var override precedence: `COPILOT_GITHUB_TOKEN` > `GH_TOKEN` >
//!    `GITHUB_TOKEN`; last-ditch fallback reads gh CLI's token from
//!    `~/.config/gh/hosts.yml`.
//!  - No identity is persisted on disk — must call GitHub REST `/user` +
//!    `/user/emails` with the stored token to label the account.
//!  - No documented Copilot-CLI usage endpoint; premium-request consumption
//!    surfaces inline with chat responses (headers-only).

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CaptureMode, CapturedAccount, PResult, Provider, ProviderError, SecretBackend,
    TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(CopilotCliProvider)
}

pub struct CopilotCliProvider;

impl Provider for CopilotCliProvider {
    fn provider_id(&self) -> &'static str {
        "copilot-cli"
    }

    fn display_name(&self) -> &'static str {
        "GitHub Copilot CLI"
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
        // TODO: read the `copilot-cli` macOS Keychain entry (or the plaintext
        // fallback in `~/.copilot/config.json`, or the env-var override
        // ladder), then GET `https://api.github.com/user` + `/user/emails`
        // with the resulting token to label the account.
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
        let p = CopilotCliProvider;
        assert_eq!(p.provider_id(), "copilot-cli");
        assert_eq!(p.display_name(), "GitHub Copilot CLI");
        let caps = p.capabilities();
        assert!(!caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
    }

    #[test]
    fn capture_returns_none_placeholder() {
        assert!(matches!(
            CopilotCliProvider.capture_current_login(),
            Ok(None)
        ));
    }
}
