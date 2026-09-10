# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] - 2026-09-10

### Changed
- **Windows/Linux tray menu now renders through [muri](https://crates.io/crates/muri)
  0.9.1's `muda-compat` facade** instead of muda/tray-icon. This is an internal
  backend swap only: the menu is feature- and pixel-identical to 0.5.23 — same
  layout, grouping, icons, and behavior. macOS is unchanged (its NSMenu styler
  still uses muda's `ns_menu()`). Surfaced and got two upstream muri fixes
  (`MenuId::new`, `compat::tray_icon::menu` re-export) landed in muri 0.9.1.

## [0.5.23] - 2026-09-10

### Fixed
- **Account switching never rotates the live CLI's refresh token.** usagio no
  longer POSTs `/token` for the *active* account on a switch or `token` command;
  it adopts the vendor CLI's current token instead. Single-use refresh tokens
  rotate server-side, so minting one for the account the real CLI is using would
  invalidate the CLI's own copy and force a re-login — breaking the "capture
  once, never re-login" guarantee.
- **A failed restore can no longer wipe your accounts.** "Restore…" moves the
  live `state.json` aside before writing the restored one; if that write failed,
  `state.json` was left missing and usagio read zero accounts on next load. The
  pre-restore state is now rolled back automatically on write failure (and if the
  rollback itself fails, the error names the backup file to recover from).
- **Corrupt/unreadable `state.json` no longer silently bypasses the
  overwrite-protection guard.** usagio now logs loudly when the guard (and rolling
  backup) is skipped, so an account-drop can't slip through unnoticed.
- **MCP context-ledger servers are fully cleaned up.** usagio now kills the
  server's whole process group, so wrapper commands (`npx`/`node`, `uvx`/`python`)
  don't leave orphaned child processes behind.

### Security
- **History files are created owner-only (0600).** Per-account usage history under
  `~/.config/usagio` no longer inherits a world-readable umask.

## [0.5.22] - 2026-09-09

### Changed
- **usagio's own icon now shows in the Windows/Linux tray** (previously it showed
  the active provider's icon, e.g. Claude's mark). The active account's provider
  icon now renders where it belongs — on the menu's group-header rows.
- **Provider icons on the Windows/Linux menu group headers.** The "Claude" /
  "Codex" group rows now carry the 16px provider icon (via muda `IconMenuItem`),
  matching what macOS already drew. macOS is unchanged.

## [0.5.21] - 2026-09-09

### Fixed
- **Windows tray tooltip now identifies usagio.** The tray icon's tooltip was
  the bare usage summary (e.g. "56% / 64%"), which doesn't tell you which app
  the icon belongs to. It now reads "usagio — 56% / 64%" (just "usagio" when
  there's no summary yet). macOS/Linux unaffected.

## [0.5.20] - 2026-09-09

### Fixed
- **Windows: no more console window / taskbar button.** usagio is a
  console-subsystem binary so its CLI subcommands can print to a terminal, but
  the `menubar` daemon was also handed a console window that surfaced as a
  taskbar button ("usagio - 1 running window") and a blank window. The menubar
  now hides its console at startup (`ShowWindow(GetConsoleWindow(), SW_HIDE)`),
  so on Windows usagio is a tray-only app — matching the macOS menu-bar and
  Linux tray experience. macOS/Linux unaffected.

## [0.5.19] - 2026-09-09

### Fixed
- **Windows: `usagio.exe` now launches.** The binary was dying at process load
  with `STATUS_ENTRYPOINT_NOT_FOUND` (0xC0000139) before `main` on every clean
  Windows box (and CI). Root cause: usagio imports ComCtl32 **v6**-only symbols
  (`SetWindowSubclass`, `RemoveWindowSubclass`, `DefSubclassProc`,
  `TaskDialogIndirect`, via `tray-icon`/`rfd`) but embedded no application
  manifest, so the loader bound the base `System32\comctl32.dll` (v5.82) which
  doesn't export them. A `build.rs` now embeds a manifest declaring a dependency
  on `Microsoft.Windows.Common-Controls` v6, so the loader activates ComCtl32 v6
  and the binary loads. macOS and Linux are unaffected. The CI Windows
  load-time smoke test (`usagio.exe --version`) is now blocking, so this can't
  regress.

## [0.5.18] - 2026-09-09

### Fixed
- **One account order everywhere.** The menu now uses a single, canonical
  account order on every surface and code path: providers alphabetical, then
  within a provider by soonest **weekly** reset first (so credits are used
  top-down and no weekly quota is left behind), with maxed-and-inactive
  accounts sunk to the bottom and headroom/email breaking ties. Previously the
  section list and the rendered order used two different sorts (one of them
  active-first), so the order could look inconsistent. The active account is no
  longer pinned to the top — it ranks on its own priority like every other
  account. (Session/5h reset is no longer part of the sort, so the list no
  longer reshuffles every few hours.)

### Changed (internal)
- Desktop notifications now route through a per-OS `Platform::notify` trait
  (native `notify-rust` on macOS/Linux, no-op on Windows) and `notify-rust` is
  excluded from the Windows build — it hard-linked a WinRT toast backend
  (`RoGetActivationFactory`) that isn't resolvable at load on clean Windows.
  macOS/Linux notifications are unchanged.

### Known issues
- **Windows: the binary still fails to launch** on Windows
  (`STATUS_ENTRYPOINT_NOT_FOUND` at process load). The WinRT toast link above is
  ruled out, as are `+crt-static` and `ProcessPrng`; root cause is still under
  investigation. macOS and Linux are unaffected and verified.

## [0.5.17] - 2026-09-09

Correctness + robustness round from a full-codebase audit (all findings
verified against the source; each fix design was refuted on two models before
landing).

### Fixed
- **Codex switch no longer forces a re-login on switch-back.** Switching Codex
  accounts now absorbs the outgoing account's on-disk token rotation *before*
  overwriting `~/.codex/auth.json` — the same guard the Claude path already had.
  Without it, a single-use refresh token the Codex CLI had rotated could be left
  stale and a later switch-back would write a dead token, dropping you into
  `codex login`. (The core "capture once, never re-login" guarantee, now upheld
  for Codex too.)
- **Auto-swap can't clobber a valid token via a race.** The inactive-account
  token-rotation mirror now runs under the same cross-process state lock a
  switch holds and re-checks identity inside it, so a concurrent `usagio switch`
  can never be interleaved to leave the keychain on one account and the identity
  on another.
- **Codex switch reports a half-applied switch honestly** instead of a bare
  "switch failed" when the vendor file was updated but recording it in
  state.json failed.
- **No needless swap off a healthy account.** `worth_returning_to` no longer
  treats an unknown weekly-reset as "infinitely far" and proactively swaps away
  from a working active account.
- **The menu-bar burn-rate row can't panic** on a non-positive "safe margin"
  value.
- **`state.json` writes are crash-durable** (the temp file and its directory are
  fsync'd around the atomic rename), so a crash right after a switch can't leave
  a truncated, unparseable state file.
- **The manual "Refresh usage now" action can't get stuck** if its worker thread
  panics (a drop-guard always clears the in-flight flag).

### Changed
- **Adaptive poll cadence no longer over-polls a maxed account with nowhere to
  go.** When the active account is at/above the swap trigger but there's no
  eligible account to switch to (a single-account user, or all others
  full/needs-relogin), the poll falls back to the base interval instead of the
  10-second backstop — the backstop is reserved for when a swap is actually
  actionable. Stops a stream of usage requests (and the HTTP 429s they cause)
  when there's nothing to catch.
- **The "Threshold alerts" menu label is derived from the actual threshold
  constants**, so it can never drift from the percentages that really fire.

## [0.5.16] - 2026-09-09

### Fixed
- **Hardened the keychain hang guard so it can't be defeated.** The native
  `security(1)` timeout now kills the whole process group on overrun (not just
  the direct child) and reaps cleanly on the error path, so a hung keychain call
  and any descendant can't wedge the poll thread. Re-enabled the real-keychain
  round-trip tests as genuine end-to-end coverage of the switch path.

## [0.5.15] - 2026-09-09

### Fixed
- **Keychain calls can no longer hang the app.** The `security(1)` hang guard
  wrapped every keychain call in `timeout(1)` — but macOS doesn't ship
  `timeout(1)` (it's GNU coreutils), so on a stock Mac the guard silently
  fell back to an *unguarded* call. Replaced with a native, dependency-free
  deadline (spawn + poll + kill) that behaves identically on every Mac, so a
  locked keychain after sleep can't stall account switching or auto-swap.

## [0.5.14] - 2026-09-09

### Fixed
- **Adaptive poll cadence is scoped to the active account.** An idle account
  already pinned at 100% used to hold the poll loop at its 10-second backstop
  indefinitely, hammering the usage endpoint into rate-limiting (HTTP 429) for
  no benefit — you never swap toward an account that's out of room. Cadence
  now tightens only as the account you're *actually using* approaches its
  limit. (Rolls up the 0.5.13 cadence change plus a formatting fix that had
  left CI red.)

## [0.5.12] - 2026-09-08

### Fixed
- **No more "Where is use_default?" dialog.** usagio registers its bundle id
  with the notification system, so the OS no longer tries to open a
  nonexistent app named "use_default" when a notification fires.

## [0.5.11] - 2026-09-08

### Changed
- **`usagio list` matches the menu order.** The CLI now sorts accounts by the
  same priority the menu bar uses (soonest weekly reset / most headroom first)
  instead of insertion order.

## [0.5.10] - 2026-09-08

### Added
- **Self-healing watchdog.** The poll loop watches its own file-descriptor use
  and keychain health, respawning the credential watcher (or restarting the
  app) before a resource leak or a wedged keychain can break switching.

## [0.5.9] - 2026-09-08

### Fixed
- **Fixed the file-descriptor leak that broke switching.** The credential
  watcher used a kqueue backend that held one open descriptor *per watched
  file* and descended `~/.claude` (whose `tasks/` and `plugins/` trees grow
  without bound), eventually exhausting the process's descriptors until every
  `security(1)` keychain call failed with EMFILE — silently breaking account
  switching, auto-swap, and refresh. Switched to the coalesced FSEvents
  backend (no per-file descriptor cost) and raised the descriptor ceiling.

## [0.5.8] - 2026-09-08

### Fixed
- **Codex `/login` is one-time again.** usagio no longer mints a token for the
  active Codex account — doing so rotated (and invalidated) Codex's single-use
  refresh token and forced a re-login. (Companion to the 0.5.5 Claude fix.)
- Stop printing `Unload failed: 5: Input/output error` during `usagio install`.

## [0.5.7] - 2026-09-08

### Changed
- **Account detail submenu redesigned for readability.** Dropped the emoji
  glyphs and the hard-to-read grey text in the per-account detail rows.

## [0.5.6] - 2026-09-08

### Fixed
- **Linux build runs on Ubuntu 22.04 LTS.** The release binary is now built on
  ubuntu-22.04 (glibc 2.35) rather than a newer image, so it runs on 22.04
  without a glibc-version error.

## [0.5.5] - 2026-09-08

### Fixed
- **Claude `/login` is one-time again.** usagio no longer mints a token for the
  active Claude account — doing so rotated (and invalidated) Claude Code's
  single-use refresh token, forcing recurring re-logins. Capturing an account
  once now keeps its login valid.

## [0.5.4] - 2026-09-08

### Fixed
- Defect batch across the menu bar, autostart, and release packaging.

## [0.5.3] - 2026-09-08

### Added
- **Compact menu rows.** Accounts render as compact rows grouped by provider,
  each with a per-account detail submenu.

### Fixed
- **macOS autostart actually starts.** The LaunchAgent is bootstrapped
  immediately after its plist is written, so the menu-bar app comes up on
  install instead of waiting for the next reboot.

## [0.5.2] - 2026-09-08

Robustness + hardening round: background-thread panic supervision, HTTP
timeouts on every provider call, several concurrency/state-merge fixes, and
a menu-UX pass grouping accounts into HR-separated blocks.

### Added
- **`poll_loop` panic supervisor.** The menu-bar background polling thread
  is now respawned automatically if it panics, instead of silently dying
  and leaving the app un-updating for the rest of the session.
- **fsnotify credential-watcher panic supervisor.** The credential-file
  watcher thread gets the same panic-catch-and-respawn treatment.

### Changed
- **HTTP read+write timeouts on all provider calls.** `ureq` requests for
  Claude usage, Claude OAuth, and Codex OAuth now carry explicit
  read/write timeouts, so a stalled proxy or unresponsive endpoint can no
  longer hang the poll thread indefinitely.
- **Shared `SwapGuard` for manual refresh.** `poll_loop` and
  `handle_refresh_now` now share the same `SwapGuard`, so a manual
  "Refresh usage now" click honors the same anti-thrash cooldown/no-return
  windows as the background poller.
- **Menu UX: account blocks separated by HR.** Accounts now render as
  visually distinct blocks separated by a horizontal rule, replacing the
  previous per-account single-line rows grouped by provider.
- **`usagio install` purges legacy System Events login items.** Previously
  only `uninstall` did this; brew-upgrade users could end up with a
  duplicate stale autostart entry.
- **`LinuxAutostart::restart()` quote-aware path splitting.** Paths
  containing spaces (e.g. `~/My Apps/usagio`) are now handled correctly.
- **`~/.config/usagio/` hardened to 0700.** Previously created at the
  process's default umask.
- **`usagio.log` opened at mode 0600.** Previously created at 0644 under
  default umask.

### Fixed
- **`refresh_usage_cache` merge no longer clobbers `needs_relogin`.** A
  concurrently-set `needs_relogin=true` flag could be overwritten back to
  `false` by an in-flight merge; the merge now preserves it.
- **`context_ledger::mcp::read_line_with_timeout` enforces a real
  wall-clock timeout**, regardless of whether the child process ever
  writes anything.

### Removed
- Dead `Provider::list_accounts` default implementation deleted.
- `main.rs::capture_current` deleted — Claude now shares
  `capture_current_generic` with every other provider. Stale doc-comment
  references to the removed path were cleaned up alongside it.

### Performance
- Four hot-path efficiency fixes from the post-0.5.1 audit: dropped a
  redundant `state.json` read+parse on every poll cycle and every
  credential fsnotify event (the computed `active_email_hint` was unused
  by the callee); skipped the `refresh_usage_cache` merge+save entirely
  when no account actually changed that cycle, avoiding a needless
  read-diff-backup-rotate-write; de-duplicated the per-account history-
  window filtering shared by `burn_rate` and `cost_tracking` in the menu
  rebuild; and throttled `cached_state()`'s `metadata()` stat call on the
  macOS main-thread 0.75s timer tick, matching the existing throttle
  already applied to `maybe_relaunch_after_upgrade`'s `canonicalize()`
  call.

## [0.5.1] - 2026-09-08

Audit-fix + UX round following the v0.5.0 release: a menu-bar readability
pass and a batch of robustness/concurrency/efficiency fixes from the
post-release codeaudit.

### Changed
- **Per-provider menu grouping.** The main menu now groups accounts by
  provider with an `NSMenuItem.separatorItem` between sections, instead of
  one flat undifferentiated list.
- **Version row right-aligned; dropped the `S`/`W` prefix.** The Quit-row
  version string is right-aligned via a tab-stop paragraph style, and no
  longer prefixes itself with the redundant `S`/`W` status letters.

### Fixed
- **`use_default` install-dialog fix.** The install flow's default-app
  prompt now routes through `lsregister` instead of the path that could
  leave macOS pointing at a stale/duplicate `usagio.app` registration.
- **concurrency-01 [lock].** Closed a lock-ordering/hold gap in the state
  lock usage the audit flagged as a potential deadlock/race window.
- **robustness-01 [HIGH]: `security(1)` calls could hang forever.**
  `real_get`/`real_set`/`real_delete` in `src/platform/macos.rs` now wrap
  every `security` invocation in `timeout(1) 5` — a locked keychain or an
  unattended SecurityAgent dialog previously hung the poll thread
  indefinitely; it now surfaces as a `keychain_read_timeout` /
  `keychain_write_timeout` / `keychain_delete_timeout` log event and a
  regular (non-fatal) `Err` to the caller. Falls back to an unguarded
  `security` call, logged once, on a machine with no `timeout` binary on
  `PATH`.
- **robustness-02 through robustness-05.** Further hardening fixes from the
  same audit pass (see individual commits for detail).
- **efficiency-01.** Closed a wasted-work finding from the audit pass.
- **concurrency-03 [narrowed].** Tightened the scope of a previously
  over-broad lock/critical section identified in round 3 of the audit.

## [0.5.0] - 2026-09-07

Cross-platform release. Linux and Windows get real `Platform` trait
implementations (menu bar, secure secret storage, autostart, paths) instead
of `cfg(target_os)` stubs, Codex gains full account switching, and the menu
gets a multi-provider-ready flat redesign.

### Added
- **Linux platform support.** Real `Platform` trait impl backed by
  `tray-icon`/`libayatana-appindicator3` (tray), the D-Bus Secret Service
  (`keyring`, with a permission-protected `~/.config/usagio/secrets.json`
  fallback when no Secret Service daemon is reachable), XDG autostart
  (`~/.config/autostart/*.desktop`), and XDG base-dir paths. See
  `packaging/linux/DEPENDENCIES.md` for build/runtime deps.
- **Windows platform support.** Real `Platform` trait impl backed by Win32
  tray APIs, Windows Credential Manager, a `HKCU\...\Run` autostart entry,
  and `%APPDATA%` paths. See `packaging/windows/DEPENDENCIES.md`.
- **Codex account switching.** Codex moves from usage-reporting-only to full
  `switch`/`start`/`continue` support via `auth.json` rewrite, matching
  Claude's account-switching model.
- **Menu-bar redesign.** Flat one-line-per-account main menu (scales to all
  15 provider slots instead of one big status block per provider), a
  Settings submenu tree, and native OS file dialogs (`rfd`) for Backups
  Save/Restore instead of a Terminal-only flow.
- **macOS `.app` bundle.** The universal tarball now ships `usagio.app`
  alongside the bare binary, giving Login Items a real icon instead of the
  generic "exec" glyph; ad-hoc signed (not yet notarised — no paid Apple
  Developer account).
- **Adaptive poll cadence.** `watch`'s background poll tightens its interval
  as an account approaches its swap trigger, rather than a fixed cadence
  throughout.
- **`rc-release.yml`.** Push a `vX.Y.Z-rcN` tag to build and publish the same
  three artifacts as a real release (as a GitHub prerelease) without
  touching the Homebrew tap, so the full build/package/attest/publish
  pipeline can be validated end-to-end before cutting the real tag.
- **CI matrix.** `ci.yml`'s `check` job now runs on macOS, Linux, and
  Windows (previously macOS-only), with a strict-cfg lint enforcing that all
  OS-specific code lives behind the `Platform` trait rather than scattered
  `cfg(target_os)`.

### Fixed
- A full pre-release code audit (H1-H4, M1-M14) closed: silent
  `notification_config` parse failures now logged instead of silently
  resetting; an aborted config restore no longer defaults old-count to 0;
  `toggle_autoswap`'s read-modify-write is now atomic under the state lock;
  a Codex TOCTOU on `auth.json` creation closed (0600, atomic); the config
  restore flow now goes through `save_state_safe` and stashes to
  `backups/`; rapid "Refresh usage now" clicks are deduped with an
  in-flight guard; `request_quit()` called before the event loop starts is
  now honored instead of dropped; `write_auth_json_atomically`'s error
  cases are differentiated; a same-thread runtime check guards `TrayState`;
  double-clicking the `.app` bundle now opens the menu bar instead of
  running the CLI's default `list` command; the menu no longer shows a
  stray `locked · ` prefix on a locked account's countdown row.
- **`release.yml` (H4):** `librsvg` is now installed before
  `generate-icns.sh` runs, so the very first tagged release doesn't fail
  mid-job (after the build already succeeded) for want of `rsvg-convert`.
- **`release.yml` (M13, M14):** only one of the three release jobs
  (`build-macos`) generates GitHub's auto release notes — the other two
  upload artifacts additively instead of racing to write the release body;
  the Homebrew tap-bump's perl/sed regex surgery is validated with `ruby -c`
  plus a structural check on the `install` block before pushing.

### Internal
- Real integration coverage closes the "compiles but was never actually
  exercised" gap on OS-integration surfaces: a live GTK/appindicator tray
  init under `xvfb-run`, a real Secret Service D-Bus round-trip via an
  unlocked `gnome-keyring-daemon`, and the real-registry-write Windows
  autostart test all now run on every push instead of being `#[ignore]`d or
  verified only by `cargo build` linking successfully.
- Backups Save/Restore file dialogs now route through a
  `Platform::file_dialog()` trait method with a capturing mock for the
  click-handler logic, since the Linux `xdg-desktop-portal` backend isn't
  practical to exercise headlessly in CI.

## [0.4.3] - 2026-09-07

Silences a recurring macOS Automation permission prompt and restores the
disabled/grey version row.

### Removed
- **"Launch at login" menu checkbox** (the one backed by
  `osascript → tell application "System Events" → make login item …`).
  Every `brew upgrade` changed the binary hash → macOS treated the new
  binary as a different app → re-prompted "usagio wants access to
  control System Events" on the next poll. Path was already redundant
  with `usagio install`, which registers a proper launchd
  `LaunchAgent` plist. **`usagio install` / `usagio uninstall`** are
  now the sole autostart entry points. To clean out the stale System
  Events entry a prior version installed, remove **usagio** from
  System Settings → General → Login Items → Open at Login.

### Changed
- **Version row is back to its own grey (disabled) row above Quit.**
  0.4.2 folded it into the Quit label to save a row; users noted the
  grey look and preferred right-alignment. NSMenu doesn't right-align
  a single label on a sibling item's row without dropping to
  `NSMenuItem.attributedTitle` + a right tab-stop paragraph style via
  objc, which is planned for 0.5.0's broader menu redesign.

## [0.4.2] - 2026-09-07

Menu declutter — removes a broken row and consolidates the footer.

### Removed
- **Context Ledger submenu.** Clicks shelled out to a new Terminal window via
  osascript, which silently failed for anyone who hadn't granted macOS
  Automation permission — a broken menu row is worse than no menu row. The
  CLI (`usagio context [--provider slug]`) is unchanged and remains the
  supported entry point. Related dead code + tests removed.

### Changed
- **Quit row now shows the version inline** (`Quit  ·  usagio v0.4.2`), so
  the version no longer eats its own disabled row above Quit. macOS NSMenu
  doesn't support arbitrary right-alignment for a submenu item, so it reads
  left-to-right rather than hard-right-aligned.

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

[Unreleased]: https://github.com/MattJackson/usagio/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/MattJackson/usagio/compare/v0.4.3...v0.5.0
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
