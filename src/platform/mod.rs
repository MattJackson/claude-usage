//! Platform abstraction.
//!
//! All host-OS integration (menu bar, credential store, autostart daemon,
//! filesystem paths) hides behind these four traits. `platform::current()`
//! returns the correct `Box<dyn Platform>` at process start; the rest of the
//! crate calls trait methods and never sees `#[cfg(target_os = ...)]`.
//!
//! Sync-only: all methods block. The menu backend event loop runs on the main
//! thread; secret / autostart / paths are called synchronously off worker
//! threads under short locks. If a backend needs async internally (D-Bus on
//! Linux, Win32 dispatch on Windows), it should use `block_on` inside the impl
//! rather than infecting the trait surface.

use std::path::{Path, PathBuf};

pub type Result<T> = anyhow::Result<T>;

/// Root platform facade. `current()` returns the concrete impl for this target.
pub trait Platform: Send + Sync + 'static {
    fn menu(&self) -> &dyn MenuBackend;
    fn secrets(&self) -> &dyn SecretStore;
    fn autostart(&self) -> &dyn Autostart;
    fn paths(&self) -> &dyn Paths;
    /// Human name used in error messages / diagnostics.
    fn os_display_name(&self) -> &'static str;
}

// ---------- MenuBackend ---------------------------------------------------

/// Opaque handle to a live status-bar item / tray icon. Dropping it removes
/// the item from the bar.
pub trait MenuHandle: Send {
    /// Replace the icon (16pt template PNG bytes on macOS; ICO/BMP on Windows;
    /// PNG on Linux via ksni).
    fn set_icon(&self, png_bytes: &[u8]) -> Result<()>;
    /// Replace the tooltip / title shown next to the icon.
    fn set_title(&self, title: &str) -> Result<()>;
    /// Replace the dropdown menu tree.
    fn set_menu(&self, menu: MenuTree) -> Result<()>;
}

/// Provider-agnostic dropdown tree. The backend translates to NSMenu / muda /
/// ksni. Kept small and imperative so backends have room to render natively.
#[derive(Debug, Clone)]
pub struct MenuTree {
    pub items: Vec<MenuItem>,
}

#[derive(Debug, Clone)]
pub enum MenuItem {
    /// Clickable row. `id` is opaque to the backend; dispatched back via
    /// `MenuBackend::on_click`.
    Action {
        id: String,
        label: String,
        icon_png: Option<Vec<u8>>, // 16pt template
        enabled: bool,
        checked: bool, // check mark
    },
    /// Non-clickable header / status row.
    Static {
        label: String,
        icon_png: Option<Vec<u8>>,
    },
    Separator,
    Submenu {
        label: String,
        icon_png: Option<Vec<u8>>,
        items: Vec<MenuItem>,
    },
}

pub trait MenuBackend: Send + Sync {
    /// Create the status-bar item. Called once at startup. Handle lives for
    /// the process lifetime; the backend owns the event loop.
    fn create_status_item(
        &self,
        initial_title: &str,
        initial_icon: &[u8],
    ) -> Result<Box<dyn MenuHandle>>;

    /// Register a click dispatch callback. Called from the menu backend's
    /// event thread with the `id` of the clicked `MenuItem::Action`.
    fn on_click(&self, cb: Box<dyn Fn(&str) + Send + Sync + 'static>) -> Result<()>;

    /// Run the platform event loop. Blocks. Called on the main thread. Never
    /// returns except on quit request (backend may return `Ok(())` cleanly
    /// when the user chooses Quit).
    fn run_event_loop(&self) -> Result<()>;

    /// Ask the event loop to exit cleanly (called from a click handler for
    /// the Quit menu item).
    fn request_quit(&self);
}

// ---------- SecretStore ---------------------------------------------------

/// Per-item secret storage keyed by `(service, account)`. Service is a stable
/// namespace string (e.g. `"claude-usage"`); account is a per-user
/// identifier (e.g. `"matt@example.com"` or `$USER`).
///
/// Kept CLI-shape (String in/out) so macOS keeps its `security(1)` subprocess
/// contract without a Rust FFI dep, and Linux / Windows can layer their
/// crates on top.
pub trait SecretStore: Send + Sync {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>>;
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()>;
    fn delete(&self, service: &str, account: &str) -> Result<()>;
    /// Enumerate account labels for a service. Empty vec if none. Some
    /// backends (macOS `security dump-keychain -a`) require heavier calls;
    /// implementations may return an empty vec + a documented caveat rather
    /// than a real enumeration until a caller needs it.
    fn list(&self, service: &str) -> Result<Vec<String>>;
}

// ---------- Autostart -----------------------------------------------------

/// Login-time launch registration. `label` is a reverse-DNS identifier
/// (`com.mattjackson.claude-usage`) reused across install / uninstall.
pub trait Autostart: Send + Sync {
    fn install(&self, label: &str, binary: &Path, args: &[&str]) -> Result<()>;
    fn uninstall(&self, label: &str) -> Result<()>;
    fn is_installed(&self, label: &str) -> Result<bool>;
    /// Stop the running instance and start the freshly-installed one. Used by
    /// `brew upgrade` hot-swap. Backends without a live-restart primitive
    /// (Windows registry Run key) may implement as uninstall+install+spawn.
    fn restart(&self, label: &str) -> Result<()>;
}

// ---------- Paths ---------------------------------------------------------

/// Canonical directories for the app's mutable state. Callers should use
/// these, never `~/.config/...` string literals.
///
/// Naming convention: pass the app slug (`"claude-usage"`, later `"usagio"`)
/// as `app` so the rename doesn't ripple through every callsite.
pub trait Paths: Send + Sync {
    fn config_dir(&self, app: &str) -> PathBuf;
    fn data_dir(&self, app: &str) -> PathBuf;
    fn log_dir(&self, app: &str) -> PathBuf;
    fn cache_dir(&self, app: &str) -> PathBuf;
}

// ---------- current() -----------------------------------------------------

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

/// Return the platform impl for this OS. Panics only in the impossible case
/// of an unsupported target that got past the cfg guard.
pub fn current() -> Box<dyn Platform> {
    #[cfg(target_os = "macos")]
    return Box::new(macos::MacOsPlatform::new());
    #[cfg(target_os = "linux")]
    return Box::new(linux::LinuxPlatform::new());
    #[cfg(target_os = "windows")]
    return Box::new(windows::WindowsPlatform::new());
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    compile_error!("unsupported target OS");
}
