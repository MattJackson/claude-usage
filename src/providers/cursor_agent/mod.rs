//! Cursor CLI (`cursor-agent`, Anysphere) provider — STUB ONLY.
//!
//! Recon summary:
//!  - Default storage: macOS login keychain (exact service label is not
//!    publicly documented — Cursor's own docs point users to remove "cursor"
//!    entries in Keychain Access).
//!  - Opt-in file store: `AGENT_CLI_CREDENTIAL_STORE=file` writes
//!    `~/.cursor/auth.json` (owner-only).
//!  - Identity endpoint: GET `https://api.cursor.com/v1/me` (Bearer) returns
//!    `user_email`, `user_id`, `user_first_name`, `user_last_name`.
//!  - Non-interactive env: `CURSOR_API_KEY`.
//!  - No documented per-account usage endpoint; Cursor exposes a per-principal
//!    rate-limit-status endpoint that "does not consume rate limit points"
//!    (path not published in the pages consulted).

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CapturedAccount, PResult, Provider, ProviderError, SecretBackend, TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(CursorAgentProvider)
}

pub struct CursorAgentProvider;

impl Provider for CursorAgentProvider {
    fn provider_id(&self) -> &'static str {
        "cursor-agent"
    }

    fn display_name(&self) -> &'static str {
        "Cursor CLI"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_usage: false,
            supports_switching: false,
            supports_email_capture: false,
            secret_backend: SecretBackend::Keychain,
        }
    }

    fn capture_current_login(&self) -> PResult<Option<CapturedAccount>> {
        // TODO: with `AGENT_CLI_CREDENTIAL_STORE=file`, read
        // `~/.cursor/auth.json`; otherwise probe the macOS keychain (service
        // label undocumented) or accept a `CURSOR_API_KEY` env var. Then
        // call `https://api.cursor.com/v1/me` to attach the email.
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
        let p = CursorAgentProvider;
        assert_eq!(p.provider_id(), "cursor-agent");
        assert_eq!(p.display_name(), "Cursor CLI");
        let caps = p.capabilities();
        assert!(!caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
    }

    #[test]
    fn capture_returns_none_placeholder() {
        assert!(matches!(
            CursorAgentProvider.capture_current_login(),
            Ok(None)
        ));
    }
}
