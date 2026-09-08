## usagio v0.5.2 — robustness + hardening round

A follow-up release to v0.5.1: background threads now supervise themselves
and respawn on panic instead of silently dying, every provider HTTP call
gets a hang-proof timeout, and a batch of concurrency/state and menu-UX
fixes from the latest audit pass. No schema or CLI-flag changes — safe to
`brew upgrade` in place.

### Highlights

- **Panic supervisors for background threads.** Both `poll_loop` (the
  menu-bar's background poller) and the fsnotify credential-watcher thread
  now catch panics and respawn automatically, instead of leaving usagio
  silently frozen for the rest of the session.
- **HTTP timeouts on every provider call (HIGH).** `ureq` requests for
  Claude usage, Claude OAuth, and Codex OAuth now carry explicit
  read/write timeouts — a stalled proxy or unresponsive endpoint can no
  longer hang the poll thread forever.
- **Shared anti-thrash guard for manual refresh.** "Refresh usage now"
  from the menu now shares its `SwapGuard` with the background poller, so
  manual clicks honor the same cooldown/no-return windows.
- **Menu UX: account blocks separated by HR.** Accounts render as
  visually distinct blocks with a horizontal rule between them, replacing
  the previous per-account single-line rows.
- **`needs_relogin` no longer clobbered.** Fixed a merge race where a
  concurrently-set `needs_relogin=true` could be overwritten back to
  `false`.

### Fixed

Further hardening: `usagio install` now purges legacy System Events login
items (previously only `uninstall` did); `LinuxAutostart::restart()`
handles paths with spaces; `~/.config/usagio/` is created at mode 0700 and
`usagio.log` at 0600 (previously default umask); and
`context_ledger::mcp::read_line_with_timeout` now enforces a real
wall-clock timeout even if the child process never writes. Dead
`Provider::list_accounts` and `main.rs::capture_current` were deleted as
part of the cleanup. Full list in `CHANGELOG.md`.

### Performance

Four efficiency fixes from the post-0.5.1 audit: eliminated a redundant
`state.json` read+parse on every poll cycle and credential-change event, a
needless state save cycle on idle/degraded poll cycles, redundant
per-account history-window rescans in the menu-rebuild path, and an
unthrottled disk `stat()` on the macOS main thread's 0.75s timer tick.

### Packages

- **macOS:** `usagio-v0.5.2-universal-apple-darwin.tar.gz` (or
  `brew upgrade usagio` / `brew install mattjackson/tap/usagio`)
- **Linux (x86_64):** `usagio-v0.5.2-x86_64-unknown-linux-gnu.tar.gz`
- **Windows (x86_64):** `usagio-v0.5.2-x86_64-pc-windows-msvc.zip`

Each artifact ships a matching `.sha256` checksum, and macOS additionally
ships a combined `SHA256SUMS` manifest. All three are built with
[GitHub attestations](https://github.com/MattJackson/usagio/attestations) —
verify with:

```sh
gh attestation verify <file> --repo MattJackson/usagio
```

### Known limitations

Same as v0.5.1: Windows and Linux builds are unsigned (no Snap/AppImage/AUR
or MSI/scoop packaging yet), and macOS `usagio.app` is ad-hoc signed, not
notarised.

See `CHANGELOG.md` for the complete, unabridged list of changes since
v0.5.1.
