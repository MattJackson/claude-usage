## usagio v0.5.0 — Linux, Windows, and full Codex switching

usagio now runs its menu bar / tray, secure secret storage, and autostart on
**Linux** and **Windows**, not just macOS — the `Platform` trait behind
usagio finally has real implementations on all three OSes instead of
cross-compile stubs. **Codex** also graduates from usage-reporting-only to
full account switching, and the menu bar gets a flat, multi-provider-ready
redesign.

### Highlights

- **Linux support** — tray via `libayatana-appindicator3`, secrets via the
  D-Bus Secret Service (falls back to a permission-protected local file if
  no Secret Service daemon is running), XDG autostart, XDG base-dir paths.
  See `packaging/linux/DEPENDENCIES.md` for the build/runtime package list.
- **Windows support** — native tray icon, Credential Manager for secrets, a
  `HKCU\...\Run` autostart entry, `%APPDATA%` paths. See
  `packaging/windows/DEPENDENCIES.md` (includes a note on the expected
  first-run Defender/SmartScreen prompt on this unsigned build).
- **Codex account switching.** `switch` / `start` / `continue` now work for
  Codex accounts, not just Claude.
- **Menu-bar redesign.** A flat, one-line-per-account main menu that scales
  to every provider slot, a Settings submenu, and native OS file dialogs for
  config backup/restore.
- **macOS `.app` bundle.** The universal tarball now includes `usagio.app`
  so Login Items shows a real icon. Ad-hoc signed only — not notarised yet,
  so a first manual double-click may show a Gatekeeper warning; installing
  via `usagio install` or `brew install` does not go through that path.
- **Adaptive auto-swap polling.** `watch`'s poll interval tightens as an
  account nears its swap trigger instead of polling on a fixed cadence.

### Fixed

A full pre-release audit closed out a further round of findings, including:
a Codex `auth.json` TOCTOU on account capture, a non-atomic `toggle_autoswap`
read-modify-write, silent `notification_config` parse failures, a config
restore path that could default a stale backup count to `0` instead of
aborting, deduping rapid "Refresh usage now" clicks, honoring `request_quit`
requests made before the event loop starts, and a stray `locked · ` prefix
on a locked account's countdown row. Full list in `CHANGELOG.md`.

### Packages

- **macOS:** `usagio-v0.5.0-universal-apple-darwin.tar.gz` (or
  `brew upgrade usagio` / `brew install mattjackson/tap/usagio`)
- **Linux (x86_64):** `usagio-v0.5.0-x86_64-unknown-linux-gnu.tar.gz`
- **Windows (x86_64):** `usagio-v0.5.0-x86_64-pc-windows-msvc.zip`

Each artifact ships a matching `.sha256` checksum, and macOS additionally
ships a combined `SHA256SUMS` manifest. All three are built with
[GitHub attestations](https://github.com/MattJackson/usagio/attestations) —
verify with:

```sh
gh attestation verify <file> --repo MattJackson/usagio
```

### Known limitations

- Windows and Linux builds are unsigned; Linux has no Snap/AppImage/AUR
  package yet and Windows no MSI/scoop bucket (both planned post-v0.5.0).
  The Homebrew tap only tracks the macOS artifact.
- macOS `usagio.app` is ad-hoc signed, not notarised.

See `CHANGELOG.md` for the complete, unabridged list of changes since
v0.4.3.
