# usagio — manual smoke test: Codex end-to-end pipeline

Audience: Matthew, running this by hand on macOS. Companion to
`docs/testing-0.5.0.md` (Claude's contract tests) and
`docs/architecture-0.5.0.md`.

## Read this first — what this build actually does for Codex

An audit of `src/providers/codex/**`, `src/main.rs`, and `src/menubar.rs` on
`dev` (2026-09-07) found that Codex's **credential-lifecycle primitives are
complete and unit-tested**, but **nothing in the live app calls them for
switching or active-token refresh yet**. Specifically:

- `CodexProvider::capture_current_login` (reads `~/.codex/auth.json`) —
  wired: `menubar.rs::handle_capture` calls it for any non-Claude slug.
- `CodexProvider::write_active_account` (atomic, mode-0600 `auth.json`
  rewrite) — implemented and tested in isolation, but **no caller exists**.
  `menubar.rs::handle_switch` hard-gates on `slug != CLAUDE_SLUG` and shows
  "Switching is not yet supported for codex" before it would ever reach this
  method. `main.rs::switch_to` / `cmd_switch` (the CLI `usagio switch`) are
  likewise hardcoded to `provider_by_slug(CLAUDE_SLUG)`.
- `CodexProvider::mirror_rotated_token` / `codex::oauth::active_refresh_cas`
  — implemented and tested in isolation (`oauth_tests.rs`), but
  `main.rs::active_refresh_cas` (the function the live poll cycle calls) is
  hardcoded to the Claude provider and Claude's keychain-shaped `Account`
  type; it never touches a Codex account. `Capabilities::supports_active_refresh`
  is never read anywhere outside its own definition and doc comments.
- Root cause (documented in code as **H3, v0.5.0 codeaudit**,
  `src/providers/codex/mod.rs` module doc): `capabilities().supports_switching`
  is deliberately `false` for Codex because v1 `State`/`state.json` has no
  bucket to persist a *second* Codex account to switch to — `handle_capture`
  for Codex doesn't even persist the captured account into `state.json`
  ("Captured Codex account (persistence lands in a later phase)").

**Practical effect: today, a user cannot click-switch between two Codex
accounts in the menu bar, and there is no background process that will
proactively refresh a Codex `auth.json` on usagio's own cadence.** Both are
blocked on "state v2" (a real multi-account slot for non-Claude providers),
which is a larger design change than this checklist can validate — see the
BLOCKER note at the end.

What you *can* smoke-test today, and what this checklist covers:

1. Capture correctly reads whichever Codex account is currently logged in.
2. The menu bar shows the Codex account and its usage.
3. `write_active_account` and the CAS refresh primitives are individually
   correct (via direct file inspection / a throwaway CLI harness), even
   though nothing in the shipped app calls them yet.
4. `codex --version` / real `codex` CLI usage is unaffected by usagio being
   installed.

## Prerequisites

```sh
usagio --version                       # → usagio 0.5.0 (or newer)
pgrep -f 'usagio menubar'              # → a PID (menu-bar app is running)
codex --version                        # → confirms the codex CLI itself works
ls -la ~/.codex/auth.json              # → mode -rw------- (0600)
tail -f ~/.config/usagio/usagio.log &  # keep this running in a side terminal
```

---

## T1 — Capture a Codex account

1. `codex login` (or however you normally sign in to Codex) with account A.
2. Confirm `~/.codex/auth.json` exists and is mode `0600`:
   ```sh
   stat -f '%Lp' ~/.codex/auth.json    # → 600
   ```
3. Open the usagio menu bar icon. Codex's account row should appear with A's
   email and usage (this exercises `capture_current_login` +
   `fetch_usage`/`USAGE_URL`).
4. From the menu, use whatever "Capture"/"Refresh" action exists for Codex
   (or run `usagio capture --provider codex` if the CLI exposes it in your
   build; check `usagio --help`). Confirm the log shows a capture-style
   event and the notification reads
   `"Captured Codex account (persistence lands in a later phase)"` — that
   exact wording confirms you're on the current, not-yet-persisted path.

## T2 — Second account does NOT get a switchable slot (expected today)

1. Sign out of A and `codex login` as account B.
2. Re-open the usagio menu. You should see B's usage now (capture always
   reflects whatever is currently in `auth.json`), but there is **no
   "Switch to this account" row** for either A or B — `supports_switching`
   is `false`, so `build_account_submenu` never builds one. This is
   expected, not a bug you need to chase.
3. If you deliberately construct a `switch:codex:<key>` click id (e.g. via a
   debug build), confirm it's rejected with the notification "Switching is
   not yet supported for codex" and `~/.codex/auth.json` is untouched.

## T3 — `write_active_account` correctness (offline, without state v2)

Since nothing in the shipped app calls `write_active_account` yet, validate
it directly against a scratch `$CODEX_HOME` so you don't risk your real
`auth.json`:

```sh
export CODEX_HOME=/tmp/codex-smoketest
mkdir -p "$CODEX_HOME"
cp ~/.codex/auth.json "$CODEX_HOME/auth.json"   # seed with a real, valid blob

# Exercise the provider's unit-level behavior via the existing test suite
# instead of `cargo test` on your real machine (per the no-cargo-test-on-
# this-Mac constraint) — run it in CI or a throwaway VM/container instead:
#   cargo test -p usagio providers::codex:: -- --nocapture
```

Confirm (by reading `$CODEX_HOME/auth.json` after any such run):
- The file is still valid JSON with a non-empty `tokens.access_token`.
- Permissions are `0600` on the file and `0700` on `$CODEX_HOME`.
- No `.auth.json.tmp.*` files are left behind.

## T4 — Refresh-token rotation against the real endpoint

This exercises `codex::oauth::refresh_token_grant` / `active_refresh_cas`
against the real `https://auth.openai.com/oauth/token` endpoint using your
real refresh token — do this only with an account you're comfortable
possibly forcing a re-login on if something goes wrong.

1. Note the current `tokens.refresh_token` prefix in `~/.codex/auth.json`
   (first 8 chars is enough — never paste the full token anywhere).
2. Let the token approach its natural expiry, or manually back-date
   `last_refresh` in `auth.json` by more than 8 days to trip the vendor's
   own staleness cadence (`SESSION_STALE_AFTER_DAYS`).
3. Run `codex` normally (any command that touches the network) and let the
   vendor CLI itself refresh — this is the reference behavior; usagio isn't
   involved in this app version, since nothing calls
   `codex::oauth::active_refresh_cas` from the live poll loop yet.
4. Confirm `auth.json`'s `tokens.access_token` and `tokens.refresh_token`
   changed and `last_refresh` updated to now.
5. Run `codex --version` and a real Codex command afterward to confirm the
   CLI is still fully functional post-rotation.

## T5 — CAS win/lose/skip logging (currently N/A for Codex)

`grep 'active_refresh_cas' ~/.config/usagio/usagio.log` today will only ever
show Claude events (`event=active_refresh_cas_won/lost/skipped_drift`,
emitted by `main.rs::active_refresh_cas`) — there is no equivalent Codex
line to look for yet, because `main.rs`'s poll cycle never calls
`codex::oauth::active_refresh_cas`. Do not spend time hunting for a Codex
CAS log line in this build; its absence is expected, not a bug.

---

## BLOCKER for v0.5.0 sign-off

**Codex account switching (menu-bar click and `usagio switch`) and Codex's
own active-refresh cadence are not reachable from the live app.** The
provider-level code (`write_active_account`, `mirror_rotated_token`,
`codex::oauth::active_refresh_cas`, `supports_active_refresh`) is complete
and covered by its own unit tests, but:

- `state.json` (v1) has no slot to persist a second Codex account, so
  `handle_capture` for Codex explicitly does not persist what it captures.
- `menubar.rs::handle_switch` and `main.rs::switch_to`/`cmd_switch` are
  hardcoded to the Claude provider slug; Codex hits an explicit "not yet
  supported" guard before any provider code runs.
- `main.rs::active_refresh_cas`/`refresh_usage_cache` (the function the
  polling/menu-bar cadence actually calls) is hardcoded to
  `provider_by_slug(CLAUDE_SLUG)` and Claude's keychain-shaped `Account`
  type; it never iterates non-Claude accounts, so `supports_active_refresh`
  is dead weight today.

This is a known, already-documented gap in the code itself (see the "H3,
v0.5.0 codeaudit" comments in `src/providers/codex/mod.rs` and
`src/menubar.rs::handle_switch`), not something newly discovered here — but
it means the *end-to-end* pipeline this checklist was asked to validate
("menu bar → click a Codex account → active vendor slot changes → `codex`
CLI picks up the new account") **does not exist in this build**. Closing it
requires a "state v2" change (a real per-provider multi-account slot,
touching `src/store.rs`, `src/main.rs`, and `src/menubar.rs`) — out of scope
for a small wiring fix and out of this audit's file-edit permissions.
