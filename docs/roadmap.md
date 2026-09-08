# Roadmap & branch model

## Branches

- **`release/0.4.x`** — maintenance line, pinned at v0.4.1. Hotfixes only.
  Every bug found in prod lands here first, gets a `0.4.x` tag, brew ships
  it. Then the same commit is merged into `dev`.
- **`main`** — the currently-released version. Fast-forwards to whatever
  the newest tagged release is (currently v0.4.1 == release/0.4.x tip).
- **`dev`** — v0.5.0 line. Features + refactors. Bugs cherry-picked from
  release/0.4.x on merge.

## v0.5.0 scope (feature line) — done, shipped 2026-09-07

## v0.5.1 (audit-fix + UX round) — done

Follow-up pass after the v0.5.0 release audit: per-provider menu grouping,
right-aligned version row (dropped the S/W status prefix), the
`use_default` install-dialog fix routed through `lsregister`, a
concurrency-01 lock fix, robustness-01 through 05, efficiency-01, and a
concurrency-03 narrowing. See `CHANGELOG.md`'s `[0.5.1]` section for the
full list.

## v0.5.2 (robustness + hardening round) — done

Follow-up pass after the v0.5.1 audit: `poll_loop` and the fsnotify
credential-watcher thread both get panic supervisors (respawn instead of
silently dying), `ureq` calls for every provider (Claude usage, Claude
OAuth, Codex OAuth) get read+write timeouts, `refresh_usage_cache`'s merge
no longer clobbers a concurrently-set `needs_relogin`, `poll_loop` and
`handle_refresh_now` share one `SwapGuard`, `~/.config/usagio/` and
`usagio.log` are hardened to 0700/0600, `usagio install` purges legacy
System Events login items, `LinuxAutostart::restart()` handles paths with
spaces, `context_ledger::mcp::read_line_with_timeout` enforces a real
wall-clock timeout, the menu now renders accounts as HR-separated blocks,
and four hot-path efficiency fixes landed. Dead `Provider::list_accounts`
and `main.rs::capture_current` were deleted. See `CHANGELOG.md`'s
`[0.5.2]` section for the full list.

### Menu-bar redesign (multi-provider prep)

Now that Codex is coming and 13 more provider slots exist, the current
top-of-menu status header ("Session resets in X, Weekly resets in X,
Estimate 1h 27m") doesn't scale — one such block per provider would fill
the screen. Consolidate:

- **Remove** the two big status rows at the top of the menu.
- **Main menu** = flat "Claude — account | account | ..." per provider,
  one line per account. Provider label on the left, account list next
  to it, active account marked (▶ or bold).
- **Weekly summary** moves to a submenu ("Weekly overview").
- **Side menu** = white text, non-clickable (currently grey/disabled and
  hard to read).
- **Side menu** = **no percentages** (already shown in main), just
  labels/timings/context.

### API-key providers

- **Remove "Paste API Key"** action entry from the "Capture current login"
  submenu. Instead, just **list the API-key providers below the OAuth
  ones** in the main menu, same as any captured account.

### Codex switching

Currently reporting-only for Codex. In v0.5.0 the switch/start/continue
verbs work for Codex too.

### Platforms

Linux + Windows platform impls (drafts staged at /tmp/usagio-drafts/).

## v0.5.0 quality bar

Same as v0.4.0: `/codeaudit` converges to 0 confirmed issues, all lenses
retired, 357/357 tests green under `--test-threads=1` × 5 runs.

CI closes the "compile-only" gap on every OS-integration surface before the
v0.5.0 tag: a real GTK/appindicator tray init under `xvfb-run`, a real
Secret Service D-Bus round-trip via an unlocked `gnome-keyring-daemon`, and
the real-registry-write autostart test — all previously `#[ignore]`d or
verified only by `cargo build` linking successfully — now run on every push
(see `.github/workflows/ci.yml`). The one surface still not exercised
end-to-end in CI is the Linux `xdg-desktop-portal` file-dialog backend
(installing a headless portal implementation is fragile and adds real time
to every PR for a single dialog); that path is instead covered by routing
`rfd::FileDialog` through a `Platform::file_dialog()` trait method
(`src/platform/mod.rs`) with a capturing mock, so the click-handler logic
around it is unit-tested even though the native portal call itself isn't.

### Pre-release step: `vX.Y.Z-rcN` tags

Before cutting the real `vX.Y.Z` tag, push a `vX.Y.Z-rcN` tag (e.g.
`v0.5.0-rc1`). `.github/workflows/rc-release.yml` builds and publishes the
same three artifacts `release.yml` does (macOS universal, Linux x86_64
tarball, Windows x86_64 zip) as a GitHub prerelease, but does **not** touch
the Homebrew tap formula — that only ever tracks the latest stable
`vX.Y.Z` release. This is the only realistic way to validate the
release/tap-bump machinery end to end (build matrix, packaging, codesign,
attestation) without risking `brew upgrade usagio` resolving to an RC:
download the macOS tarball from the RC release,
`brew install --formula ./usagio-v0.5.0-rc1-universal-apple-darwin.tar.gz`,
boot the tray, and eyeball it.

## Hotfix candidates (→ release/0.4.x → 0.4.2)

Filed by user during 0.4.1 soak — evaluating each for 0.4.x vs. 0.5.0:

- **Refuse `usagio switch <locked-account>`.** UX bug: switching to an
  account that's at 100% capacity silently succeeds and then Claude
  Code errors on first request. Should refuse with a helpful message.
  → **0.4.x hotfix.**
- **Side menu contrast (grey → white).** Currently uses NSMenu default
  disabled colour which is illegibly grey on modern macOS. Ship on
  0.4.x as a readability fix; the more ambitious "no percentages"
  restructure ships with 0.5.0's redesign.
  → **0.4.x hotfix (colour only).**

Everything else on this page is 0.5.0 material.
