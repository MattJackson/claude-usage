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
//!
//! ## v0.5.0 never-re-login scope decision: DEFERRED to v0.6
//!
//! opencode's `auth.json` is MULTI-PROVIDER: one top-level JSON object keyed
//! by `providerID` (`anthropic`, `openai`, `google`, ...), each holding an
//! independent `{type: "oauth"|"api"|"wellknown", ...}` blob. Any active-login
//! write-back (switching, or mirroring a rotated token) would have to PATCH
//! exactly one key inside that shared file, never overwrite the whole thing —
//! a naive whole-file write (the approach that works fine for Codex's
//! single-purpose `auth.json`) would silently drop every other configured
//! provider's login the moment usagio touched the file.
//!
//! Two ways to model that were considered:
//!
//!   (a) Treat opencode as a multiplexed provider — one usagio account per
//!       opencode `providerID` — and implement a partial-file-patch
//!       mirror-back (read the whole object, replace one key, write the
//!       whole object back atomically).
//!   (b) Defer opencode active-account handling to v0.6 entirely; keep this
//!       provider read-only (capture only, no switching / no refresh
//!       mirror-back) until the multi-provider modeling questions are
//!       resolved.
//!
//! **(b) is what's implemented here**, for three reasons:
//!   1. Scope: v0.5.0's contract is "never re-login" for accounts usagio
//!      actively manages. opencode's `capture_current_login` is still a
//!      stub (`Ok(None)` below) — there's no captured opencode account for a
//!      refresh/mirror-back to apply to yet, so building CAS/mirror-back
//!      plumbing now would have no caller.
//!   2. Identity gap: per-`providerID` blobs carry no email/user_id (see
//!      recon summary above) — `AccountKey`'s per-provider identity model
//!      (email, else UUID, else anon-hash) doesn't have a natural per-account
//!      key to hang a opencode-multiplexed account on without also deciding
//!      how `providerID` composes with the *underlying* provider's own
//!      identity (e.g. an opencode `anthropic` entry vs. a native Claude
//!      account — are they the same "account" for switching purposes?). That
//!      question needs its own design pass, not a v0.5.0-timeline decision.
//!   3. Blast radius: a partial-file-patch bug here corrupts OTHER providers'
//!      logins inside the same file, not just opencode's — a strictly higher
//!      risk than any other provider's mirror-back, which only ever touches
//!      its own single-provider credential file. That risk deserves its own
//!      review cycle.
//!
//! `Capabilities` below are therefore all `false` except `supports_remove`
//! (dropping a captured account from state.json is safe regardless), and
//! `Provider::supports_active_refresh` is left at its `false` trait default.
//! Revisit this file when v0.6 designs the multi-provider account model.

#![allow(dead_code)]

use crate::providers::trait_def::{
    Capabilities, CaptureMode, CapturedAccount, PResult, Provider, ProviderError, SecretBackend,
    TokenGrant,
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
            supports_launch: false,
            supports_remove: true,
            supports_email_capture: false,
            secret_backend: SecretBackend::File,
            capture_mode: CaptureMode::CredsOnDisk,
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
        assert_eq!(caps.capture_mode, CaptureMode::CredsOnDisk);
    }

    #[test]
    fn capture_returns_none_placeholder() {
        assert!(matches!(OpencodeProvider.capture_current_login(), Ok(None)));
    }
}
