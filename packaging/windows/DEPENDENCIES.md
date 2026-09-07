# Windows build & runtime dependencies

## Build

- Rust toolchain with the `x86_64-pc-windows-msvc` target:
  ```
  rustup target add x86_64-pc-windows-msvc
  ```
- Windows SDK — comes with the **Visual Studio Build Tools** ("Desktop
  development with C++" workload), which also supplies the MSVC linker
  (`link.exe`) `rustc` needs for this target. `ring` (pulled in transitively
  by `ureq`'s rustls TLS backend) compiles a small amount of C and needs the
  Windows SDK headers (`assert.h` etc.) on the include path — this is
  provided automatically by a VS Build Tools install / a `vcvarsall.bat` /
  Developer Command Prompt environment. Building `usagio` from a plain
  `cmd.exe` without one of those will fail with `ring`'s C compile step
  unable to find `assert.h`.
- CI builds the Windows binary on a real `windows-latest` runner. This repo's
  non-Windows contributors cannot fully cross-compile-check the Windows
  target locally for the same reason (no MSVC/Windows SDK headers on
  macOS/Linux) — `cargo check --target x86_64-pc-windows-msvc` on the crate
  as a whole fails at `ring`'s C compile step, unrelated to
  `src/platform/windows.rs` itself. The Windows-specific platform code was
  validated by extracting it into a standalone scratch crate pinned to the
  same dependency versions and running `cargo check --target
  x86_64-pc-windows-msvc` there (no `ring`/`ureq` in the dependency graph) —
  that path compiles clean, including the `#[cfg(test)]` module. The real
  Windows build (whole binary, including `ureq`/`ring`) is verified by CI.

## Runtime

None — no separate runtime install is required.

- **Tray icon**: `tray-icon` + `muda` use plain Win32 APIs
  (`Shell_NotifyIconW`, message-only windows); no extra runtime component.
- **Secret storage**: `keyring` (via `windows-native-keyring-store`) talks to
  Windows Credential Manager (`CredWriteW`/`CredReadW`/`CredDeleteW`), which
  ships with the OS.
- **Autostart**: `winreg` writes a value under
  `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` — no
  driver, service, or scheduled task involved.
- **File dialogs**: `rfd` uses the Win32 COM `IFileOpenDialog` /
  `IFileSaveDialog` APIs, shipped with the OS.
- **Notifications**: `notify-rust` depends on `tauri-winrt-notification` on
  Windows unconditionally (it's a `[target.'cfg(target_os="windows")']`
  dependency in `notify-rust`'s own `Cargo.toml`, not behind a Cargo
  feature) — no extra feature flag needed on our side, and no extra runtime
  DLL beyond what Windows itself ships (WinRT / Action Center).
- **Terminal launcher** (not yet wired to any caller — see
  `src/platform/windows.rs`): probes for `wt.exe` (Windows Terminal) first,
  falling back to the always-present `cmd.exe`, then `powershell.exe`. All
  three are either optional-but-common (`wt.exe`, pre-installed on Windows
  11 and available via the Microsoft Store on Windows 10) or ship with every
  supported Windows version.

Everything else statically links into the single `usagio.exe`.

## Known first-run friction: Windows Defender

An unsigned, freshly-built/downloaded `usagio.exe` (no EV code-signing
certificate yet) can get flagged by Windows Defender / SmartScreen on first
run — either quarantined outright or shown an "Windows protected your PC"
prompt. Until the release pipeline has a signing certificate, the documented
workaround is to exclude the install path from Defender's real-time
scanning:

```powershell
Add-MpPreference -ExclusionPath "C:\Path\To\usagio.exe"
# or, for a whole install directory:
Add-MpPreference -ExclusionPath "C:\Program Files\usagio"
```

This requires an elevated (Administrator) PowerShell prompt. Revisit this
note once release binaries are code-signed — the exclusion workaround should
no longer be necessary at that point.
