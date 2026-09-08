# usagio — manual smoke test: Codex end-to-end pipeline

Audience: Matthew, running this by hand on macOS. Companion to
`docs/testing-0.5.0.md` (Claude's contract tests) and
`docs/architecture-0.5.0.md`.

## Read this first — what changed (codex-switch-e2e, closed the v0.5.0 BLOCKER)

The 2026-09-07 audit of `src/providers/codex/**`, `src/main.rs`, and
`src/menubar.rs` found that Codex's credential-lifecycle primitives were
complete and unit-tested, but nothing in the live app called them for
switching or active-token refresh. The `codex-switch-e2e` work (now merged to
`dev`) closed that gap by giving `state.json` a real "state v2" multi-account
slot for non-Claude providers and wiring every dispatch point through it:

- **`state.json` schema v2** (`src/store.rs`): a new `providers` map
  (`HashMap<String, ProviderAccounts>`) holds every captured account for a
  non-Claude provider, keyed by provider slug, plus that provider's own
  `active` selection — the same shape Claude's `accounts`/`active` fields
  have always given Claude. `schema_version` is bumped to `2`. A v1
  `state.json` (no `schema_version`, no `providers` key) loads exactly as
  before — every v2 field is `#[serde(default)]` and purely additive — see
  `store_tests.rs::v1_state_json_loads_and_upgrades_to_v2` for the pinned
  fixture.
- **`menubar.rs::handle_capture`** persists a captured non-Claude account
  into `state.providers[slug]` and sets it active, instead of the old
  "persistence lands in a later phase" notification.
- **`menubar.rs::handle_switch`** and the CLI's **`cmd_switch`**
  (`usagio switch <selector>`) dispatch by provider slug —
  `switch_to_provider_account(slug, key)` calls
  `Provider::write_active_account` and updates `state.providers[slug].active`
  — instead of hard-gating on Claude.
- **`CodexProvider::capabilities().supports_switching`** is now `true`
  (H3, v0.5.0 codeaudit — closed): the menu's "Switch to this account" row
  and account list now render for Codex, backed by real captured accounts.
- **`main.rs`'s poll cycle** now dispatches the active-account CAS refresh
  per provider, gated on `Provider::supports_active_refresh()` (previously
  read nowhere outside its own doc comment). For Codex,
  `refresh_provider_active_account("codex")` drives
  `providers::codex::oauth::active_refresh_cas` (the file-CAS on
  `auth.json`) instead of any Claude-shaped flow; Claude's own path is
  unchanged behaviorally (`ClaudeProvider::supports_active_refresh` now
  explicitly returns `true` to keep its existing CAS running under the new
  gate).
- **`CodexProvider::absorb_credential`** now actually persists a rotated
  `auth.json` blob into `state.providers["codex"]` for an already-captured
  account (mirrors `ClaudeProvider::absorb_credential`), instead of no-op.

All of the above is covered by unit/integration tests that run under
`cargo test` (hermetic — no real keychain, no real `~/.codex/auth.json`, no
real network; see `src/store_tests.rs`, `src/main_tests.rs`,
`src/providers/codex/mod.rs`'s `absorb_credential_*` tests). This checklist
now covers exercising the same pipeline against the REAL `codex` CLI and a
real menu bar, which the automated suite can't do.

## Prerequisites

```sh
usagio --version                       # → usagio 0.5.0 (or newer)
pgrep -f 'usagio menubar'              # → a PID (menu-bar app is running)
codex --version                        # → confirms the codex CLI itself works
ls -la ~/.codex/auth.json              # → mode -rw------- (0600)
tail -f ~/.config/usagio/usagio.log &  # keep this running in a side terminal
```

---

## T1 — Capture a Codex account (now persists)

1. `codex login` (or however you normally sign in to Codex) with account A.
2. Confirm `~/.codex/auth.json` exists and is mode `0600`:
   ```sh
   stat -f '%Lp' ~/.codex/auth.json    # → 600
   ```
3. Open the usagio menu bar icon. Codex's account row should appear with A's
   email and usage (this exercises `capture_current_login` +
   `fetch_usage`/`USAGE_URL`).
4. From the menu, use the "Capture current login ▸ Codex" action (or
   `usagio capture` if the CLI exposes a `--provider` flag in your build;
   check `usagio --help`). Confirm the notification now reads
   `"Captured Codex <email>"` (not the old "persistence lands in a later
   phase" wording), and confirm the capture landed in state:
   ```sh
   cat ~/.config/usagio/state.json | python3 -c \
     'import json,sys; d=json.load(sys.stdin); print(d["providers"]["codex"])'
   ```
   should show your captured account under `accounts`, and `active` set to
   its key.

## T2 — A second account IS switchable

1. Sign out of A and `codex login` as account B. Capture B the same way (T1
   step 4) — state now holds both A and B under `providers.codex.accounts`.
2. Re-open the usagio menu. Both A and B should appear as Codex sub-rows,
   each with a **"Switch to this account"** row — `supports_switching` is
   now `true`, so `build_account_submenu` builds it.
3. Click "Switch to this account" on A (while B is active). Confirm:
   - The notification reads `"Switched to a@..."`.
   - `~/.codex/auth.json` now holds A's blob (`stat`/`cat` to check the
     `tokens.access_token` matches A's captured token).
   - The usagio log shows `event=capture` / a switch-adjacent entry — check
     `tail -f ~/.config/usagio/usagio.log` for the switch.
   - Running `codex` (any command that reads its own login) picks up
     account A, confirming the switch is real from the vendor CLI's
     perspective, not just usagio's own state.
4. Switch back to B via the menu and repeat the `auth.json` check.

## T3 — CLI switch by provider

```sh
usagio switch a@example.com     # resolves against state.providers.codex too
```
Confirm this prints `Active login is now a@example.com (codex).` and
`~/.codex/auth.json` matches A's blob — this exercises
`main.rs::cmd_switch`'s fallback to `resolve_provider_selector` /
`switch_to_provider_account` when the selector doesn't match a Claude
account.

## T4 — Active-account CAS refresh against the real endpoint

This exercises `codex::oauth::active_refresh_cas` — now reachable from the
live poll cycle via `main.rs::refresh_provider_active_account("codex")` —
against the real `https://auth.openai.com/oauth/token` endpoint using your
real refresh token. Do this only with an account you're comfortable possibly
forcing a re-login on if something goes wrong.

1. Make sure the account you're testing is CAPTURED and ACTIVE for Codex
   (T1/T2 above) so `state.providers.codex.active` points at it.
2. Note the current `tokens.refresh_token` prefix in `~/.codex/auth.json`
   (first 8 chars is enough — never paste the full token anywhere).
3. Manually back-date `last_refresh` in `auth.json` by more than 8 days (or
   wait for natural expiry) to trip `SESSION_STALE_AFTER_DAYS`.
4. Wait for usagio's next poll cycle (`WATCH_INTERVAL_SECS`, ~150s) rather
   than running `codex` yourself — the point of this test is confirming
   USAGIO's own poll cycle performs the refresh now, not the vendor CLI.
5. Confirm `auth.json`'s `tokens.access_token`/`refresh_token` changed and
   `last_refresh` updated to now, AND that `state.providers.codex`'s
   matching account in `state.json` shows the same new `access_token` (the
   CAS win is mirrored back into state, not just the file).
6. Run `codex --version` and a real Codex command afterward to confirm the
   CLI is still fully functional post-rotation.

## T5 — CAS win/lose/skip logging

```sh
grep 'active_refresh_cas' ~/.config/usagio/usagio.log
```
should now show BOTH Claude events
(`event=active_refresh_cas_won/lost/skipped_drift`) AND Codex events
(`event=active_refresh_cas_failed provider=codex ...` on a failure path; a
clean win/adopt doesn't itself log a line from `main.rs` today beyond the
provider-level `oauth.rs` behavior — if you want a affirmative "codex CAS
won" log line for a specific run, cross-reference `auth.json`'s
`last_refresh` timestamp against the poll cadence instead).

---

## Automated coverage (run in CI, not on this Mac per the keychain-safety rule)

```sh
cargo test --all-features
```

covers, hermetically (in-memory keychain / tempdir `$CODEX_HOME` and
`state.json`, no real notifications):

- `store_tests.rs`: v1→v2 schema migration losslessness, provider-account
  upsert/remove, save-refusal-on-silent-drop for provider accounts (mirrors
  the existing Claude guard).
- `main_tests.rs`: `capture_current_generic` persistence, provider-account
  switch (including the auth.json rewrite + state `active` update),
  `resolve_provider_selector` prefix matching, and
  `refresh_provider_active_account`'s gate (`supports_active_refresh`) plus
  a full mocked-network CAS refresh round trip for Codex.
- `providers/codex/mod.rs`: `absorb_credential` now persisting into state
  v2 for an existing account, and a no-op for an unknown one.
