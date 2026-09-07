# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.1] - 2026-09-07

Hotfix for a file-descriptor leak that could exhaust the macOS default
NOFILE cap (256) within hours of running the menu-bar app, causing every
subsequent state.json read, DNS lookup, and Keychain call to fail with
`Too many open files`. Symptom the user sees: `capture failed`, blank
usage cells, `usagio switch` refusing to run.

### Fixed
- **fsnotify watcher no longer watches `$HOME`.** `Provider::credential_paths`
  for Claude returns `~/.claude.json` whose parent directory is the user's
  home. Registering a watcher on that parent turned the NonRecursive-on-macOS
  emulation into a file-descriptor firehose — every log file under
  `~/.config/gcloud/logs/`, image caches, etc. got tracked. `spawn_watchers`
  now skips parents equal to `$HOME` or `/`; the periodic `absorb_all_lagging`
  poll picks up `~/.claude.json` writes within one watch cycle (~30s worst
  case vs. ~2s under fsnotify).
- **RLIMIT_NOFILE raised at startup** from macOS's stock 256 to 4096 (or the
  hard cap, whichever is smaller) as belt-and-suspenders — a menu-bar that
  keeps fsnotify watchers open, opens state.json on every poll, does DNS,
  and talks to the Keychain has no business running under 256.
- **`usagio install`** no longer prints two near-identical warnings about
  a co-existing `~/.config/claude-usage/` and `~/.config/usagio/`.
- **`usagio install`** no longer prints `Unload failed: 5: Input/output error`
  when the legacy launchd plist is already unloaded (typical after reboot).

### Tap
- `Formula/usagio.rb`: dropped the `oldnames "claude-usage"` DSL call
  (removed in Homebrew 6.x); rename is now handled by `tap_migrations.json`
  at the tap root.

## [0.4.0] - 2026-09-06

Big-bang release. Rebrand from **claude-usage** to **usagio**. Refactored into
a core+agents architecture; adds OpenAI Codex support alongside Claude with 13
more provider slots stubbed and ready to fill.

### Renamed
- Project renamed from `claude-usage` to `usagio` — now tracks usage across
  Claude Code, Codex, and (via feature-gated stubs) every other AI-coding CLI.
- Binary, crate, config directory (`~/.config/claude-usage/` →
  `~/.config/usagio/`), launchd label
  (`com.claude-usage.menubar` → `com.mattjackson.usagio.menubar`), macOS Login
  Items entry, log filename, and bundle identifier all migrate on first run
  (atomic rename with cross-device copy+delete fallback; idempotent).
- macOS Keychain service string stays `"claude-usage"` for backwards
  compatibility — existing tokens are preserved without a re-login.

### Added
- **Multi-provider architecture.** New `Provider` trait + registry with 15
  slots (Claude + Codex live; 13 stubs: opencode, Gemini CLI, Qwen, Copilot
  CLI, Cursor Agent, Amazon Q, Cline, Grok, Kimi, and API-key providers
  OpenRouter, DeepSeek, Z.ai, Fireworks, Synthetic, Vertex AI). All gated
  behind default-on Cargo features.
- **Menu redesign.** Flat one-line-per-account row (`agent · email · S % ·
  W % · ✓`), right-arrow submenu for details, per-provider icons on section
  headers, countdown-when-locked (`locked · 23h 52m`).
- **Never-re-login credential sync.** fsnotify watchers on every provider's
  credential paths + absorb + provider-trait extension so vendor-CLI token
  rotations never strand our stored copy; last-chance fallback re-reads
  on-disk credentials before flagging any account as `needs_relogin`.
- **Analytics.** Usage log (`~/.config/usagio/history.YYYY-MM.ndjson`),
  weighted-linear-fit burn-rate forecast (`empty in ~40 m · 6 m before
  reset`), model→USD cost tracking, subscription verdict (`Cancel /
  Downgrade / Keep / Upgrade`).
- **Context Ledger.** Audit what each CLI auto-injects into its context per
  turn (`~/.claude/CLAUDE.md`, skills, MCP tool schemas, subagents, plugins);
  available as `usagio context` and via the menu.
- **Overwrite protection.** `save_state` refuses silent account drops
  (dumps rejected states to `/tmp/usagio-state-rejected-<ts>.json` for
  evidence), rolling backups in `~/.config/usagio/backups/`, `Settings ▸
  Backup Config` menu, and a test-isolation tripwire that panics if any
  unit test tries to touch the real `~/.config/usagio/`.
- **Platform trait.** `MenuBackend` / `SecretStore` / `Autostart` / `Paths`
  facade behind `Platform`; macOS wired (keychain via `security`, launchctl,
  `~/.config`), Linux + Windows stubs behind `cfg(target_os)` (real impls
  target v1.0).
- **Notifications.** Threshold (70 / 90 %), reset-back, and weekly-pace
  triggers via `notify-rust`; once-per-crossing dedup persisted in state.
- CLI additions: `usagio context [--provider SLUG] [--project PATH]`,
  `usagio report --pace|--pricing|--verdict`.
- Website at usagio.dev (Astro 5, dark mode, alternating usage-state
  layout, GitHub icon, 404 page); `THIRD_PARTY_NOTICES.md` with icon +
  dependency attribution.

### Changed
- `state.json` v1 → v2 (providers.<slug>.accounts, `secret_ref` pointers,
  `Vec<UsageWindow>` for deterministic ordering); v1 auto-migrates on first
  launch.
- `REFRESH_SKEW_SECS` 300 → 900 (refresh 15 min before expiry instead of 5).
- Menu: provider section only renders if that provider has ≥ 1 captured
  account; "Capture current login" submenu filters to providers with
  `supports_usage=true`; API-key providers grouped under `Paste API key ▸`.
- Accounts sort by soonest-expiration on add rather than add-order.
- `CLAUDE_CODE_OAUTH_TOKEN` env override surfaces a disabled row and
  skips the provider from auto-swap.
- `LAUNCHD_LABEL` → `AUTOSTART_LABEL` (OS-agnostic naming).
- Rust MSRV bumped to 1.88.

### Fixed
- **CRITICAL:** `switch_to_guarded` self-deadlock — `absorb_before_switch`
  re-acquired the state lock via `Provider::absorb_credential` and blocked
  forever (per-open-fd `flock` semantics). Made `with_state_lock`
  thread-local reentrant.
- **HIGH:** OAuth 400 `invalid_grant` was lumped into generic HTTP errors
  and swallowed; account carried a dead grant forever. Now typed as
  `RefreshError::InvalidGrant`; flags `Account.needs_relogin`; `cmd_token`
  and `refresh_usage_cache` both run `last_chance_fallback` before flagging.
- `spawn_watchers` skipped credential paths whose parent didn't exist at
  startup (fresh installs missed every fsnotify event). Now creates the
  parent (`0700`) before registering.
- `refresh_inactive_if_stale` snapshotted `active` outside the state lock
  (a mid-tick switch could refresh the just-became-active account). Now
  snapshots and re-checks under the lock.
- `store::config_dir` silently returned an empty/relative `PathBuf` when
  the platform Paths impl couldn't resolve `$HOME`. Now errors with
  actionable context.
- `choose_swap_target` filters out `needs_relogin` accounts so auto-swap
  can't hand a working login over to a dead account.

### Security
- **`save_state_safe`** refuses account drops without an explicit
  `Account::remove()` flag; rejected states dumped to
  `/tmp/usagio-state-rejected-<ts>.json` for post-mortem.
- **Rolling backups** — 20 timestamped copies of `state.json` in
  `~/.config/usagio/backups/`, one-command restore via `Settings ▸ Backup
  Config ▸ Restore from backup`.
- **Test isolation guard** — `store::config_dir()` panics if called from a
  `#[cfg(test)]` context without a `ScopedConfigDir` / `TestConfigDir`
  fixture active. Prevents the exact "test wipes prod state" incident that
  surfaced during this cycle.
- **Anthropic ToS disclaimer** in menu About + website footer + README:
  usagio uses OAuth tokens issued to your own accounts; provider ToS may
  restrict this use.

### Notes
- **Codex** support is functional but marked USAGE ONLY in v0.4.0;
  switching enabled in v0.5.0 after Codex proof-of-trait validation.
- **Linux / Windows** Platform trait implementations are skeleton stubs
  behind `cfg(target_os)` so cross-compile links; real impls target v1.0.
- **Keychain service string** stays `"claude-usage"` — renaming would
  strand every existing token. Documented at the constant site.

## [0.3.1] - 2026-09-05

### Changed
- **CI/release actions bumped off Node.js 20.** `actions/checkout` (→ v7.0.1),
  `actions/attest-build-provenance` (→ v4.2.2), and `softprops/action-gh-release`
  (→ v3.0.3) now run on Node.js 24, clearing GitHub's Node 20 deprecation warnings.
  All remain pinned to commit SHAs. No binary changes.

## [0.3.0] - 2026-09-05

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

[Unreleased]: https://github.com/MattJackson/claude-usage/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/MattJackson/claude-usage/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/MattJackson/claude-usage/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/MattJackson/claude-usage/compare/v0.2.0...v0.3.0
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
