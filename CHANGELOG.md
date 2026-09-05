# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.6] - 2026-09-05

### Fixed
- Keychain access reverted to the `security` CLI. v0.1.5's Security.framework
  (`security-framework`) call made macOS prompt for Keychain access on every launch
  from the unsigned brew binary; the CLI path doesn't prompt. (The write blob is
  visible in `security`'s argv — LOW risk under the single-user threat model. This
  will move back to Security.framework once the app ships code-signed.)

### Removed
- Dead `ask_name` menu helper (capture/remove no longer prompt for a name) and the
  now-unused `security-framework` dependency.

## [0.1.5] - 2026-09-05

### Changed
- **Accounts are now identified by email, not a made-up name.** `capture` takes no
  name and keys on the account's email; `switch`/`start`/`continue`/`token`/`rm`
  accept a full email or a unique prefix (ambiguous prefixes error and list the
  matches). Old name-keyed `state.json` files migrate automatically on load
  (email backfilled from the stored identity; the active name mapped to its email).
- Removed the `rename` command/menu item (emails are fixed by the account).
- Menu bar: each account is now a **submenu** (switch · session/weekly/opus stats
  with reset countdowns · "updated Xm ago" · Remove); auto-swap is a single
  **"Auto-swap at high usage" submenu** (Off / 90 / 95 / 98); "Quit claude-usage"
  is now just "Quit".
- Keychain access goes through Security.framework (`security-framework`) instead of
  the `security` CLI, so tokens are never passed on a process command line (argv).

### Fixed
- **Switching is now atomic.** The identity is resolved first (a switch to an
  identity-less account while offline now errors instead of half-applying);
  `~/.claude.json` is written before the keychain, and the keychain write is the
  commit point — on failure `~/.claude.json` is rolled back. No more "switch
  reported success but the account didn't change."
- **`sync_active_from_keychain` verifies identity** (accountUuid, fallback email)
  before adopting keychain tokens, so a `/login` into a different account no longer
  silently overwrites the tracked account's tokens.
- **Cross-process state lock.** All state read-modify-writes take an advisory file
  lock, and the scheduler does its network I/O outside the lock then reloads and
  merges under it — a concurrent daemon poll and CLI/menu switch can no longer
  clobber each other.
- Auto-pick / auto-swap tie-break now prefers **more** headroom (lower usage) among
  equally-soon-resetting accounts (was inverted).
- `~/.claude.json`: the previous account's `userID` is removed when the new account
  has none (no stale identity pairing).
- `history.jsonl` is now size-capped/rotated like the debug log; the launchd agent
  no longer captures an uncapped stdout/stderr log.
- Menu-bar: the snapshot is computed before taking the UI lock (no stalls across
  blocking `osascript`/disk reads); a poisoned lock is recovered consistently.
- Saturating arithmetic on token-expiry and cache-age math; temp files cleaned up
  on rename failure and created owner-only; OAuth error bodies are no longer logged.

## [0.1.4] - 2026-09-05

### Added
- `rename` command (CLI `claude-usage rename <old> <new>`, and Rename/Remove in
  the menu bar) for managing captured accounts.
- `--version` / `-V`.
- Per-account **cached usage** in state, refreshed only by the scheduler.
- A debug log at `~/.config/claude-usage/claude-usage.log` (no secrets).
- Unit tests (store/usage/main/oauth) and black-box CLI integration tests.

### Changed
- **Usage is fetched only on the scheduler tick, never on switch or ad-hoc
  commands.** `list` and the menu bar read the cache (shown as "updated Xm ago"),
  so ordinary use can no longer trigger HTTP 429s.
- Menu bar runs **Dock-less** (accessory activation policy) — truly background.
- The open menu is no longer dismissed by background refreshes — it rebuilds only
  when the displayed data actually changes.
- Profile-backfilled account identity is now **persisted**, so accounts captured
  before the identity fix self-heal on first switch and later switches make no
  network calls.
- CI hardening: build-provenance attestation, SHA-pinned actions, concurrency,
  `--locked` release builds.

### Fixed
- Transient usage-fetch errors (e.g. 429) keep the last-known percentage instead
  of showing `!`, with exponential poll backoff.
- `~/.claude.json` rewrites preserve the original file mode and no longer re-sort
  keys (`preserve_order`).
- Case-insensitive account matching also applies when clearing the active account
  on `rm`.

## [0.1.2] - 2026-09-05

### Fixed
- **Switching now actually changes the active account.** A switch also writes the
  `oauthAccount` identity in `~/.claude.json` (not just the Keychain token), which
  is what Claude Code uses to select the account. `capture` snapshots this
  identity; existing accounts are backfilled from the profile API on switch.
- Account names are matched case-insensitively (`personal` == `Personal`).

### Changed
- Removed in-app auto-update; upgrades are handled by `brew upgrade`.
- Menu bar: percent-only title showing the **session** (5h) utilization, a version
  line, "Launch at login" clarified, and instant refresh when the active account
  changes from the CLI.
- Switch messaging clarified: new `claude` sessions use the account; already-running
  sessions keep theirs until restarted.

## [0.1.1] - 2026-09-05

### Changed
- Release automation: each `v*.*.*` tag now auto-bumps the Homebrew tap formula,
  so `brew upgrade` picks up new versions.
- Releases fail fast if the git tag doesn't match the `Cargo.toml` version.

## [0.1.0] - 2026-09-05

### Added
- Multi-account usage dashboard (`list`) showing the 5-hour session and 7-day
  weekly utilization for every captured account, with reset countdowns.
- `capture` — snapshot the Claude account you are currently logged into from the
  macOS Keychain and store it under a friendly name.
- Instant account switching (`switch` / `start` / `continue`) by writing the
  chosen account's login into the shared Keychain item, which every running
  `claude` process adopts on its next request — no browser `/login`.
- Auto-pick: with no name, `switch`/`start`/`continue` choose an account that has
  headroom and whose weekly limit resets soonest.
- `watch` — auto-swap daemon that moves off an account at 95% utilization to one
  with room, using a hysteresis band (trigger 95% / target ≤85%), swap cooldown,
  and no-bounce-back to avoid thrashing. Native notifications on swap and when no
  account has room.
- macOS **menu-bar app** (`menubar`) — live usage % in the menu bar, click-to-switch
  accounts, auto-swap toggle (90/95/98), capture-login, start-at-login, and quit.
- `install` / `uninstall` — run the menu-bar app (which includes the auto-swap
  daemon) at every login via a launchd agent.
- **Self-update** (`update`, plus a once-daily background check) — downloads the
  latest release, verifies its SHA-256 checksum, replaces the binary, and relaunches.
- `report` — usage patterns by weekday, hour of day, and per-account weekly peak.
- `token` — print a fresh access token for scripting.
- Local, owner-only token store at `~/.config/claude-usage/state.json` (0600).

[Unreleased]: https://github.com/MattJackson/claude-usage/compare/v0.1.6...HEAD
[0.1.6]: https://github.com/MattJackson/claude-usage/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/MattJackson/claude-usage/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/MattJackson/claude-usage/compare/v0.1.2...v0.1.4
[0.1.2]: https://github.com/MattJackson/claude-usage/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/MattJackson/claude-usage/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/MattJackson/claude-usage/releases/tag/v0.1.0
