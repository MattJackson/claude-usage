# Linux system dependencies

usagio's Linux platform impl (`src/platform/linux.rs`) uses the `tray-icon`
crate for the tray/status-bar item. On Linux that crate is backed by
`libappindicator`/`libayatana-appindicator3`, which is itself a GTK
status-icon wrapper — so both a C toolchain's GTK3 development headers (to
*build* usagio) and the GTK3 + appindicator runtime libraries (to *run* it)
are required. Secret storage, autostart, and file dialogs only need what's
already present on any desktop Linux install (D-Bus, `xdg-desktop-portal`).

## Build-time dependencies

Needed to compile usagio from source (`cargo build` / `cargo install`).

### Ubuntu / Debian

```sh
sudo apt install libgtk-3-dev libayatana-appindicator3-dev libxdo-dev libssl-dev pkg-config
```

### Fedora

```sh
sudo dnf install gtk3-devel libappindicator-gtk3-devel libxdo-devel openssl-devel pkgconf-pkg-config
```

### Arch Linux / Manjaro

```sh
sudo pacman -S gtk3 libappindicator-gtk3 xdotool openssl pkgconf
```

(`libayatana-appindicator3` is preferred where available; Arch's
`libappindicator-gtk3` package provides the same `AppIndicator3` API the
`tray-icon` crate links against. Substitute your distro's Ayatana package if
it splits the two.)

## Runtime dependencies

Needed to *run* the built `usagio` binary (menubar / tray mode).

### Ubuntu / Debian

```sh
sudo apt install libgtk-3-0 libayatana-appindicator3-1 libsecret-1-0 xdg-desktop-portal
```

### Fedora

```sh
sudo dnf install gtk3 libayatana-appindicator3 libsecret xdg-desktop-portal
```

### Arch Linux / Manjaro

```sh
sudo pacman -S gtk3 libayatana-appindicator libsecret xdg-desktop-portal
```

## Notes on optional pieces

- **Secret storage** (`keyring` crate, Secret Service backend) talks to
  whatever D-Bus Secret Service provider is running — GNOME Keyring, KWallet
  (via its Secret Service compatibility layer), or `keepassxc`'s Secret
  Service integration all work. `libsecret-1-0` above is the common runtime
  most of those depend on; nothing usagio itself links against directly (the
  D-Bus round-trip is pure Rust via `zbus`, no `libdbus` link-time
  dependency). **If no Secret Service daemon is reachable at all** (headless
  servers, minimal window managers, some containers), usagio falls back to a
  `chmod 0600` file at `~/.config/usagio/secrets.json` — no extra package
  needed for that path, but note it isn't encrypted at rest, only
  permission-protected.
- **File dialogs** (Backups Save/Restore in the tray menu) go through
  `xdg-desktop-portal` — this needs an actual portal *backend* running for
  your desktop, not just the base `xdg-desktop-portal` package:
  `xdg-desktop-portal-gtk` (GNOME/generic GTK), `xdg-desktop-portal-kde`
  (KDE Plasma), or `xdg-desktop-portal-wlr` (wlroots-based Wayland
  compositors — Sway, etc.). Most desktop-environment installs pull the
  right one in automatically; a minimal window-manager setup usually needs
  it installed explicitly.
- **Notifications** use `notify-rust`, which talks to whatever
  `org.freedesktop.Notifications` D-Bus service is running (GNOME Shell,
  KDE Plasma, `dunst`, `mako`, etc.) — no extra usagio-specific package.
- **Autostart** just writes a `.desktop` file to `~/.config/autostart/`; any
  desktop environment following the XDG Autostart spec (GNOME, KDE, XFCE,
  MATE, Cinnamon, LXQt, ...) picks it up with no extra package.
