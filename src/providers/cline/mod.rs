//! Cline CLI provider — STUB ONLY.
//!
//! Recon summary:
//!  - Cline is BYO provider API keys (Anthropic / OpenAI / OpenRouter /
//!    Bedrock / ...); creds live in a Cline-owned JSON file on disk.
//!  - No first-party subscription and no first-party quota endpoint — a real
//!    integration would parse the per-provider keys and hit each vendor's
//!    own usage endpoint (Cline hub excepted, which has its own usage API).
//!  - Included in v1 as a stub so the "Capture current login" submenu lists
//!    it and users understand the scope; no real capture logic wired yet.

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CaptureMode, CapturedAccount, PResult, Provider, ProviderError, SecretBackend,
    TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(ClineProvider)
}

pub struct ClineProvider;

impl Provider for ClineProvider {
    fn provider_id(&self) -> &'static str {
        "cline"
    }

    fn display_name(&self) -> &'static str {
        "Cline"
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
        // TODO: read Cline's on-disk config JSON (BYO provider API keys, one
        // entry per configured vendor). Real usage requires calling each
        // vendor's own quota endpoint — the Cline binary itself has no
        // aggregate usage API (Cline hub subscription excepted).
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
        let p = ClineProvider;
        assert_eq!(p.provider_id(), "cline");
        assert_eq!(p.display_name(), "Cline");
        let caps = p.capabilities();
        assert!(!caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::File);
    }

    #[test]
    fn capture_returns_none_placeholder() {
        assert!(matches!(ClineProvider.capture_current_login(), Ok(None)));
    }
}
