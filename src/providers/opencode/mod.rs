//! opencode (SST / opencode.ai) provider — STUB ONLY.
//!
//! Registered so the "Capture current login" submenu can list it, but every
//! real operation is deferred. Nothing here reads credentials or hits the
//! network yet.
//!
//! Recon summary (see `wnllabvkg.output` for full detail):
//!  - Creds path: `~/.local/share/opencode/auth.json` (`OPENCODE_AUTH_CONTENT`
//!    env overrides it with an inline JSON string).
//!  - Format: top-level JSON object keyed by providerID; each value is a
//!    discriminated union `{type:"oauth"|"api"|"wellknown", ...}` with
//!    optional `accountId` / `enterpriseUrl` — no email, no user_id.
//!  - Single-slot per providerID; no macOS Keychain use.
//!  - No dedicated usage/quota endpoint (only response headers).

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CapturedAccount, PResult, Provider, ProviderError, SecretBackend, TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(OpencodeProvider)
}

pub struct OpencodeProvider;

impl Provider for OpencodeProvider {
    fn provider_id(&self) -> &'static str {
        "opencode"
    }

    fn display_name(&self) -> &'static str {
        "opencode"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_usage: false,
            supports_switching: false,
            supports_email_capture: false,
            secret_backend: SecretBackend::File,
        }
    }

    fn capture_current_login(&self) -> PResult<Option<CapturedAccount>> {
        // TODO: read `~/.local/share/opencode/auth.json` (or the
        // `OPENCODE_AUTH_CONTENT` env override) and iterate its providerID
        // keys to produce one CapturedAccount per configured provider.
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
        let p = OpencodeProvider;
        assert_eq!(p.provider_id(), "opencode");
        assert_eq!(p.display_name(), "opencode");
        let caps = p.capabilities();
        assert!(!caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::File);
    }

    #[test]
    fn capture_returns_none_placeholder() {
        assert!(matches!(OpencodeProvider.capture_current_login(), Ok(None)));
    }
}
