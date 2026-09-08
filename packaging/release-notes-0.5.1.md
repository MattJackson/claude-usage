## usagio v0.5.1 — audit-fix + UX round

A follow-up release to v0.5.0: a menu-bar readability pass plus a batch of
robustness, concurrency, and efficiency fixes from the post-release code
audit. No schema or CLI-flag changes — safe to `brew upgrade` in place.

### Highlights

- **Per-provider menu grouping.** The main menu now groups accounts by
  provider, with a separator line between each provider's section, instead
  of one flat list.
- **Version row right-aligned; dropped the `S`/`W` prefix.** The Quit-row
  version string is now right-aligned and no longer carries the redundant
  `S`/`W` status-letter prefix.
- **`security(1)` hang guard (robustness-01, HIGH).** Keychain reads/writes/
  deletes on macOS are now bounded by a 5-second `timeout(1)` wrapper — a
  locked keychain or an unattended "Allow/Deny" SecurityAgent dialog can no
  longer hang usagio's background poll thread forever. Falls back to an
  unguarded call (logged once) if no `timeout` binary is on `PATH`.
- **`use_default` install-dialog fix.** The install flow's default-app
  prompt now routes through `lsregister`, fixing a stale/duplicate
  `usagio.app` registration some users hit.

### Fixed

Further findings from the post-v0.5.0 audit: a state-lock ordering/hold gap
(concurrency-01), a batch of additional robustness hardening
(robustness-02 through 05), a wasted-work fix (efficiency-01), and a
narrowed lock scope from a follow-up audit pass (concurrency-03). Full list
in `CHANGELOG.md`.

### Packages

- **macOS:** `usagio-v0.5.1-universal-apple-darwin.tar.gz` (or
  `brew upgrade usagio` / `brew install mattjackson/tap/usagio`)
- **Linux (x86_64):** `usagio-v0.5.1-x86_64-unknown-linux-gnu.tar.gz`
- **Windows (x86_64):** `usagio-v0.5.1-x86_64-pc-windows-msvc.zip`

Each artifact ships a matching `.sha256` checksum, and macOS additionally
ships a combined `SHA256SUMS` manifest. All three are built with
[GitHub attestations](https://github.com/MattJackson/usagio/attestations) —
verify with:

```sh
gh attestation verify <file> --repo MattJackson/usagio
```

### Known limitations

Same as v0.5.0: Windows and Linux builds are unsigned (no Snap/AppImage/AUR
or MSI/scoop packaging yet), and macOS `usagio.app` is ad-hoc signed, not
notarised.

See `CHANGELOG.md` for the complete, unabridged list of changes since
v0.5.0.
