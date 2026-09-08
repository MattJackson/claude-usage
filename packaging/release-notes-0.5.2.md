## usagio v0.5.2 — install-UX round

A focused release: "install means install" everywhere. No schema or
CLI-flag changes — safe to `brew upgrade` / re-run the Windows installer in
place.

### Highlights

- **Homebrew formula auto-runs `usagio install`.** `brew install
  mattjackson/tap/usagio` and `brew upgrade usagio` now register the
  LaunchAgent and start the menu bar automatically via a `post_install`
  hook — no more separate manual `usagio install` step after a fresh
  install. Run `usagio uninstall` to stop the menu bar and remove
  autostart.
- **Windows NSIS installer.** Windows now ships a proper
  `usagio-v0.5.2-x86_64-pc-windows-msvc-setup.exe` installer alongside the
  existing `.zip`: installs to `%ProgramFiles%\usagio\`, adds it to the
  system `Path`, creates a Start Menu shortcut, registers an Add/Remove
  Programs entry with a standard uninstaller, and runs `usagio install` at
  the end. Supports silent/unattended install: `usagio-setup.exe /S`. The
  `.zip` distributable is still published for power users who prefer a
  portable unzip-and-run install.

### Packages

- **macOS:** `usagio-v0.5.2-universal-apple-darwin.tar.gz` (or
  `brew upgrade usagio` / `brew install mattjackson/tap/usagio`)
- **Linux (x86_64):** `usagio-v0.5.2-x86_64-unknown-linux-gnu.tar.gz`
- **Windows (x86_64):** `usagio-v0.5.2-x86_64-pc-windows-msvc-setup.exe`
  (installer, new) or `usagio-v0.5.2-x86_64-pc-windows-msvc.zip` (portable)

Each artifact ships a matching `.sha256` checksum, and macOS additionally
ships a combined `SHA256SUMS` manifest. All three platforms' primary
artifacts are built with
[GitHub attestations](https://github.com/MattJackson/usagio/attestations) —
verify with:

```sh
gh attestation verify <file> --repo MattJackson/usagio
```

### Known limitations

Same as v0.5.1: Windows and Linux builds are unsigned (no Snap/AppImage/AUR
or MSI/scoop packaging yet — the new NSIS installer is unsigned too), and
macOS `usagio.app` is ad-hoc signed, not notarised.

See `CHANGELOG.md` for the complete, unabridged list of changes since
v0.5.1.
