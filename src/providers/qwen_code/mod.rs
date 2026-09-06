//! Qwen Code (Alibaba) provider — STUB ONLY.
//!
//! Recon summary:
//!  - Creds path: `~/.qwen/oauth_creds.json` (mode 0600), OAuth 2.0 device
//!    flow against `chat.qwen.ai`.
//!  - Format: `{ access_token, refresh_token, token_type, resource_url,
//!    expiry_date }`. No email / sub / user_id / plan_tier are ever
//!    persisted — the `id_token` field is declared but never written.
//!  - No `/me`, `/userinfo`, `/quota`, or `/usage` endpoint is called by the
//!    CLI; rate-limit info arrives only as chat-completion response headers.
//!  - Single-slot; second `/auth` login overwrites via atomic temp+rename.

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CapturedAccount, PResult, Provider, ProviderError, SecretBackend, TokenGrant,
};

pub fn new() -> Box<dyn Provider> {
    Box::new(QwenCodeProvider)
}

pub struct QwenCodeProvider;

impl Provider for QwenCodeProvider {
    fn provider_id(&self) -> &'static str {
        "qwen-code"
    }

    fn display_name(&self) -> &'static str {
        "Qwen Code"
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
        // TODO: read `~/.qwen/oauth_creds.json` and populate `TokenGrant` from
        // its `access_token` / `refresh_token` / `expiry_date` fields. Email
        // is not available at capture — the CLI never asks for it.
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
        let p = QwenCodeProvider;
        assert_eq!(p.provider_id(), "qwen-code");
        assert_eq!(p.display_name(), "Qwen Code");
        let caps = p.capabilities();
        assert!(!caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::File);
    }

    #[test]
    fn capture_returns_none_placeholder() {
        assert!(matches!(
            QwenCodeProvider.capture_current_login(),
            Ok(None)
        ));
    }
}
