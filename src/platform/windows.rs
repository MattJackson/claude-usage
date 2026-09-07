//! Windows platform impl.
//!
//! Every Windows-specific decision (tray-icon message pump, Credential
//! Manager, registry Run key, `%APPDATA%` / `%LOCALAPPDATA%` paths, terminal
//! probing) lives in this file. Nothing outside `src/platform/` should ever
//! need `#[cfg(target_os = "windows")]` — callers go through the `Platform`
//! trait (`platform::current()`), and the handful of Windows-only helpers
//! below that aren't (yet) part of the trait surface — `probe_terminal`,
//! `launch_repl` — are `pub` only within this module tree so nothing outside
//! it can reach for them by accident. File dialogs go through
//! `Platform::file_dialog()` (see `src/platform/mod.rs::RfdFileDialog`), not
//! a Windows-only helper, since `rfd` is already cross-platform.
//!
//! ## Tray icon threading model
//!
//! `tray-icon` / `muda` build their menu tree out of `Rc<RefCell<_>>`, so
//! `Menu`/`MenuItem`/`Submenu` (and, transitively, `tray_icon::TrayIcon`,
//! which owns a boxed menu) are **not** `Send`. Real Win32 windows are also
//! thread-affine: the hidden window `tray-icon` creates for the notification
//! icon must only ever be touched from the thread that created it and pumps
//! its message queue.
//!
//! Our `MenuHandle` trait, however, requires `Send` — a caller on any thread
//! needs to be able to update the icon/title/menu. We resolve this with a
//! command channel: `WindowsMenuHandle` (the `Send` handle returned to
//! callers) is just a `mpsc::Sender<UiCmd>`. The actual `TrayIcon` lives
//! inside `WindowsMenu`, wrapped in `TrayState` with a documented
//! `unsafe impl Send` — sound because `TrayState` is only ever touched from
//! inside `run_event_loop`, which the trait contract requires to run on the
//! main thread, which is also the thread `create_status_item` must be called
//! from. `run_event_loop`'s pump drains `UiCmd`s and applies them to the
//! real `TrayIcon` in place, so the thread-confined native objects never
//! actually cross a thread boundary — only plain owned data (`Vec<u8>`,
//! `String`, `MenuTree`) does.

use super::*;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem as NativeMenuItem,
    PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

pub struct WindowsPlatform {
    menu: WindowsMenu,
    secrets: WindowsSecrets,
    autostart: WindowsAutostart,
    paths: WindowsPaths,
}

impl WindowsPlatform {
    pub fn new() -> Self {
        Self {
            menu: WindowsMenu::default(),
            secrets: WindowsSecrets,
            autostart: WindowsAutostart,
            paths: WindowsPaths,
        }
    }
}

impl Platform for WindowsPlatform {
    fn menu(&self) -> &dyn MenuBackend {
        &self.menu
    }
    fn secrets(&self) -> &dyn SecretStore {
        &self.secrets
    }
    fn autostart(&self) -> &dyn Autostart {
        &self.autostart
    }
    fn paths(&self) -> &dyn Paths {
        &self.paths
    }
    fn os_display_name(&self) -> &'static str {
        "Windows"
    }
    fn secure_permissions(&self, _path: &Path) -> Result<()> {
        // No chmod equivalent worth forging: `%APPDATA%` / `%LOCALAPPDATA%`
        // are per-user directories already protected by the owning user's
        // NTFS ACL (Credential Manager entries are additionally DPAPI
        // encrypted at rest — see `WindowsSecrets` below). A no-op here, not
        // a scattered `#[cfg(windows)]` at every `0o600`/`0o700` call site in
        // shared code — those sites are already `#[cfg(unix)]`-gated.
        Ok(())
    }
}

// ---- MenuBackend (tray icon) ----------------------------------------------

/// Commands applied to the live `TrayIcon` from inside `run_event_loop`'s
/// pump, on the thread that owns it. See the module doc for why this exists
/// instead of touching `TrayIcon` directly from `MenuHandle` methods.
enum UiCmd {
    SetIcon(Vec<u8>),
    SetTitle(String),
    SetMenu(MenuTree),
}

/// Wraps the thread-confined `TrayIcon` (see module doc). Never touched
/// outside `run_event_loop`, which the `MenuBackend` contract requires to
/// run on the same (main) thread `create_status_item` was called from.
struct TrayState {
    tray: TrayIcon,
}

// SAFETY: `TrayState` holds a `tray_icon::TrayIcon`, which is `!Send`
// because `muda`'s menu tree is `Rc`-based and Win32 window handles are
// thread-affine. We uphold the invariant `tray-icon` itself relies on (only
// ever touch it from its creating thread) by construction: `WindowsMenu`
// only ever reads/writes its `Mutex<Option<TrayState>>` from inside
// `run_event_loop`, and `create_status_item` — which creates the
// `TrayState` — is documented (see `MenuBackend::create_status_item`) as
// being called once at startup on that same main thread. Every other
// thread only ever sends plain owned data through the `UiCmd` channel.
unsafe impl Send for TrayState {}

pub struct WindowsMenu {
    tray_state: Mutex<Option<TrayState>>,
    cmd_rx: Mutex<Option<mpsc::Receiver<UiCmd>>>,
    cmd_tx: Mutex<Option<mpsc::Sender<UiCmd>>>,
    click_cb: Arc<Mutex<Option<Box<dyn Fn(&str) + Send + Sync + 'static>>>>,
    quit: Arc<AtomicBool>,
}

impl Default for WindowsMenu {
    fn default() -> Self {
        Self {
            tray_state: Mutex::new(None),
            cmd_rx: Mutex::new(None),
            cmd_tx: Mutex::new(None),
            click_cb: Arc::new(Mutex::new(None)),
            quit: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// `Send`-safe handle returned to callers. Holds only a channel `Sender` —
/// never the native `TrayIcon` — so it satisfies `MenuHandle: Send` without
/// requiring `tray_icon::TrayIcon` itself to be `Send`.
pub struct WindowsMenuHandle {
    tx: mpsc::Sender<UiCmd>,
}

impl MenuHandle for WindowsMenuHandle {
    fn set_icon(&self, png_bytes: &[u8]) -> Result<()> {
        self.tx
            .send(UiCmd::SetIcon(png_bytes.to_vec()))
            .map_err(|_| anyhow::anyhow!("tray event loop is not running"))
    }
    fn set_title(&self, title: &str) -> Result<()> {
        self.tx
            .send(UiCmd::SetTitle(title.to_string()))
            .map_err(|_| anyhow::anyhow!("tray event loop is not running"))
    }
    fn set_menu(&self, menu: MenuTree) -> Result<()> {
        self.tx
            .send(UiCmd::SetMenu(menu))
            .map_err(|_| anyhow::anyhow!("tray event loop is not running"))
    }
}

/// Decode `bytes` (ICO/BMP per the `MenuHandle::set_icon` doc, but PNG works
/// too — `image::load_from_memory` sniffs the format) into a `tray_icon::Icon`.
fn decode_icon(bytes: &[u8]) -> Result<Icon> {
    let img = image::load_from_memory(bytes).context("decoding tray icon bytes")?;
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    Icon::from_rgba(rgba.into_raw(), width, height)
        .map_err(|e| anyhow::anyhow!("bad tray icon bytes: {e}"))
}

/// Recursively translate one generic `MenuItem` into a native muda item.
/// `icon_png` on `Action`/`Static`/`Submenu` rows is currently ignored
/// (label-only rows) — no caller exercises per-row icons yet; wiring
/// `IconMenuItem` through is deferred until one does.
fn build_item(item: &MenuItem) -> Result<Box<dyn IsMenuItem>> {
    Ok(match item {
        MenuItem::Action {
            id,
            label,
            enabled,
            checked,
            ..
        } => {
            if *checked {
                Box::new(CheckMenuItem::with_id(
                    MenuId::new(id),
                    label,
                    *enabled,
                    *checked,
                    None,
                ))
            } else {
                Box::new(NativeMenuItem::with_id(
                    MenuId::new(id),
                    label,
                    *enabled,
                    None,
                ))
            }
        }
        MenuItem::Static { label, .. } => Box::new(NativeMenuItem::new(label, false, None)),
        MenuItem::Separator => Box::new(PredefinedMenuItem::separator()),
        MenuItem::Submenu { label, items, .. } => {
            let sub = Submenu::new(label, true);
            for child in items {
                let native_child = build_item(child)?;
                sub.append(native_child.as_ref())
                    .map_err(|e| anyhow::anyhow!("submenu append: {e}"))?;
            }
            Box::new(sub)
        }
    })
}

fn build_native_menu(tree: &MenuTree) -> Result<Menu> {
    let menu = Menu::new();
    for item in &tree.items {
        let native = build_item(item)?;
        menu.append(native.as_ref())
            .map_err(|e| anyhow::anyhow!("menu append: {e}"))?;
    }
    Ok(menu)
}

/// Drain the Win32 message queue for the calling thread without blocking.
/// `tray-icon`'s hidden window needs its messages dispatched for mouse
/// clicks / menu commands on the notification icon to turn into
/// `TrayIconEvent`/`MenuEvent` channel entries at all.
fn pump_windows_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid, exclusively-owned out-parameter for the
    // duration of the call; `hwnd = None` means "any window owned by this
    // thread", which is exactly the hidden tray window tray-icon created on
    // this same thread.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn apply_ui_cmd(tray: &TrayIcon, cmd: UiCmd) -> Result<()> {
    match cmd {
        UiCmd::SetIcon(bytes) => {
            let icon = decode_icon(&bytes)?;
            tray.set_icon(Some(icon))
                .map_err(|e| anyhow::anyhow!("set_icon: {e}"))?;
        }
        UiCmd::SetTitle(title) => {
            tray.set_tooltip(Some(title))
                .map_err(|e| anyhow::anyhow!("set_tooltip: {e}"))?;
        }
        UiCmd::SetMenu(tree) => {
            let native = build_native_menu(&tree)?;
            tray.set_menu(Some(Box::new(native)));
        }
    }
    Ok(())
}

impl MenuBackend for WindowsMenu {
    fn create_status_item(
        &self,
        initial_title: &str,
        initial_icon: &[u8],
    ) -> Result<Box<dyn MenuHandle>> {
        let icon = decode_icon(initial_icon)?;
        let menu = Menu::new();
        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip(initial_title)
            .with_menu(Box::new(menu))
            .build()
            .map_err(|e| anyhow::anyhow!("failed to create tray icon: {e}"))?;

        let (tx, rx) = mpsc::channel::<UiCmd>();
        *self.tray_state.lock().unwrap() = Some(TrayState { tray });
        *self.cmd_rx.lock().unwrap() = Some(rx);
        *self.cmd_tx.lock().unwrap() = Some(tx.clone());

        Ok(Box::new(WindowsMenuHandle { tx }))
    }

    fn on_click(&self, cb: Box<dyn Fn(&str) + Send + Sync + 'static>) -> Result<()> {
        *self.click_cb.lock().unwrap() = Some(cb);
        Ok(())
    }

    fn run_event_loop(&self) -> Result<()> {
        let rx = self
            .cmd_rx
            .lock()
            .unwrap()
            .take()
            .context("run_event_loop called before create_status_item")?;
        let mut guard = self.tray_state.lock().unwrap();
        let state = guard
            .as_mut()
            .context("run_event_loop called before create_status_item")?;

        let menu_rx = MenuEvent::receiver();
        self.quit.store(false, Ordering::SeqCst);
        loop {
            if self.quit.load(Ordering::SeqCst) {
                break;
            }
            pump_windows_messages();
            while let Ok(cmd) = rx.try_recv() {
                if let Err(e) = apply_ui_cmd(&state.tray, cmd) {
                    crate::logging::log(&format!("windows tray: UI command failed: {e}"));
                }
            }
            while let Ok(ev) = menu_rx.try_recv() {
                if let Some(cb) = self.click_cb.lock().unwrap().as_ref() {
                    cb(&ev.id.0);
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        Ok(())
    }

    fn request_quit(&self) {
        self.quit.store(true, Ordering::SeqCst);
    }
}

// ---- SecretStore (Credential Manager via `keyring`) -----------------------

/// Windows Credential Manager, via the `keyring` crate's `windows-native-
/// keyring-store` backend. Per-user, DPAPI-encrypted at rest — no fallback
/// needed the way macOS's unsigned-binary keychain-prompt problem needed one
/// (see the header comment in `macos.rs`).
pub struct WindowsSecrets;

impl SecretStore for WindowsSecrets {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        let entry =
            keyring::Entry::new(service, account).context("opening Credential Manager entry")?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => {
                bail!("Credential Manager read failed for service={service} account={account}: {e}")
            }
        }
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        let entry =
            keyring::Entry::new(service, account).context("opening Credential Manager entry")?;
        entry.set_password(secret).with_context(|| {
            format!("Credential Manager write for service={service} account={account}")
        })
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let entry =
            keyring::Entry::new(service, account).context("opening Credential Manager entry")?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => {
                bail!(
                    "Credential Manager delete failed for service={service} account={account}: {e}"
                )
            }
        }
    }

    fn list(&self, _service: &str) -> Result<Vec<String>> {
        // Enumerating every Credential Manager entry for a service requires
        // the lower-level `keyring-core` + `CredEnumerateW`, which the `v1`
        // shim doesn't expose. Same documented caveat as macOS's `list` —
        // callers track accounts via state.json today.
        Ok(Vec::new())
    }
}

// ---- Autostart (HKCU Run key) ----------------------------------------------

use winreg::enums::{HKEY_CURRENT_USER, KEY_ALL_ACCESS};
use winreg::RegKey;

const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub struct WindowsAutostart;

impl WindowsAutostart {
    fn run_key() -> Result<RegKey> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _disposition) = hkcu
            .create_subkey_with_flags(RUN_KEY_PATH, KEY_ALL_ACCESS)
            .with_context(|| format!(r"opening HKCU\{RUN_KEY_PATH}"))?;
        Ok(key)
    }

    /// Build the registry value: the binary path always quoted (paths may
    /// contain spaces — `C:\Program Files\...`), each arg quoted only if it
    /// contains whitespace. `usagio menubar` never needs it, but a future
    /// caller passing a path-like arg shouldn't corrupt the command line.
    fn command_line(binary: &Path, args: &[&str]) -> String {
        let mut out = format!("\"{}\"", binary.display());
        for a in args {
            if a.contains(char::is_whitespace) {
                out.push_str(&format!(" \"{a}\""));
            } else {
                out.push(' ');
                out.push_str(a);
            }
        }
        out
    }
}

impl Autostart for WindowsAutostart {
    fn install(&self, label: &str, binary: &Path, args: &[&str]) -> Result<()> {
        let key = Self::run_key()?;
        let value = Self::command_line(binary, args);
        key.set_value(label, &value)
            .with_context(|| format!(r"writing HKCU\{RUN_KEY_PATH}\{label}"))
    }

    fn uninstall(&self, label: &str) -> Result<()> {
        let key = Self::run_key()?;
        match key.delete_value(label) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!(r"deleting HKCU\{RUN_KEY_PATH}\{label}")),
        }
    }

    fn is_installed(&self, label: &str) -> Result<bool> {
        let key = Self::run_key()?;
        match key.get_value::<String, _>(label) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!(r"reading HKCU\{RUN_KEY_PATH}\{label}")),
        }
    }

    fn restart(&self, label: &str) -> Result<()> {
        // No live-restart primitive for a registry Run key (it only governs
        // the *next* login). "Restart" here means: read back whatever
        // `install()` most recently wrote (the freshly-installed binary +
        // args) and spawn it now, so a hot-swap (e.g. an installer replacing
        // usagio.exe) picks up the new binary immediately rather than
        // waiting for the next login.
        let key = Self::run_key()?;
        let value: String = key
            .get_value(label)
            .with_context(|| format!(r"reading HKCU\{RUN_KEY_PATH}\{label} to restart"))?;
        let (program, args) = split_command_line(&value)
            .with_context(|| format!("parsing Run key value for {label}: {value:?}"))?;
        Command::new(&program)
            .args(&args)
            .spawn()
            .with_context(|| format!("spawning {program} {args:?}"))?;
        Ok(())
    }
}

/// Split a `"<program>" arg1 "arg with spaces" arg2`-shaped command line (the
/// exact shape `WindowsAutostart::command_line` produces) into a program
/// path and its remaining args. Not a general shell-lexer — just enough to
/// round-trip what we write.
fn split_command_line(value: &str) -> Result<(String, Vec<String>)> {
    let value = value.trim();
    if !value.starts_with('"') {
        bail!("expected a quoted program path, got: {value:?}");
    }
    let rest = &value[1..];
    let end = rest
        .find('"')
        .context("unterminated quote in program path")?;
    let program = rest[..end].to_string();
    if program.is_empty() {
        bail!("empty program path");
    }
    let mut args = Vec::new();
    let mut remaining = rest[end + 1..].trim_start();
    while !remaining.is_empty() {
        if let Some(after_quote) = remaining.strip_prefix('"') {
            let close = after_quote
                .find('"')
                .context("unterminated quote in argument")?;
            args.push(after_quote[..close].to_string());
            remaining = after_quote[close + 1..].trim_start();
        } else {
            let end = remaining
                .find(char::is_whitespace)
                .unwrap_or(remaining.len());
            args.push(remaining[..end].to_string());
            remaining = remaining[end..].trim_start();
        }
    }
    Ok((program, args))
}

// ---- Paths -----------------------------------------------------------------

pub struct WindowsPaths;

/// `%APPDATA%` (Roaming) — falls back to `dirs::config_dir()` (which
/// resolves the same known folder via `SHGetKnownFolderPath` and so doesn't
/// depend on the env var), then to `%USERPROFILE%\.usagio` as a last
/// resort. Reading the env var first (rather than going straight to `dirs`)
/// is deliberate: it's what makes `config_dir` testable by setting `APPDATA`
/// in-process, and it's the same value `dirs` would resolve to on any
/// normally-configured Windows install.
fn appdata_root() -> PathBuf {
    if let Some(p) = std::env::var_os("APPDATA") {
        return PathBuf::from(p);
    }
    if let Some(p) = dirs::config_dir() {
        return p;
    }
    userprofile_fallback()
}

/// `%LOCALAPPDATA%` — same fallback chain as `appdata_root`.
fn localappdata_root() -> PathBuf {
    if let Some(p) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(p);
    }
    if let Some(p) = dirs::cache_dir() {
        return p;
    }
    userprofile_fallback()
}

fn userprofile_fallback() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_default();
    home.join(".usagio")
}

impl Paths for WindowsPaths {
    fn config_dir(&self, app: &str) -> PathBuf {
        appdata_root().join(app)
    }
    fn data_dir(&self, app: &str) -> PathBuf {
        self.config_dir(app)
    }
    fn log_dir(&self, app: &str) -> PathBuf {
        self.config_dir(app)
    }
    fn cache_dir(&self, app: &str) -> PathBuf {
        localappdata_root().join(app)
    }
}

// ---- Terminal launcher ------------------------------------------------------
//
// Not (yet) part of the `Platform` trait — no caller launches an interactive
// REPL in a new terminal window on any OS today (`Provider::launch_client`
// runs the vendor CLI in-place via `Command::status()`; see
// `src/providers/claude/mod.rs`). This is the Windows-side primitive for
// when that lands; kept `pub(crate)` so it's ready to wire up without
// needing another pass through `src/platform/`.

/// Which terminal `launch_repl` picked, exposed for tests / diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalKind {
    WindowsTerminal,
    Cmd,
    PowerShell,
}

/// Probe order: Windows Terminal, then `cmd.exe`, then `powershell.exe`.
/// `exists` is injected so the selection logic is unit-testable without
/// touching the real `PATH` — production code passes
/// `|prog| which::which(prog).is_ok()`.
pub(crate) fn probe_terminal(mut exists: impl FnMut(&str) -> bool) -> TerminalKind {
    if exists("wt.exe") {
        TerminalKind::WindowsTerminal
    } else if exists("cmd.exe") {
        TerminalKind::Cmd
    } else {
        TerminalKind::PowerShell
    }
}

impl TerminalKind {
    fn command(self, program: &str) -> Command {
        match self {
            TerminalKind::WindowsTerminal => {
                let mut c = Command::new("wt.exe");
                c.args(["new-tab", program]);
                c
            }
            TerminalKind::Cmd => {
                let mut c = Command::new("cmd.exe");
                c.args(["/K", program]);
                c
            }
            TerminalKind::PowerShell => {
                let mut c = Command::new("powershell.exe");
                c.args(["-NoExit", program]);
                c
            }
        }
    }
}

/// Launch `program` (e.g. `"claude"`) in a new interactive terminal window,
/// probing wt.exe → cmd.exe → powershell.exe. Fire-and-forget: the spawned
/// terminal owns its own lifetime, we don't wait on it.
#[allow(dead_code)]
pub(crate) fn launch_repl(program: &str) -> Result<()> {
    let kind = probe_terminal(|prog| which::which(prog).is_ok());
    kind.command(program)
        .spawn()
        .with_context(|| format!("launching {program} via {kind:?}"))?;
    Ok(())
}

// ---- File dialogs -----------------------------------------------------------
//
// `rfd` is cross-platform (Win32 IFileOpenDialog under the hood here) and
// needs no Windows-specific wiring beyond being a dependency — see the note
// in Cargo.toml. Routed through `Platform::file_dialog()` /
// `platform::RfdFileDialog` (src/platform/mod.rs) rather than a Windows-only
// free function, so `crate::menubar`'s Backups Save…/Restore… handlers call
// one cross-platform trait method instead of reaching for `rfd::` directly.

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_terminal_probe_prefers_wt_if_present() {
        let kind = probe_terminal(|prog| prog == "wt.exe");
        assert_eq!(kind, TerminalKind::WindowsTerminal);
    }

    #[test]
    fn terminal_probe_falls_back_to_cmd_when_wt_missing() {
        let kind = probe_terminal(|prog| prog == "cmd.exe");
        assert_eq!(kind, TerminalKind::Cmd);
    }

    #[test]
    fn terminal_probe_falls_back_to_powershell_when_nothing_else_found() {
        let kind = probe_terminal(|_prog| false);
        assert_eq!(kind, TerminalKind::PowerShell);
    }

    #[test]
    fn command_line_quotes_binary_and_bare_args() {
        let line =
            WindowsAutostart::command_line(Path::new(r"C:\Path\To\usagio.exe"), &["menubar"]);
        assert_eq!(line, r#""C:\Path\To\usagio.exe" menubar"#);
    }

    #[test]
    fn command_line_quotes_args_with_spaces() {
        let line = WindowsAutostart::command_line(
            Path::new(r"C:\Program Files\usagio\usagio.exe"),
            &["menubar"],
        );
        assert_eq!(line, r#""C:\Program Files\usagio\usagio.exe" menubar"#);
    }

    #[test]
    fn split_command_line_round_trips_command_line() {
        let line =
            WindowsAutostart::command_line(Path::new(r"C:\Path\To\usagio.exe"), &["menubar"]);
        let (program, args) = split_command_line(&line).unwrap();
        assert_eq!(program, r"C:\Path\To\usagio.exe");
        assert_eq!(args, vec!["menubar".to_string()]);
    }

    #[test]
    fn split_command_line_round_trips_quoted_program_with_spaces() {
        let line = WindowsAutostart::command_line(
            Path::new(r"C:\Program Files\usagio\usagio.exe"),
            &["menubar"],
        );
        let (program, args) = split_command_line(&line).unwrap();
        assert_eq!(program, r"C:\Program Files\usagio\usagio.exe");
        assert_eq!(args, vec!["menubar".to_string()]);
    }

    #[test]
    fn windows_paths_use_appdata_config_dir() {
        crate::env_lock::scoped_env_var("APPDATA", Some(r"C:\Users\dev\AppData\Roaming"), || {
            let got = WindowsPaths.config_dir("usagio");
            assert_eq!(
                got,
                PathBuf::from(r"C:\Users\dev\AppData\Roaming").join("usagio")
            );
        });
    }

    #[test]
    fn localappdata_root_uses_localappdata_env_var() {
        crate::env_lock::scoped_env_var(
            "LOCALAPPDATA",
            Some(r"C:\Users\dev\AppData\Local"),
            || {
                let got = WindowsPaths.cache_dir("usagio");
                assert_eq!(
                    got,
                    PathBuf::from(r"C:\Users\dev\AppData\Local").join("usagio")
                );
            },
        );
    }

    /// Registry round-trip against the real `HKCU` Run key. `#[ignore]`d by
    /// default — this is the one test in this module with a real side
    /// effect on the machine running it. Run explicitly on Windows with
    /// `cargo test -- --ignored windows_autostart_writes_registry_key`.
    #[test]
    #[ignore = "writes to the real HKCU Run key; run with --ignored on Windows"]
    fn windows_autostart_writes_registry_key() {
        let label = format!("usagio-platform-test-{}", std::process::id());
        let autostart = WindowsAutostart;
        let binary = Path::new(r"C:\Path\To\usagio-test.exe");

        let _ = autostart.uninstall(&label);
        assert!(!autostart.is_installed(&label).unwrap());

        autostart.install(&label, binary, &["menubar"]).unwrap();
        assert!(autostart.is_installed(&label).unwrap());

        let key = WindowsAutostart::run_key().unwrap();
        let value: String = key.get_value(&label).unwrap();
        assert_eq!(value, r#""C:\Path\To\usagio-test.exe" menubar"#);

        autostart.uninstall(&label).unwrap();
        assert!(!autostart.is_installed(&label).unwrap());
    }
}
