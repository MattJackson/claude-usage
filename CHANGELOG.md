# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Proactive flip-back.** Auto-swap now returns to an account after its 5h session
  resets — as long as it's still the best one to be on (soonest weekly reset) — so
  the daemon keeps draining each account's weekly quota in order instead of stranding
  it after a single session. Guarded by the existing swap cooldown / no-return
  window, plus a headroom margin so two accounts sharing a weekly reset don't
  ping-pong. Proactive swaps are labelled "Flipped back to …" and logged with
  `"reason": "proactive"`.
- **Priority-ordered menu.** The menu-bar dropdown lists accounts by swap priority
  (the account to use next on top, maxed-out ones last) instead of insertion order.
  The CLI keeps insertion order.

### Changed
- **Swap-target eligibility is now session-gated.** A swap/return target must have a
  **session** at or below the ceiling and a **weekly** below the trigger, rather than
  requiring the max of both under the ceiling. This lets the daemon return to an
  account whose weekly is high (but not yet maxed) once its session frees up, to
  finish spending that weekly before it resets.

## [0.2.0] - 2026-09-05

Post-audit hardening milestone. A full multi-lens code audit (10 review lenses
plus opus escalation and adversarial verification rounds) drove the following
fixes; every top finding was independently confirmed before fixing.

### Fixed
- **Concurrent token clobbering.** A `switch` and the background poll each
  snapshot an account's tokens *before* taking the state lock, then wrote them
  back unconditionally — so a refresh-token rotation landing in that window was
  overwritten with a stale, already-superseded single-use token, breaking the
  next refresh. Token writes are now recency-guarded (`set_tokens_if_newer`), and
  `switch` uses whichever tokens are freshest at commit time.
- **Auto-swap overriding a manual switch.** The daemon chose a swap target from a
  snapshot then switched with no compare-and-set; a manual switch in the gap was
  silently reverted. The swap now only commits if the expected account is still
  active.
- **`expires_at` overflow.** `ensure_fresh` used non-saturating arithmetic; a
  corrupt `expires_at` (e.g. near `i64::MIN` from a malformed state.json) could
  panic the daemon (debug) or wrap and never refresh (release). Now saturating.
- **OAuth refresh without a rotated token.** The refresh response *required*
  `refresh_token`, but it's optional (RFC 6749 §6); a valid response omitting it
  aborted the refresh. The existing refresh token is now kept in that case.
- **Half-applied switch hardening.** The `~/.claude.json` rollback no longer
  swallows its own error (a double-fault is surfaced) and now preserves the
  file's permission mode; its temp file is cleaned up on failure.
- **Hot-swap relaunch never exits into a dead state.** If `launchctl kickstart`
  (launchd) or the bare-run self-spawn fails, the app now stays alive on the
  current binary and logs it, instead of exiting with no replacement.
- **Re-capture no longer wipes cached usage.** `capture` on an existing account
  now preserves its usage snapshot (it only refreshes identity/tokens).
- **`report` no longer mixes accounts.** Weekday/hour consumption deltas are only
  computed between consecutive samples of the *same* account.
- **Log hygiene.** The usage-fetch error no longer folds the raw HTTP response
  body into the error/debug log (matching the token endpoint's existing rule).
- **`toggle_login_item`** now surfaces an osascript failure instead of silently
  no-opping, and the Login Item AppleScript escapes the interpolated path.

### Changed
- **Menu-bar CPU/IO.** The 0.75s UI tick no longer re-reads+parses `state.json`
  every tick (now gated on file mtime) nor forks an `osascript` to probe the
  Login Item every tick (now probed at most once a minute, refreshed instantly
  when toggled).
- Menu redraw signature now includes the Opus reset countdown, so a changing Opus
  reset no longer leaves a stale value on screen.
- `left_at` no-return state is pruned so it can't grow unbounded over a long-lived
  daemon.

### Internal
- Shared log-rotation helper (`logging::rotate_if_large`) used by both the debug
  log and history.jsonl. Extracted pure helpers (`identity_matches`,
  `launchd_managed_from_env`, `write_bytes_atomic_mode`) and added 10 unit tests
  covering the keychain-adoption gate, swap hysteresis, backoff, atomic-write
  mode application, and UTF-16 styling offsets (incl. astral characters).

## [0.1.10] - 2026-09-05

### Fixed
- **Hot-swap on `brew upgrade` now actually restarts the menu bar.** The running
  app is a launchd agent, and the old relaunch spawned a child then exited — but
  the child was in the job's process group, which launchd SIGKILLs when the main
  process exits (`AbandonProcessGroup` defaults false), so the replacement died
  with us and (with `KeepAlive=false`) was never restarted. The launchd instance
  now asks launchd to restart the job (`launchctl kickstart -k gui/<uid>/<label>`)
  instead of self-spawning; bare/from-source runs keep the orphan-survives
  self-spawn. (Upgrading *from* 0.1.9 still needs one manual restart since the old
  binary does the relaunch; 0.1.10 onward hot-swaps correctly.)

## [0.1.9] - 2026-09-05

### Changed
- **Menu polish, battery-menu style.** Account rows now render their trailing
  `S% / W%` **right-aligned** at a fixed tab stop, the **active account is bold**
  (the checkmark on the row is gone), and any percentage in a danger band is
  **colored** — amber at ≥80%, red at ≥95% — so a nearly-spent account is obvious
  at a glance. The same coloring applies to the top info line and the per-account
  session/weekly/opus stat rows.

### Internal
- These effects need `NSMenuItem.attributedTitle`, which muda's plain-string API
  can't set. Rather than depend on an unreleased muda fork, we build the muda menu
  as before (so clicks/structure/events are unchanged) then walk the native
  `NSMenu` via muda's public `ns_menu()` and set attributed titles ourselves with
  objc2 (right `NSTextTab` paragraph style, bold `NSFont`, `systemOrange`/`systemRed`
  over the percentage ranges). Upstream muda PR
  [#399](https://github.com/tauri-apps/muda/pull/399) adds a typed
  `set_attributed_title`; if it merges and ships we migrate the ~80-line objc2
  helper to a couple of calls.

## [0.1.8] - 2026-09-05

### Added
- **Hot-swap on `brew upgrade`.** A running menu-bar app detects when the binary
  is replaced and relaunches itself into the new version — no manual restart. The
  launchd login item now targets the stable `<brew-prefix>/bin/claude-usage`
  symlink instead of the versioned Cellar path an upgrade deletes.

### Removed
- The menu's "Refresh now" item. It was the only user-triggered off-schedule
  usage fetch and could contribute to rate limiting; usage now refreshes solely on
  the scheduler tick.

## [0.1.7] - 2026-09-05

### Fixed
- **The menu-bar menu no longer closes on its own.** Dropped the `tao` dependency
  and drive the app on a native `NSApplication` run loop with an `NSTimer`
  scheduled in the default run-loop mode, so the open status menu (which runs in
  `NSEventTrackingRunLoopMode`) is never dismissed by the event loop. Root cause:
  tao registered its run-loop observer/timer/source in `kCFRunLoopCommonModes`
  (upstream: tauri-apps/tao#1324, PR #1325).
- "updated Xm ago" now actually ticks — the menu re-renders from cached state
  every second instead of only on a poll.

### Added
- Auto-swap submenu: **"Switch to best account now"** — immediately move to the
  account that has room and whose weekly limit resets soonest (stays put if you're
  already on the best one).

### Changed
- Account headers in the menu read `email   xx% / xx%` (dropped the S/W prefixes).

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

[Unreleased]: https://github.com/MattJackson/claude-usage/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/MattJackson/claude-usage/compare/v0.1.10...v0.2.0
[0.1.10]: https://github.com/MattJackson/claude-usage/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/MattJackson/claude-usage/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/MattJackson/claude-usage/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/MattJackson/claude-usage/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/MattJackson/claude-usage/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/MattJackson/claude-usage/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/MattJackson/claude-usage/compare/v0.1.2...v0.1.4
[0.1.2]: https://github.com/MattJackson/claude-usage/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/MattJackson/claude-usage/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/MattJackson/claude-usage/releases/tag/v0.1.0
