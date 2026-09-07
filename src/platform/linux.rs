//! Linux platform impl.
//!
//! Every Linux-specific decision lives in this file — nothing outside
//! `src/platform/linux.rs` should ever need `#[cfg(target_os = "linux")]`.
//!
//! - **Tray icon**: the same `tray-icon` crate macOS/Windows use. On Linux
//!   it's backed by `libappindicator`/`libayatana-appindicator3`, itself a
//!   GTK status-icon wrapper, so an actual GTK main loop has to be pumped on
//!   the thread that created the tray icon (see the module doc on
//!   `LinuxMenu` below for how that's threaded through the `Send`-bound
//!   trait objects the rest of the crate holds onto).
//! - **Secrets**: `keyring`'s Secret Service backend (GNOME Keyring / KWallet
//!   over D-Bus), falling back to a permissions-protected file when no
//!   daemon is reachable (headless servers, containers, minimal window
//!   managers with no keyring agent running).
//! - **Autostart**: an XDG autostart `.desktop` file under
//!   `~/.config/autostart/`.
//! - **Paths**: XDG base directories (`$XDG_CONFIG_HOME` / `$XDG_CACHE_HOME`
//!   with the standard `~/.config` / `~/.cache` fallbacks).
//! - **File dialogs** (Backups Save/Restore in the tray menu) and
//!   **notifications** need no Linux-specific code at all: `rfd`'s
//!   `xdg-desktop-portal` backend and `notify_rust` (see
//!   `crate::notifications::fire`) already work cross-platform. `rfd` is
//!   still declared as a Linux-only Cargo dependency for now since nothing
//!   outside Linux calls it yet.
//! - **Terminal launcher**: `usagio start` / `continue` want to run `claude`
//!   in a fresh terminal window when invoked from the tray (the tray process
//!   itself isn't attached to a terminal). `launch_claude_in_terminal` probes
//!   a priority list of terminal emulators; it isn't wired into the
//!   `Platform` trait (there's no cross-platform "open a terminal" concept
//!   in `mod.rs`) — Linux call sites should invoke it directly once the
//!   Linux tray's Start/Continue menu actions land.

use super::*;
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

pub struct LinuxPlatform {
    menu: LinuxMenu,
    secrets: LinuxSecrets,
    autostart: LinuxAutostart,
    paths: LinuxPaths,
}

impl LinuxPlatform {
    pub fn new() -> Self {
        Self {
            menu: LinuxMenu::new(),
            secrets: LinuxSecrets,
            autostart: LinuxAutostart,
            paths: LinuxPaths,
        }
    }
}

impl Platform for LinuxPlatform {
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
        "Linux"
    }
    fn secure_permissions(&self, path: &Path) -> Result<()> {
        super::secure_permissions_unix(path)
    }
}

// ---------------------------------------------------------------------------
// Paths — XDG base directories
// ---------------------------------------------------------------------------

pub struct LinuxPaths;

/// `$XDG_CONFIG_HOME`, or `~/.config` if unset/empty.
fn xdg_config_home() -> PathBuf {
    if let Some(v) = std::env::var_os("XDG_CONFIG_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    home_dir().join(".config")
}

/// `$XDG_CACHE_HOME`, or `~/.cache` if unset/empty.
fn xdg_cache_home() -> PathBuf {
    if let Some(v) = std::env::var_os("XDG_CACHE_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    home_dir().join(".cache")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

impl Paths for LinuxPaths {
    fn config_dir(&self, app: &str) -> PathBuf {
        xdg_config_home().join(app)
    }
    fn data_dir(&self, app: &str) -> PathBuf {
        // Same directory as config, matching macOS's choice — there isn't
        // enough state yet to justify splitting XDG_DATA_HOME out.
        self.config_dir(app)
    }
    fn log_dir(&self, app: &str) -> PathBuf {
        self.config_dir(app)
    }
    fn cache_dir(&self, app: &str) -> PathBuf {
        xdg_cache_home().join(app)
    }
}

// ---------------------------------------------------------------------------
// Autostart — XDG autostart .desktop file
// ---------------------------------------------------------------------------

/// Fixed filename: usagio only ever registers one autostart entry, so unlike
/// macOS's launchd label (which names the plist, since launchd is a general
/// multi-service manager) there's no need to derive the filename from
/// `label`. `label` is accepted (and ignored) purely to satisfy the
/// cross-platform `Autostart` trait shape.
const AUTOSTART_DESKTOP_FILE: &str = "usagio.desktop";

pub struct LinuxAutostart;

impl LinuxAutostart {
    fn desktop_path() -> PathBuf {
        xdg_config_home()
            .join("autostart")
            .join(AUTOSTART_DESKTOP_FILE)
    }
}

/// Quote one `Exec=` argument per the freedesktop Desktop Entry spec: an
/// argument containing whitespace or a reserved shell metacharacter must be
/// wrapped in double quotes, with `\`, `"`, `` ` ``, and `$` backslash-escaped
/// inside the quotes. Same class of bug as macOS's plist `xml_escape` guards
/// against — a path or arg breaking the generated file.
fn desktop_exec_quote(s: &str) -> String {
    let needs_quoting = s.is_empty()
        || s.chars()
            .any(|c| c.is_whitespace() || "\"'\\$`><|&;()#*?[]!{}~".contains(c));
    if !needs_quoting {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '`' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

impl Autostart for LinuxAutostart {
    fn install(&self, label: &str, binary: &Path, args: &[&str]) -> Result<()> {
        let _ = label; // filename is fixed — see AUTOSTART_DESKTOP_FILE.
        let mut exec = desktop_exec_quote(&binary.display().to_string());
        for a in args {
            exec.push(' ');
            exec.push_str(&desktop_exec_quote(a));
        }
        let contents = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=usagio\n\
             Exec={exec}\n\
             Terminal=false\n\
             Hidden=false\n\
             X-GNOME-Autostart-enabled=true\n"
        );
        let path = Self::desktop_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms)
            .with_context(|| format!("chmod 0644 {}", path.display()))?;
        Ok(())
    }

    fn uninstall(&self, label: &str) -> Result<()> {
        let _ = label;
        let path = Self::desktop_path();
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        Ok(())
    }

    fn is_installed(&self, label: &str) -> Result<bool> {
        let _ = label;
        Ok(Self::desktop_path().exists())
    }

    fn restart(&self, label: &str) -> Result<()> {
        let _ = label;
        // No systemd user unit (or equivalent live-restart primitive) is
        // managed here, so — like Windows' registry Run key per the trait
        // doc — this is uninstall+install+spawn in spirit: read back the
        // `Exec=` line we wrote in `install`, best-effort kill any already
        // running instance of that binary (`brew upgrade` hot-swap shouldn't
        // leave two processes fighting over the tray icon), then spawn a
        // fresh detached instance.
        let path = Self::desktop_path();
        let contents = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "reading {} to restart — run `usagio install` first",
                path.display()
            )
        })?;
        let exec_line = contents
            .lines()
            .find_map(|l| l.strip_prefix("Exec="))
            .context("usagio.desktop has no Exec= line")?;
        let mut parts = exec_line.split_whitespace();
        let bin = parts.next().context("Exec= line is empty")?;
        let rest: Vec<&str> = parts.collect();

        let bin_name = Path::new(bin)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(bin);
        // Best-effort: absence of `pkill`, or nothing running, is fine.
        let _ = Command::new("pkill").args(["-f", bin_name]).status();
        std::thread::sleep(std::time::Duration::from_millis(200));

        Command::new(bin)
            .args(&rest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning {bin} after restart"))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SecretStore — Secret Service via `keyring`, with an offline file fallback
// ---------------------------------------------------------------------------

pub struct LinuxSecrets;

/// `true` for the `keyring::Error` variants that mean "the daemon isn't
/// reachable" (no Secret Service running, locked with no prompt possible,
/// D-Bus session bus missing — the headless-server case) as opposed to a
/// real logic error worth surfacing.
fn is_daemon_unavailable(e: &keyring::Error) -> bool {
    matches!(
        e,
        keyring::Error::NoStorageAccess(_) | keyring::Error::PlatformFailure(_)
    )
}

/// Fallback store keyed by `"{service}\u{1}{account}"` inside one JSON object
/// at `<config_dir>/secrets.json`, `chmod 0600`. This is a permissions-only
/// safeguard, not encryption at rest — there's no master passphrase in this
/// design to derive a real key from, the same tradeoff macOS's plaintext-argv
/// `security(1)` call already documents (see SEC-2 in `macos.rs`). Only used
/// when the Secret Service daemon isn't reachable at all.
fn fallback_path(dir: &Path) -> PathBuf {
    dir.join("secrets.json")
}

fn fallback_key(service: &str, account: &str) -> String {
    format!("{service}\u{1}{account}")
}

fn fallback_load(dir: &Path) -> Result<Map<String, Value>> {
    let path = fallback_path(dir);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let v: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok(v.as_object().cloned().unwrap_or_default())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn fallback_write(dir: &Path, map: &Map<String, Value>) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = fallback_path(dir);
    let body = serde_json::to_vec_pretty(map).context("serializing secrets.json fallback")?;
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    let mut perms = std::fs::metadata(&path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(&path, perms)
        .with_context(|| format!("chmod 0600 {}", path.display()))?;
    Ok(())
}

fn fallback_get(dir: &Path, service: &str, account: &str) -> Result<Option<String>> {
    let map = fallback_load(dir)?;
    Ok(map
        .get(&fallback_key(service, account))
        .and_then(|v| v.as_str())
        .map(String::from))
}

fn fallback_set(dir: &Path, service: &str, account: &str, secret: &str) -> Result<()> {
    let mut map = fallback_load(dir)?;
    map.insert(
        fallback_key(service, account),
        Value::String(secret.to_string()),
    );
    fallback_write(dir, &map)
}

fn fallback_delete(dir: &Path, service: &str, account: &str) -> Result<()> {
    let mut map = fallback_load(dir)?;
    if map.remove(&fallback_key(service, account)).is_some() {
        fallback_write(dir, &map)?;
    }
    Ok(())
}

fn fallback_list(dir: &Path, service: &str) -> Result<Vec<String>> {
    let map = fallback_load(dir)?;
    let prefix = format!("{service}\u{1}");
    Ok(map
        .keys()
        .filter_map(|k| k.strip_prefix(&prefix))
        .map(String::from)
        .collect())
}

/// Parameterized over the actual keyring call so tests can inject a
/// `keyring::Error` without touching a real Secret Service daemon (or the
/// lack of one in CI).
fn get_impl(
    keyring_get: impl FnOnce() -> keyring::Result<String>,
    dir: &Path,
    service: &str,
    account: &str,
) -> Result<Option<String>> {
    match keyring_get() {
        Ok(s) => Ok(Some(s)),
        Err(keyring::Error::NoEntry) => fallback_get(dir, service, account),
        Err(e) if is_daemon_unavailable(&e) => fallback_get(dir, service, account),
        Err(e) => bail!("keyring get failed for service={service} account={account}: {e}"),
    }
}

fn set_impl(
    keyring_set: impl FnOnce() -> keyring::Result<()>,
    dir: &Path,
    service: &str,
    account: &str,
    secret: &str,
) -> Result<()> {
    match keyring_set() {
        Ok(()) => Ok(()),
        Err(e) if is_daemon_unavailable(&e) => fallback_set(dir, service, account, secret),
        Err(e) => bail!("keyring set failed for service={service} account={account}: {e}"),
    }
}

fn delete_impl(
    keyring_delete: impl FnOnce() -> keyring::Result<()>,
    dir: &Path,
    service: &str,
    account: &str,
) -> Result<()> {
    match keyring_delete() {
        // Also clear any stale fallback entry left over from a period when
        // the daemon was unreachable, so `get` after `delete` can't resurrect
        // it from the file.
        Ok(()) => fallback_delete(dir, service, account),
        Err(keyring::Error::NoEntry) => fallback_delete(dir, service, account),
        Err(e) if is_daemon_unavailable(&e) => fallback_delete(dir, service, account),
        Err(e) => bail!("keyring delete failed for service={service} account={account}: {e}"),
    }
}

impl SecretStore for LinuxSecrets {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        let dir = xdg_config_home().join("usagio");
        get_impl(
            || keyring::Entry::new(service, account)?.get_password(),
            &dir,
            service,
            account,
        )
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        let dir = xdg_config_home().join("usagio");
        set_impl(
            || keyring::Entry::new(service, account)?.set_password(secret),
            &dir,
            service,
            account,
            secret,
        )
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let dir = xdg_config_home().join("usagio");
        delete_impl(
            || keyring::Entry::new(service, account)?.delete_credential(),
            &dir,
            service,
            account,
        )
    }

    fn list(&self, service: &str) -> Result<Vec<String>> {
        // Secret Service (and `keyring`'s abstraction over it) has no
        // "enumerate every item for a service" primitive without walking the
        // whole default collection, which the crate doesn't expose. Same
        // documented caveat as macOS's `security dump-keychain` (needs a UI
        // prompt) — callers already track accounts via state.json. Accounts
        // that live only in the offline fallback file *are* enumerable, so
        // return those.
        let dir = xdg_config_home().join("usagio");
        fallback_list(&dir, service)
    }
}

// ---------------------------------------------------------------------------
// MenuBackend — tray-icon + a GTK main loop
// ---------------------------------------------------------------------------

/// `tray_icon::TrayIcon` wraps an `Rc<RefCell<..>>` (and, transitively, a raw
/// `AppIndicator` pointer) on Linux — it is not `Send`. But `MenuBackend` and
/// `MenuHandle` both require `Send` (+`Sync` for the backend) so the rest of
/// the crate can hold a `Box<dyn Platform>` in a `OnceLock` without caring
/// which OS it's on. The two requirements are reconciled like this:
///
/// - The actual `TrayIcon` (and the receiving end of the handle-mutation
///   channel below) live in a `thread_local!`, never as a field of `LinuxMenu`
///   itself. `create_status_item` must be called on the same thread that
///   later calls `run_event_loop` — exactly the shape `crate::menubar`
///   already uses on macOS (build the tray, then block in the run loop, both
///   on the same thread) — so this thread-local is populated and drained by
///   the same OS thread throughout the process's life.
/// - `LinuxMenuHandle` (the `Send` value returned to callers) holds only an
///   `mpsc::Sender` of plain owned data (`Vec<u8>` / `String` / `MenuTree`).
///   Mutating the tray from any thread just queues a message; the GTK
///   thread's `run_event_loop` poll applies it to the thread-local tray.
/// - `glib::MainLoop` (used for `request_quit`) genuinely is `Send + Sync` —
///   GLib documents `g_main_loop_quit` as callable from any thread — so it's
///   stored directly as an `Arc<Mutex<Option<glib::MainLoop>>>` field.
// ClickCb: factored-out signature for the tray's click-handler callback
// slot. Keeps `LinuxMenu::click_cb` under clippy's type-complexity
// threshold; mirror of `WindowsMenu`'s `ClickCb`.
type ClickCb = Box<dyn Fn(&str) + Send + Sync + 'static>;

pub struct LinuxMenu {
    click_cb: Arc<Mutex<Option<ClickCb>>>,
    main_loop: Arc<Mutex<Option<gtk::glib::MainLoop>>>,
    gtk_init: OnceLock<Result<(), String>>,
    /// Set by `request_quit` when it's called before `run_event_loop` has
    /// populated `main_loop` — otherwise that quit request would be silently
    /// discarded (there's no `glib::MainLoop` yet to call `.quit()` on).
    /// `run_event_loop` checks this on entry and returns immediately if set,
    /// so a quit requested during startup is still honored.
    should_quit_early: std::sync::atomic::AtomicBool,
}

impl LinuxMenu {
    fn new() -> Self {
        Self {
            click_cb: Arc::new(Mutex::new(None)),
            main_loop: Arc::new(Mutex::new(None)),
            gtk_init: OnceLock::new(),
            should_quit_early: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn ensure_gtk_init(&self) -> Result<()> {
        match self
            .gtk_init
            .get_or_init(|| gtk::init().map_err(|e| e.to_string()))
        {
            Ok(()) => Ok(()),
            Err(e) => bail!("gtk::init failed: {e}"),
        }
    }
}

/// The `Set*` naming is deliberate — each variant is a setter operation
/// on the tray. Renaming to drop the shared prefix would only obscure
/// the intent for clippy's benefit (mirror of `WindowsMenu`'s `UiCmd`).
#[allow(clippy::enum_variant_names)]
enum HandleMsg {
    SetIcon(Vec<u8>),
    SetTitle(String),
    SetMenu(MenuTree),
}

struct TrayState {
    tray: tray_icon::TrayIcon,
    rx: std::sync::mpsc::Receiver<HandleMsg>,
}

thread_local! {
    /// Populated by `create_status_item`, drained by `run_event_loop`'s poll
    /// tick — both required (by the `LinuxMenu` doc above) to run on the same
    /// thread, so a plain `RefCell` (not a `Mutex`) is enough here.
    static TRAY_STATE: std::cell::RefCell<Option<TrayState>> = const { std::cell::RefCell::new(None) };
}

/// Decode PNG bytes (the format every bundled provider icon and tray icon
/// ships as, see `src/icons.rs`) into raw RGBA + dimensions. Pure Rust (the
/// `png` crate), so it cross-compiles without a system libpng.
fn decode_png_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .context("decoding PNG header for a tray/menu icon")?;
    let buf_size = reader
        .output_buffer_size()
        .context("PNG output buffer size overflow")?;
    let mut buf = vec![0u8; buf_size];
    let info = reader
        .next_frame(&mut buf)
        .context("decoding PNG frame for a tray/menu icon")?;
    let bytes = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => {
            // as_chunks::<3>() is what clippy prefers over chunks_exact(3);
            // it returns a slice of fixed-size arrays so the compiler can
            // elide the bounds check inside the closure below.
            let (chunks, _rem) = bytes.as_chunks::<3>();
            chunks
                .iter()
                .flat_map(|c| [c[0], c[1], c[2], 255])
                .collect()
        }
        other => {
            bail!("unsupported PNG color type for a tray/menu icon: {other:?} (need RGB or RGBA)")
        }
    };
    Ok((rgba, info.width, info.height))
}

fn decode_tray_icon(bytes: &[u8]) -> Result<tray_icon::Icon> {
    let (rgba, w, h) = decode_png_rgba(bytes)?;
    tray_icon::Icon::from_rgba(rgba, w, h).context("building a tray icon from decoded PNG")
}

fn decode_menu_icon(bytes: &[u8]) -> Result<tray_icon::menu::Icon> {
    let (rgba, w, h) = decode_png_rgba(bytes)?;
    tray_icon::menu::Icon::from_rgba(rgba, w, h)
        .context("building a menu-item icon from decoded PNG")
}

/// `tray_icon::menu::{Menu, Submenu}` both have an inherent `append(&dyn
/// IsMenuItem)` method but share no common trait that exposes it, so this
/// bridges the two for the recursive `MenuTree` walk below.
trait NativeMenuContainer {
    fn append_native(&self, item: &dyn tray_icon::menu::IsMenuItem);
}
impl NativeMenuContainer for tray_icon::menu::Menu {
    fn append_native(&self, item: &dyn tray_icon::menu::IsMenuItem) {
        let _ = self.append(item);
    }
}
impl NativeMenuContainer for tray_icon::menu::Submenu {
    fn append_native(&self, item: &dyn tray_icon::menu::IsMenuItem) {
        let _ = self.append(item);
    }
}

/// Translate the cross-platform `MenuTree` (from `mod.rs`) into a native
/// `tray-icon`/`muda` menu. Only ever called from the GTK thread — the
/// `muda` item types (`Rc`-backed) aren't `Send` either, which is fine since
/// nothing here escapes past the caller in `apply_handle_msg`.
fn build_native_menu(tree: &MenuTree) -> tray_icon::menu::Menu {
    let menu = tray_icon::menu::Menu::new();
    append_children(&menu, &tree.items);
    menu
}

fn append_children(container: &dyn NativeMenuContainer, items: &[MenuItem]) {
    use tray_icon::menu::{
        CheckMenuItem, IconMenuItem, MenuItem as NativeMenuItem, PredefinedMenuItem, Submenu,
    };
    for item in items {
        match item {
            MenuItem::Action {
                id,
                label,
                icon_png,
                enabled,
                checked,
            } => {
                if *checked {
                    container.append_native(&CheckMenuItem::with_id(
                        id.as_str(),
                        label,
                        *enabled,
                        *checked,
                        None,
                    ));
                } else if let Some(icon_bytes) = icon_png {
                    match decode_menu_icon(icon_bytes) {
                        Ok(icon) => container.append_native(&IconMenuItem::with_id(
                            id.as_str(),
                            label,
                            *enabled,
                            Some(icon),
                            None,
                        )),
                        Err(_) => container.append_native(&NativeMenuItem::with_id(
                            id.as_str(),
                            label,
                            *enabled,
                            None,
                        )),
                    }
                } else {
                    container.append_native(&NativeMenuItem::with_id(
                        id.as_str(),
                        label,
                        *enabled,
                        None,
                    ));
                }
            }
            MenuItem::Static { label, .. } => {
                container.append_native(&NativeMenuItem::with_id("noop", label, false, None));
            }
            MenuItem::Separator => {
                container.append_native(&PredefinedMenuItem::separator());
            }
            MenuItem::Submenu { label, items, .. } => {
                let sub = Submenu::new(label, true);
                append_children(&sub, items);
                container.append_native(&sub);
            }
        }
    }
}

fn apply_handle_msg(tray: &tray_icon::TrayIcon, msg: HandleMsg) {
    match msg {
        HandleMsg::SetIcon(bytes) => match decode_tray_icon(&bytes) {
            Ok(icon) => {
                if let Err(e) = tray.set_icon(Some(icon)) {
                    crate::logging::log(&format!("linux tray: set_icon failed: {e}"));
                }
            }
            Err(e) => crate::logging::log(&format!("linux tray: set_icon decode failed: {e:#}")),
        },
        HandleMsg::SetTitle(title) => {
            // tray-icon docs: tooltips are unsupported on Linux; the title
            // (shown next to the icon, requires an icon to be set — which we
            // always have) is the supported analog.
            tray.set_title(Some(title));
        }
        HandleMsg::SetMenu(tree) => {
            let native = build_native_menu(&tree);
            tray.set_menu(Some(Box::new(native)));
        }
    }
}

pub struct LinuxMenuHandle {
    tx: std::sync::mpsc::Sender<HandleMsg>,
}

impl MenuHandle for LinuxMenuHandle {
    fn set_icon(&self, png_bytes: &[u8]) -> Result<()> {
        self.tx
            .send(HandleMsg::SetIcon(png_bytes.to_vec()))
            .map_err(|_| anyhow::anyhow!("Linux menu event loop is not running"))
    }
    fn set_title(&self, title: &str) -> Result<()> {
        self.tx
            .send(HandleMsg::SetTitle(title.to_string()))
            .map_err(|_| anyhow::anyhow!("Linux menu event loop is not running"))
    }
    fn set_menu(&self, menu: MenuTree) -> Result<()> {
        self.tx
            .send(HandleMsg::SetMenu(menu))
            .map_err(|_| anyhow::anyhow!("Linux menu event loop is not running"))
    }
}

impl MenuBackend for LinuxMenu {
    fn create_status_item(
        &self,
        initial_title: &str,
        initial_icon: &[u8],
    ) -> Result<Box<dyn MenuHandle>> {
        self.ensure_gtk_init()?;
        let icon = decode_tray_icon(initial_icon)?;
        let tray = tray_icon::TrayIconBuilder::new()
            .with_title(initial_title)
            // tray-icon's own Linux note: "the icon won't be visible unless
            // a menu is set. Setting an empty Menu is enough." The real menu
            // arrives via the first `set_menu` call.
            .with_menu(Box::new(tray_icon::menu::Menu::new()))
            .with_icon(icon)
            .build()
            .map_err(|e| anyhow::anyhow!("failed to create Linux tray icon: {e}"))?;
        let (tx, rx) = std::sync::mpsc::channel();
        TRAY_STATE.with(|slot| {
            *slot.borrow_mut() = Some(TrayState { tray, rx });
        });
        Ok(Box::new(LinuxMenuHandle { tx }))
    }

    fn on_click(&self, cb: Box<dyn Fn(&str) + Send + Sync + 'static>) -> Result<()> {
        *self.click_cb.lock().unwrap() = Some(cb);
        Ok(())
    }

    fn run_event_loop(&self) -> Result<()> {
        // A `request_quit()` that arrived before this call (main_loop was
        // still None, so request_quit had nowhere to route the quit) set
        // this flag instead of silently discarding the request. Honor it now
        // by returning immediately, without pumping a GTK loop the caller
        // already wants stopped.
        if self
            .should_quit_early
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        self.ensure_gtk_init()?;
        let main_loop = gtk::glib::MainLoop::new(None, false);
        *self.main_loop.lock().unwrap() = Some(main_loop.clone());

        let click_cb = Arc::clone(&self.click_cb);
        gtk::glib::source::timeout_add_local(std::time::Duration::from_millis(100), move || {
            // Menu-item clicks arrive on tray-icon's own global channel.
            while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                if let Some(cb) = click_cb.lock().unwrap().as_ref() {
                    cb(&event.id.0);
                }
            }
            // Apply any pending `LinuxMenuHandle` mutations to the
            // thread-local tray — see the `LinuxMenu` doc comment for why
            // this can't just be a struct field.
            TRAY_STATE.with(|slot| {
                if let Some(state) = slot.borrow_mut().as_mut() {
                    while let Ok(msg) = state.rx.try_recv() {
                        apply_handle_msg(&state.tray, msg);
                    }
                }
            });
            gtk::glib::ControlFlow::Continue
        });

        main_loop.run();
        Ok(())
    }

    fn request_quit(&self) {
        if let Some(ml) = self.main_loop.lock().unwrap().as_ref() {
            ml.quit();
        } else {
            // run_event_loop hasn't populated main_loop yet — remember the
            // request so run_event_loop can honor it on entry instead of
            // this call being a silent no-op.
            self.should_quit_early
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal launcher — `usagio start` / `continue` from the tray
// ---------------------------------------------------------------------------

/// Priority order after `$TERMINAL`. `xterm` last as the near-universal
/// fallback every X11 install ships.
const TERMINAL_CANDIDATES: &[&str] = &[
    "gnome-terminal",
    "konsole",
    "xfce4-terminal",
    "kitty",
    "alacritty",
    "xterm",
];

/// Pick the terminal emulator to run `claude` in: `$TERMINAL` first (if set
/// and found), then the first of `TERMINAL_CANDIDATES` present on `$PATH`.
/// `exists` is injected so tests can simulate specific binaries being
/// present/absent without touching the real `$PATH`.
fn find_terminal_with(env_terminal: Option<&str>, exists: impl Fn(&str) -> bool) -> Option<String> {
    if let Some(t) = env_terminal {
        if !t.is_empty() && exists(t) {
            return Some(t.to_string());
        }
    }
    TERMINAL_CANDIDATES
        .iter()
        .find(|c| exists(c))
        .map(|s| s.to_string())
}

fn binary_exists_on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(bin).is_file())
}

/// Probe for a terminal emulator and launch `claude` inside it, logging
/// which one was picked (or that none was found) so a user report of
/// "nothing happened" is diagnosable. Not wired into the `Platform` trait —
/// see the module doc at the top of this file.
pub fn launch_claude_in_terminal() -> Result<()> {
    let env_terminal = std::env::var("TERMINAL").ok();
    let Some(term) = find_terminal_with(env_terminal.as_deref(), binary_exists_on_path) else {
        bail!(
            "no terminal emulator found (checked $TERMINAL, gnome-terminal, konsole, \
             xfce4-terminal, kitty, alacritty, xterm) — install one or set $TERMINAL"
        );
    };
    crate::logging::log(&format!("linux: launching claude via terminal `{term}`"));
    Command::new(&term)
        .args(["-e", "claude"])
        .spawn()
        .with_context(|| format!("spawning `{term} -e claude`"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Paths ---------------------------------------------------------

    #[test]
    fn paths_config_dir_respects_xdg_config_home() {
        crate::env_lock::scoped_env_var(
            "XDG_CONFIG_HOME",
            Some("/tmp/linux-platform-test-xdg"),
            || {
                let got = LinuxPaths.config_dir("usagio");
                assert_eq!(got, PathBuf::from("/tmp/linux-platform-test-xdg/usagio"));
            },
        );
    }

    #[test]
    fn paths_cache_dir_respects_xdg_cache_home() {
        crate::env_lock::scoped_env_var(
            "XDG_CACHE_HOME",
            Some("/tmp/linux-platform-test-cache"),
            || {
                let got = LinuxPaths.cache_dir("usagio");
                assert_eq!(got, PathBuf::from("/tmp/linux-platform-test-cache/usagio"));
            },
        );
    }

    // --- Autostart -------------------------------------------------------

    #[test]
    fn linux_autostart_writes_valid_desktop_file() {
        let tmp = tempfile::tempdir().unwrap();
        crate::env_lock::scoped_env_var(
            "XDG_CONFIG_HOME",
            Some(tmp.path().to_str().unwrap()),
            || {
                let a = LinuxAutostart;
                a.install(
                    "com.mattjackson.usagio.menubar",
                    Path::new("/usr/local/bin/usagio"),
                    &["menubar"],
                )
                .expect("install");

                let path = tmp.path().join("autostart").join("usagio.desktop");
                let contents = std::fs::read_to_string(&path).expect("desktop file written");
                assert!(
                    contents.starts_with("[Desktop Entry]"),
                    "missing header:\n{contents}"
                );
                for expected in [
                    "Type=Application",
                    "Name=usagio",
                    "Exec=/usr/local/bin/usagio menubar",
                    "Terminal=false",
                    "Hidden=false",
                    "X-GNOME-Autostart-enabled=true",
                ] {
                    assert!(
                        contents.contains(expected),
                        "missing `{expected}` in:\n{contents}"
                    );
                }
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o644);

                assert!(a.is_installed("com.mattjackson.usagio.menubar").unwrap());
                a.uninstall("com.mattjackson.usagio.menubar").unwrap();
                assert!(!a.is_installed("com.mattjackson.usagio.menubar").unwrap());
            },
        );
    }

    #[test]
    fn desktop_exec_quote_leaves_simple_args_unquoted() {
        assert_eq!(
            desktop_exec_quote("/usr/local/bin/usagio"),
            "/usr/local/bin/usagio"
        );
        assert_eq!(desktop_exec_quote("menubar"), "menubar");
    }

    #[test]
    fn desktop_exec_quote_wraps_and_escapes_special_chars() {
        assert_eq!(
            desktop_exec_quote("/path with spaces/usagio"),
            "\"/path with spaces/usagio\""
        );
        assert_eq!(desktop_exec_quote("has\"quote"), "\"has\\\"quote\"");
    }

    // --- Secrets: offline fallback ---------------------------------------

    #[test]
    fn linux_secrets_falls_back_to_encrypted_file_when_no_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let no_daemon = || {
            keyring::Error::NoStorageAccess(Box::new(std::io::Error::other(
                "no D-Bus session / Secret Service daemon",
            )))
        };

        set_impl(
            || Err(no_daemon()),
            tmp.path(),
            "claude-usage",
            "matt@example.com",
            "s3cret-token",
        )
        .expect("set falls back to file");

        let path = tmp.path().join("secrets.json");
        assert!(path.exists(), "fallback file was not written");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "fallback file must be chmod 0600");

        let got = get_impl(
            || Err(no_daemon()),
            tmp.path(),
            "claude-usage",
            "matt@example.com",
        )
        .expect("get falls back to file");
        assert_eq!(got.as_deref(), Some("s3cret-token"));

        let listed = fallback_list(tmp.path(), "claude-usage").unwrap();
        assert_eq!(listed, vec!["matt@example.com".to_string()]);

        delete_impl(
            || Err(no_daemon()),
            tmp.path(),
            "claude-usage",
            "matt@example.com",
        )
        .expect("delete falls back to file");
        let after = get_impl(
            || Err(no_daemon()),
            tmp.path(),
            "claude-usage",
            "matt@example.com",
        )
        .unwrap();
        assert!(
            after.is_none(),
            "secret still present after fallback delete"
        );
    }

    #[test]
    fn linux_secrets_missing_entry_is_not_treated_as_daemon_down() {
        // `NoEntry` still needs to check the fallback file (an account may
        // have been written there during a prior daemon outage) but must not
        // be reported as an error.
        let tmp = tempfile::tempdir().unwrap();
        let got = get_impl(
            || Err(keyring::Error::NoEntry),
            tmp.path(),
            "claude-usage",
            "nobody@example.com",
        )
        .expect("NoEntry is not an error");
        assert!(got.is_none());
    }

    #[test]
    fn linux_secrets_real_error_is_not_swallowed() {
        let tmp = tempfile::tempdir().unwrap();
        let err = get_impl(
            || Err(keyring::Error::TooLong("account".into(), 255)),
            tmp.path(),
            "claude-usage",
            "matt@example.com",
        );
        assert!(
            err.is_err(),
            "a real keyring error must surface, not fall back silently"
        );
    }

    // --- Terminal probe ----------------------------------------------------

    #[test]
    fn linux_terminal_probe_falls_through_missing_binaries() {
        // gnome-terminal, konsole, xfce4-terminal all "missing"; kitty found.
        let exists = |bin: &str| bin == "kitty";
        assert_eq!(find_terminal_with(None, exists).as_deref(), Some("kitty"));
    }

    #[test]
    fn linux_terminal_probe_prefers_dollar_terminal_env() {
        let exists = |bin: &str| bin == "foot" || bin == "xterm";
        assert_eq!(
            find_terminal_with(Some("foot"), exists).as_deref(),
            Some("foot")
        );
    }

    #[test]
    fn linux_terminal_probe_ignores_dollar_terminal_when_missing() {
        let exists = |bin: &str| bin == "xterm";
        // $TERMINAL says "foot" but it isn't installed — fall through to the
        // priority list instead of failing outright.
        assert_eq!(
            find_terminal_with(Some("foot"), exists).as_deref(),
            Some("xterm")
        );
    }

    #[test]
    fn linux_terminal_probe_returns_none_when_nothing_found() {
        let exists = |_: &str| false;
        assert_eq!(find_terminal_with(None, exists), None);
    }

    // --- M8: request_quit() before run_event_loop() must not be a no-op ----

    #[test]
    fn request_quit_before_run_event_loop_is_honored() {
        let menu = LinuxMenu::new();
        // No main_loop exists yet — a naive request_quit would have nowhere
        // to route this and would silently discard it.
        menu.request_quit();
        assert!(
            menu.should_quit_early
                .load(std::sync::atomic::Ordering::SeqCst),
            "request_quit before run_event_loop must set should_quit_early"
        );

        // run_event_loop must see the flag and return immediately, without
        // touching GTK (which would need a display in CI).
        menu.run_event_loop()
            .expect("an early quit request must make run_event_loop return Ok immediately");
        assert!(
            !menu
                .should_quit_early
                .load(std::sync::atomic::Ordering::SeqCst),
            "the flag must be consumed, not left set for a future real run"
        );
    }
}
