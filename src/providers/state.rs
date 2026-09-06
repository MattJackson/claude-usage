//! State schema v2 for the multi-provider refactor.
//!
//! v1 (the shape `crate::store::State` still persists today) is a flat list of
//! Claude accounts with inline tokens. v2 namespaces every account under its
//! provider slug, replaces `active: <email>` with a `{provider, account}` pair,
//! carries usage windows as a deterministic `Vec<UsageWindow>` instead of the
//! session/weekly/opus triple, and moves token bytes out of state.json into a
//! keychain (or vendor file / sqlite) pointed at by a `secret_ref`. Unknown
//! providers in the `providers` map round-trip verbatim so forward-compat
//! downgrades don't lose data.
//!
//! Nothing consumes this yet — the v1 `State` still drives every command.
//! Later phases route `menubar`, `switch`, and the auto-swap daemon through
//! v2, at which point `load_and_migrate` becomes the sole reader on startup
//! and the v1 code goes away. Until then, the module is scaffolding + a
//! migration function + tests, and the runtime binary keeps behaving as today.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::providers::trait_def::{IdentitySnapshot, SecretBackend, UsageWindow};
use crate::store::State as StateV1;

/// The version this build reads/writes. A state.json with `schema < 2` is
/// migrated in memory on load; a state.json with `schema == 2` is parsed
/// directly; a higher-numbered file is refused (we can't safely round-trip
/// fields we don't know about beyond the `providers` bucket).
pub const CURRENT_SCHEMA: u32 = 2;

/// Which provider + account is currently active. `provider` is a `Provider`
/// slug; `account` is that provider's per-account key (see `AccountV2::
/// identifier`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveRef {
    pub provider: String,
    pub account: String,
}

/// One rate-limit / quota window carried in `AccountV2::cached_usage`. Same
/// shape as `trait_def::UsageWindow` but serialized as a plain owned struct
/// so state.json's `cached_usage.windows` is a simple JSON array.
pub type WindowV2 = UsageWindow;

/// Cached usage snapshot for an account, mirroring `trait_def::UsageSnapshot`
/// but with an epoch-millis timestamp so state.json stays JSON-number typed
/// (matches every other timestamp field in the file).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedUsageV2 {
    /// Unix epoch millis when this snapshot was fetched.
    pub fetched_at_ms: i64,
    #[serde(default)]
    pub windows: Vec<WindowV2>,
}

/// Where the account's secret bytes live. state.json holds only this pointer,
/// never the token itself, so a downgrade / accidental copy of state.json
/// doesn't leak credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRef {
    pub backend: SecretBackend,
    /// Backend-specific service / bucket / table name.
    pub service: String,
    /// Backend-specific per-secret key.
    pub account: String,
}

/// Historical bookkeeping the auto-swap daemon uses to avoid ping-pong.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryV2 {
    /// Unix epoch millis of the last time this account was made active.
    #[serde(default)]
    pub last_swap_ms: i64,
    /// Unix epoch millis of the last time we swapped AWAY from this account.
    #[serde(default)]
    pub left_at_ms: i64,
}

/// A single account under `providers.<slug>.accounts.<identifier>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountV2 {
    /// Stable key inside the parent map. Duplicates the map key so downstream
    /// consumers holding just an `&AccountV2` don't need the surrounding key.
    pub identifier: String,
    /// Human-facing label (usually the email, sometimes a nickname).
    pub display: String,
    /// Everything the provider knows about this account's identity.
    pub identity: IdentitySnapshot,
    /// Where the secret bytes live. Never the bytes themselves.
    pub secret_ref: SecretRef,
    /// Access-token expiry, epoch millis. Kept out of the secret so the
    /// scheduler can decide when to refresh without unlocking the keychain.
    #[serde(default)]
    pub expires_at_ms: i64,
    /// Last usage snapshot; populated only by the scheduler poll.
    #[serde(default)]
    pub cached_usage: Option<CachedUsageV2>,
    /// Per-account auto-swap history.
    #[serde(default)]
    pub history: HistoryV2,
}

/// Per-provider policy overrides. `active` mirrors the vendor CLI's single
/// slot so the menu can bold the right row even when the top-level
/// `state.active` points at a different provider.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PerProviderPolicy {
    #[serde(default = "default_true")]
    pub autoswap: bool,
    #[serde(default)]
    pub active: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Global + per-provider policy knobs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PolicyV2 {
    #[serde(default)]
    pub autoswap_disabled: bool,
    #[serde(default)]
    pub trigger_pct: Option<f64>,
    #[serde(default)]
    pub per_provider: BTreeMap<String, PerProviderPolicy>,
}

/// One provider's full bucket, deserialized on demand (see `known_bucket`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderBucket {
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountV2>,
}

/// The state.json v2 root.
///
/// `providers` is `BTreeMap<String, Value>` (not `BTreeMap<String,
/// ProviderBucket>`) so unknown provider slugs on load round-trip verbatim on
/// save — a downgrade from a later build that added `providers.foo` doesn't
/// silently drop `foo`'s bucket. Known providers are parsed on demand via
/// `known_bucket` / `set_known_bucket`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateV2 {
    pub schema: u32,
    #[serde(default)]
    pub active: Option<ActiveRef>,
    #[serde(default)]
    pub policy: PolicyV2,
    #[serde(default)]
    pub providers: BTreeMap<String, Value>,
}

impl Default for StateV2 {
    fn default() -> Self {
        StateV2 {
            schema: CURRENT_SCHEMA,
            active: None,
            policy: PolicyV2::default(),
            providers: BTreeMap::new(),
        }
    }
}

impl StateV2 {
    /// Typed view of one provider's bucket. Returns `Ok(None)` if the slug
    /// isn't present, `Err` if the bucket exists but doesn't deserialize as
    /// `ProviderBucket` (a future / hand-edited entry — the raw `Value` is
    /// still round-tripped verbatim through `providers`).
    pub fn known_bucket(&self, slug: &str) -> Result<Option<ProviderBucket>> {
        match self.providers.get(slug) {
            None => Ok(None),
            Some(v) => serde_json::from_value(v.clone())
                .map(Some)
                .with_context(|| format!("parsing providers.{slug}")),
        }
    }

    /// Write back a typed bucket for `slug`, replacing whatever was there.
    pub fn set_known_bucket(&mut self, slug: &str, bucket: ProviderBucket) -> Result<()> {
        let v = serde_json::to_value(bucket)
            .with_context(|| format!("serializing providers.{slug}"))?;
        self.providers.insert(slug.to_string(), v);
        Ok(())
    }

    /// Deserialize a v2 state.json byte slice.
    pub fn parse_v2_bytes(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).context("parsing state.json (v2)")
    }

    /// Deserialize either shape from bytes: detect the schema, migrate v1 in
    /// memory if needed. Secrets are handed to `sink`; a real caller passes a
    /// keychain writer, tests pass a `Vec`-backed sink.
    pub fn load_and_migrate_bytes(
        bytes: &[u8],
        sink: &mut dyn SecretSink,
    ) -> Result<Self> {
        let v: Value = serde_json::from_slice(bytes).context("state.json is not valid JSON")?;
        Self::load_and_migrate_value(&v, sink)
    }

    /// Same, from a parsed `Value` (useful when a caller already has one).
    pub fn load_and_migrate_value(v: &Value, sink: &mut dyn SecretSink) -> Result<Self> {
        let schema = v.get("schema").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        if schema > CURRENT_SCHEMA {
            // A higher-schema file was written by a future build that knows
            // fields we don't. `StateV2` doesn't set `deny_unknown_fields`, so
            // parsing it here would silently drop any new top-level or nested
            // key (only unknown *provider* buckets round-trip verbatim via the
            // `providers: BTreeMap<String, Value>` bag). Rather than let a
            // downgrade save-cycle overwrite the user's state.json with a
            // lossy version, refuse — matches the module docstring's
            // "refused" contract.
            anyhow::bail!(
                "state.json declares schema {schema}, but this build only understands \
                 schema {CURRENT_SCHEMA}; refusing to load to avoid dropping fields \
                 on save"
            );
        }
        if schema == CURRENT_SCHEMA {
            let s: StateV2 =
                serde_json::from_value(v.clone()).context("parsing state.json (v2)")?;
            return Ok(s);
        }
        // Anything without a `schema >= 2` is treated as v1 — including a
        // brand-new empty file. `StateV1::from_value` already tolerates
        // missing / partial fields, so this is a total function.
        let v1 = StateV1::from_value(v);
        Ok(Self::from_v1(&v1, sink))
    }

    /// Load state.json from disk and migrate in place. Missing file → default
    /// v2. Any bytes on disk go through `load_and_migrate_bytes`.
    pub fn load_and_migrate_path(path: &Path, sink: &mut dyn SecretSink) -> Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StateV2::default()),
            Err(e) => return Err(e).context("reading state.json"),
        };
        Self::load_and_migrate_bytes(&bytes, sink)
    }

    /// Default on-disk path — same as v1 (`~/.config/claude-usage/state.json`).
    pub fn default_path() -> Result<PathBuf> {
        Ok(crate::store::config_dir()?.join("state.json"))
    }

    /// Serialize this state to pretty JSON bytes.
    pub fn to_pretty_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).context("serializing state.json (v2)")
    }

    // ---- v1 → v2 migration -------------------------------------------------

    /// Build a v2 state from a v1 in-memory `State`. Every v1 account becomes
    /// an entry under `providers.claude.accounts`, keyed by the lowercased
    /// email (falling back to the account's `identity_uuid` or a `slot:N`
    /// synthetic — matching the trait's `account_identifier` fallback ladder).
    /// The v1 inline keychain blob is handed to `sink` alongside the
    /// `SecretRef` we stored so downstream can actually persist it; nothing
    /// about the token bytes ends up in the returned `StateV2`.
    pub fn from_v1(v1: &StateV1, sink: &mut dyn SecretSink) -> StateV2 {
        let mut bucket = ProviderBucket::default();
        let mut slot_counter: u32 = 0;

        for acc in &v1.accounts {
            let email = acc.email.clone().filter(|e| !e.is_empty());
            let identifier = if let Some(em) = &email {
                em.to_ascii_lowercase()
            } else if let Some(uuid) = acc.identity_uuid() {
                uuid
            } else {
                slot_counter += 1;
                format!("slot:{slot_counter}")
            };
            let secret_ref = claude_secret_ref(&identifier);
            if !acc.keychain_blob.is_empty() {
                sink.put(&secret_ref, &acc.keychain_blob);
            }

            let display = acc
                .oauth_account
                .as_ref()
                .and_then(|o| o.get("displayName"))
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .or_else(|| email.clone())
                .unwrap_or_else(|| identifier.clone());

            let identity = IdentitySnapshot {
                email: email.clone(),
                uuid: acc.identity_uuid(),
                display_name: acc
                    .oauth_account
                    .as_ref()
                    .and_then(|o| o.get("displayName"))
                    .and_then(|x| x.as_str())
                    .map(String::from),
                native_blob: serde_json::json!({
                    "oauthAccount": acc
                        .oauth_account
                        .clone()
                        .unwrap_or(Value::Null),
                    "userID": acc.user_id,
                }),
            };

            let cached_usage = acc.cached_usage.as_ref().map(cached_v1_to_v2);
            let account = AccountV2 {
                identifier: identifier.clone(),
                display,
                identity,
                secret_ref,
                expires_at_ms: acc.expires_at,
                cached_usage,
                history: HistoryV2::default(),
            };
            bucket.accounts.insert(identifier, account);
        }

        let mut providers = BTreeMap::new();
        if !bucket.accounts.is_empty() {
            // `serde_json::to_value` on a well-formed `ProviderBucket` cannot
            // fail (no non-string map keys, no `Serialize` panics); a
            // `Value::Null` is inserted only as a defensive fallback so a
            // hypothetical error still leaves the slug visible in the file.
            providers.insert(
                "claude".to_string(),
                serde_json::to_value(bucket).unwrap_or(Value::Null),
            );
        }

        let active = v1.active.clone().and_then(|acct| {
            let key = acct.to_ascii_lowercase();
            // Refuse to point `active.account` at a nonexistent bucket entry:
            // v1 stored `active` as a free-form email string, but the
            // migration above only inserted entries for accounts we could
            // actually key. A dangling active would break the menu's "bold
            // the active row" logic on the very first v2 load.
            providers
                .get("claude")
                .and_then(|v| v.get("accounts"))
                .and_then(|a| a.get(&key))
                .is_some()
                .then_some(ActiveRef {
                    provider: "claude".to_string(),
                    account: key,
                })
        });

        StateV2 {
            schema: CURRENT_SCHEMA,
            active,
            policy: PolicyV2 {
                autoswap_disabled: v1.autoswap_disabled,
                trigger_pct: v1.trigger_pct,
                per_provider: BTreeMap::new(),
            },
            providers,
        }
    }
}

// ---------------------------------------------------------------------------
// Secret sink — injected so v1→v2 migration is testable without touching a
// real keychain. Production code wires this to `KeychainSecretSink` (see
// bottom of module) which shells out to `security add-generic-password`.
// ---------------------------------------------------------------------------

pub trait SecretSink {
    /// Persist `blob` at the location identified by `secret_ref`. Errors are
    /// intentionally not reported: the migration is best-effort in v1, and a
    /// caller that cares can wrap a fallible sink and stash errors of its own.
    fn put(&mut self, secret_ref: &SecretRef, blob: &str);
}

/// Test-friendly sink that records every write.
#[derive(Debug, Default)]
pub struct MemorySecretSink {
    pub writes: Vec<(SecretRef, String)>,
}

impl SecretSink for MemorySecretSink {
    fn put(&mut self, secret_ref: &SecretRef, blob: &str) {
        self.writes.push((secret_ref.clone(), blob.to_string()));
    }
}

/// Sink that drops every write on the floor. Used when a caller only wants
/// the shape of the migration (e.g. a one-shot in-memory conversion) and the
/// secrets already live in the vendor CLI's own store.
#[derive(Debug, Default)]
pub struct NullSecretSink;

impl SecretSink for NullSecretSink {
    fn put(&mut self, _secret_ref: &SecretRef, _blob: &str) {}
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The `SecretRef` we point at for a migrated Claude v1 account. Per-account
/// keychain slots (service `claude-usage`, account `claude:<identifier>`) so
/// multiple captured accounts survive; the vendor CLI's own single-slot
/// `Claude Code-credentials` entry is left untouched.
fn claude_secret_ref(identifier: &str) -> SecretRef {
    SecretRef {
        backend: SecretBackend::Keychain,
        service: "claude-usage".to_string(),
        account: format!("claude:{identifier}"),
    }
}

fn cached_v1_to_v2(c: &crate::store::CachedUsage) -> CachedUsageV2 {
    let mut windows = Vec::new();
    let mut push = |id: &str, label: &str, pct: Option<f64>, reset: Option<&str>| {
        if pct.is_none() && reset.is_none() {
            return;
        }
        windows.push(WindowV2 {
            id: id.to_string(),
            label: label.to_string(),
            utilization: pct,
            resets_at: reset.and_then(parse_rfc3339_utc),
        });
    };
    push(
        "session",
        "5h",
        c.session_pct,
        c.session_reset.as_deref(),
    );
    push(
        "weekly",
        "7d",
        c.weekly_pct,
        c.weekly_reset.as_deref(),
    );
    push("opus", "Opus 7d", c.opus_pct, c.opus_reset.as_deref());
    CachedUsageV2 {
        // v1 stored seconds; v2 stores millis so it matches every other
        // timestamp field. Saturating so a value near i64::MAX doesn't panic.
        fetched_at_ms: c.fetched_at.saturating_mul(1000),
        windows,
    }
}

fn parse_rfc3339_utc(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v1_blob(access: &str, refresh: &str, expires_at: i64) -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": expires_at,
            }
        })
        .to_string()
    }

    fn v1_state_json_two_accounts() -> Value {
        serde_json::json!({
            "accounts": [
                {
                    "email": "Dev@Example.com",
                    "access_token": "a1",
                    "refresh_token": "r1",
                    "expires_at": 1_700_000_000_000i64,
                    "keychain_blob": v1_blob("a1", "r1", 1_700_000_000_000i64),
                    "oauth_account": {
                        "emailAddress": "dev@example.com",
                        "accountUuid": "uuid-dev",
                        "displayName": "Dev Person"
                    },
                    "user_id": "user-dev",
                    "cached_usage": {
                        "session_pct": 42.0,
                        "weekly_pct": 61.0,
                        "session_reset": "2026-09-06T17:00:00Z",
                        "weekly_reset": "2026-09-13T00:00:00Z",
                        "opus_pct": 12.0,
                        "opus_reset": "2026-09-13T00:00:00Z",
                        "fetched_at": 1_757_160_000i64
                    }
                },
                {
                    "email": "work@example.com",
                    "access_token": "a2",
                    "refresh_token": "r2",
                    "expires_at": 1_700_000_000_000i64,
                    "keychain_blob": v1_blob("a2", "r2", 1_700_000_000_000i64)
                }
            ],
            "active": "Work@Example.com",
            "autoswap_disabled": true,
            "trigger_pct": 90.0
        })
    }

    #[test]
    fn missing_schema_field_is_treated_as_v1() {
        let v = v1_state_json_two_accounts();
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(&v, &mut sink).unwrap();
        assert_eq!(s.schema, CURRENT_SCHEMA);
    }

    #[test]
    fn v1_accounts_are_wrapped_under_providers_claude() {
        let v = v1_state_json_two_accounts();
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(&v, &mut sink).unwrap();

        assert!(s.providers.contains_key("claude"));
        let bucket = s.known_bucket("claude").unwrap().unwrap();
        assert_eq!(bucket.accounts.len(), 2);
        // Keys are lowercased emails.
        let dev = bucket.accounts.get("dev@example.com").unwrap();
        assert_eq!(dev.identifier, "dev@example.com");
        assert_eq!(dev.display, "Dev Person");
        assert_eq!(dev.identity.email.as_deref(), Some("Dev@Example.com"));
        assert_eq!(dev.identity.uuid.as_deref(), Some("uuid-dev"));
        assert_eq!(dev.expires_at_ms, 1_700_000_000_000);

        let work = bucket.accounts.get("work@example.com").unwrap();
        assert_eq!(work.identifier, "work@example.com");
        // No oauth_account was set in v1, so display falls back to the email.
        assert_eq!(work.display, "work@example.com");
    }

    #[test]
    fn v1_active_is_migrated_to_typed_ref_and_lowercased() {
        let v = v1_state_json_two_accounts();
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(&v, &mut sink).unwrap();

        let active = s.active.expect("active should be migrated");
        assert_eq!(active.provider, "claude");
        assert_eq!(active.account, "work@example.com");
    }

    #[test]
    fn dangling_v1_active_is_dropped_rather_than_pointing_nowhere() {
        // `active` points at an account that isn't in the accounts array — v1
        // tolerated this because it was a free-form string; v2 must not
        // propagate a dead reference.
        let v = serde_json::json!({
            "accounts": [],
            "active": "ghost@example.com"
        });
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(&v, &mut sink).unwrap();
        assert!(s.active.is_none());
    }

    #[test]
    fn v1_policy_bits_are_carried_across() {
        let v = v1_state_json_two_accounts();
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(&v, &mut sink).unwrap();
        assert!(s.policy.autoswap_disabled);
        assert_eq!(s.policy.trigger_pct, Some(90.0));
    }

    #[test]
    fn inline_tokens_land_at_the_sink_not_in_state_json() {
        let v = v1_state_json_two_accounts();
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(&v, &mut sink).unwrap();

        // Both accounts had a keychain blob → both got a sink write.
        assert_eq!(sink.writes.len(), 2);
        let dev_ref = claude_secret_ref("dev@example.com");
        let work_ref = claude_secret_ref("work@example.com");
        let refs: Vec<&SecretRef> = sink.writes.iter().map(|(r, _)| r).collect();
        assert!(refs.contains(&&dev_ref));
        assert!(refs.contains(&&work_ref));

        // The persisted state.json bytes must not contain the blobs.
        let bytes = s.to_pretty_bytes().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("claudeAiOauth"));
        assert!(!text.contains("accessToken"));
        assert!(!text.contains("refreshToken"));

        // The secret_ref pointer does appear (backend + service + account).
        assert!(text.contains("\"backend\": \"keychain\""));
        assert!(text.contains("\"service\": \"claude-usage\""));
        assert!(text.contains("\"account\": \"claude:dev@example.com\""));
    }

    #[test]
    fn cached_usage_v1_becomes_ordered_windows_vec() {
        let v = v1_state_json_two_accounts();
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(&v, &mut sink).unwrap();
        let bucket = s.known_bucket("claude").unwrap().unwrap();
        let dev = bucket.accounts.get("dev@example.com").unwrap();
        let cu = dev.cached_usage.as_ref().expect("dev has cached usage");
        assert_eq!(cu.fetched_at_ms, 1_757_160_000_000);
        let ids: Vec<&str> = cu.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, vec!["session", "weekly", "opus"]);
        // Utilization comes through, and the reset timestamps are parsed as
        // real chrono values (not left as strings).
        assert_eq!(cu.windows[0].utilization, Some(42.0));
        assert!(cu.windows[1].resets_at.is_some());
    }

    #[test]
    fn schema_bump_is_persisted_on_save() {
        let v = v1_state_json_two_accounts();
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(&v, &mut sink).unwrap();
        let bytes = s.to_pretty_bytes().unwrap();
        let round: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(round.get("schema").and_then(|x| x.as_u64()), Some(2));
    }

    #[test]
    fn unknown_provider_buckets_round_trip_verbatim() {
        // A v2 file with a `providers.future-agent` bucket this build doesn't
        // know about — save it, reload it, ensure it survived.
        let original = serde_json::json!({
            "schema": 2,
            "active": { "provider": "future-agent", "account": "abc" },
            "policy": { "autoswap_disabled": false, "per_provider": {} },
            "providers": {
                "future-agent": {
                    "accounts": {
                        "abc": { "some": "future-shape", "nested": { "n": 1 } }
                    },
                    "extra_top_level_field": "kept"
                }
            }
        });
        let mut sink = MemorySecretSink::default();
        let loaded =
            StateV2::load_and_migrate_value(&original, &mut sink).unwrap();
        let bytes = loaded.to_pretty_bytes().unwrap();
        let round: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            round.pointer("/providers/future-agent"),
            original.pointer("/providers/future-agent")
        );
    }

    #[test]
    fn v2_file_without_a_v1_accounts_array_parses_directly() {
        // No `accounts` at the top level — the v1 fallback would produce an
        // empty state; the v2 path must handle this without a network / sink
        // side-effect, and preserve the typed `active` ref.
        let raw = serde_json::json!({
            "schema": 2,
            "active": { "provider": "claude", "account": "x@y.z" },
            "policy": {},
            "providers": {}
        });
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(&raw, &mut sink).unwrap();
        assert!(sink.writes.is_empty());
        assert_eq!(s.active.as_ref().unwrap().provider, "claude");
        assert_eq!(s.active.as_ref().unwrap().account, "x@y.z");
    }

    #[test]
    fn known_bucket_returns_none_for_missing_slug() {
        let s = StateV2::default();
        assert!(s.known_bucket("claude").unwrap().is_none());
    }

    #[test]
    fn set_known_bucket_replaces_or_inserts() {
        let mut s = StateV2::default();
        let mut b = ProviderBucket::default();
        b.accounts.insert(
            "x".to_string(),
            AccountV2 {
                identifier: "x".to_string(),
                display: "X".to_string(),
                identity: IdentitySnapshot {
                    email: None,
                    uuid: None,
                    display_name: None,
                    native_blob: Value::Null,
                },
                secret_ref: claude_secret_ref("x"),
                expires_at_ms: 0,
                cached_usage: None,
                history: HistoryV2::default(),
            },
        );
        s.set_known_bucket("claude", b).unwrap();
        assert_eq!(s.known_bucket("claude").unwrap().unwrap().accounts.len(), 1);
    }

    #[test]
    fn empty_v1_state_still_migrates_to_a_valid_v2_default() {
        // Truly empty input — mimicking a first-run install with no state
        // file. Must not panic and must yield the same shape as `Default`.
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(
            &Value::Object(Default::default()),
            &mut sink,
        )
        .unwrap();
        assert_eq!(s.schema, CURRENT_SCHEMA);
        assert!(s.active.is_none());
        assert!(s.providers.is_empty());
        assert!(sink.writes.is_empty());
    }

    #[test]
    fn v1_account_without_email_falls_back_to_uuid_then_slot_key() {
        // First: has an oauth_account UUID → keyed by UUID.
        let uuid_only = serde_json::json!({
            "accounts": [{
                "access_token": "a",
                "refresh_token": "r",
                "expires_at": 0,
                "keychain_blob": "",
                "oauth_account": { "accountUuid": "u-only" }
            }]
        });
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(&uuid_only, &mut sink).unwrap();
        let bucket = s.known_bucket("claude").unwrap().unwrap();
        assert!(bucket.accounts.contains_key("u-only"));

        // Second: nothing to key on → `slot:1` synthetic.
        let anon = serde_json::json!({
            "accounts": [{
                "access_token": "a",
                "refresh_token": "r",
                "expires_at": 0,
                "keychain_blob": ""
            }]
        });
        let mut sink = MemorySecretSink::default();
        let s = StateV2::load_and_migrate_value(&anon, &mut sink).unwrap();
        let bucket = s.known_bucket("claude").unwrap().unwrap();
        assert!(bucket.accounts.contains_key("slot:1"));
    }

    #[test]
    fn schema_higher_than_current_is_refused() {
        // A file from a future build carries fields this build doesn't know.
        // `StateV2` doesn't `deny_unknown_fields`, so silently parsing it as
        // v2 would drop any new key on save — exactly the downgrade-loses-
        // data outcome the `providers: BTreeMap<String, Value>` design
        // exists to prevent. `load_and_migrate_value` must refuse.
        let raw = serde_json::json!({
            "schema": 3,
            "active": { "provider": "claude", "account": "a" },
            "policy": { "autoswap_disabled": true, "some_future_knob": 42 },
            "providers": {}
        });
        let mut sink = MemorySecretSink::default();
        let err = StateV2::load_and_migrate_value(&raw, &mut sink)
            .expect_err("a higher-schema file must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("schema 3") && msg.contains("schema 2"),
            "error should name both schemas: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // Full-round-trip: v1 fixture → in-memory v2 → serialize → reparse →
    // shape-identical. Guards against a serde attribute drift that would
    // otherwise silently drop fields on save.
    // ------------------------------------------------------------------
    #[test]
    fn v1_to_v2_roundtrip_is_stable() {
        let v = v1_state_json_two_accounts();
        let mut sink = MemorySecretSink::default();
        let s1 = StateV2::load_and_migrate_value(&v, &mut sink).unwrap();
        let bytes = s1.to_pretty_bytes().unwrap();
        let s2 = StateV2::parse_v2_bytes(&bytes).unwrap();
        assert_eq!(s1, s2);
    }
}
