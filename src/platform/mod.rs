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

// Trait / MenuTree / MenuItem / MenuBackend / list / delete / etc. are
// v0.5.0 scaffolding for Linux (ksni) + Windows (Win32) menu backends —
// wired when those platforms land. macOS still drives the NSMenu path
// directly through `crate::menubar`. File-level allow so the shape stays
// reviewed as a whole rather than being pruned one method at a time.
#![allow(dead_code)]

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
    /// Restrict `path` to owner-only access: `0700` for a directory, `0600`
    /// for a file. Unix backends `chmod` based on the path's file type.
    /// Windows has no chmod equivalent worth forging here — `%APPDATA%` /
    /// `%LOCALAPPDATA%` are already per-user directories protected by the
    /// user's NTFS ACL, so this is a documented no-op on that backend rather
    /// than a scattered `#[cfg(unix)]` at every call site. `path` must exist.
    fn secure_permissions(&self, path: &Path) -> Result<()>;
    /// Native save/open panels for the "Advanced ▸ Backups ▸ Save…/Restore…"
    /// menu flow. Default-implemented against `rfd` (Win32
    /// IFileOpenDialog/IFileSaveDialog on Windows, NSOpenPanel/NSSavePanel on
    /// macOS, xdg-desktop-portal on Linux) via the process-wide
    /// [`RfdFileDialog`] — `rfd` already backs every target from one
    /// cross-platform call, so there's no per-OS behavior to fork here the
    /// way there is for menu/secrets/autostart/paths. Kept as a trait method
    /// (rather than a bare free function `menubar.rs` calls directly) so
    /// tests can substitute a capturing mock instead of popping a real
    /// native dialog — see `handle_backup_save_with` /
    /// `handle_backup_restore_dialog_with` in `crate::menubar`.
    fn file_dialog(&self) -> &dyn FileDialog {
        &RfdFileDialog
    }
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

/// **Threading invariant**: `create_status_item` and `run_event_loop` MUST
/// be called on the same OS thread. Windows enforces this at runtime via a
/// thread-id check in `TrayState` (see `windows.rs`); Linux relies on a
/// `thread_local!` that silently drops mutations if violated (see
/// `LinuxMenu` in `linux.rs`). Callers responsible for wiring the menu
/// backend must arrange the main-thread call sequence documented in
/// `menubar.rs::run`.
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
/// namespace string (e.g. `"claude-usage"` — kept frozen post-rename to
/// preserve existing tokens); account is a per-user identifier (e.g.
/// `"matt@example.com"` or `$USER`).
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
/// (`com.mattjackson.usagio.menubar`) reused across install / uninstall.
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
/// Naming convention: pass the app slug (`"usagio"`) as `app` so the rename
/// doesn't ripple through every callsite. Legacy code paths that still
/// resolve the old `"claude-usage"` directory live in
/// `crate::paths::migrate_config_dir_if_needed`.
pub trait Paths: Send + Sync {
    fn config_dir(&self, app: &str) -> PathBuf;
    fn data_dir(&self, app: &str) -> PathBuf;
    fn log_dir(&self, app: &str) -> PathBuf;
    fn cache_dir(&self, app: &str) -> PathBuf;
}

// ---------- FileDialog -----------------------------------------------------

/// Native file-open/save panels. See `Platform::file_dialog` for why this
/// isn't forked per-OS the way menu/secrets/autostart/paths are.
///
/// No `Send + Sync` supertrait bound (unlike the other backend traits): every
/// production impl (`RfdFileDialog`) is a stateless unit struct held by value
/// inside a `Platform` impl (never boxed as a trait object field), and the
/// test mock (`MockFileDialog`) intentionally uses `RefCell` — a `Sync`
/// requirement here would forbid that without buying anything, since nothing
/// stores `Box<dyn FileDialog>` behind a shared reference across threads.
pub trait FileDialog {
    /// Open a SAVE panel defaulting to `default_name` inside `default_dir`
    /// (falls back to the OS's normal default location if `None`). Returns
    /// `None` if the user cancels.
    fn save_file(&self, default_name: &str, default_dir: Option<&Path>) -> Option<PathBuf>;
    /// Open an OPEN panel defaulting to `default_dir`, filtered to
    /// `*.json` (the only caller today is the state-file restore flow).
    /// Returns `None` if the user cancels.
    fn pick_file(&self, default_dir: Option<&Path>) -> Option<PathBuf>;
}

/// `rfd`-backed `FileDialog`. Stateless — `rfd::FileDialog` is built fresh
/// per call, so this is a zero-sized marker type wired into every `Platform`
/// impl via the trait's default method.
pub struct RfdFileDialog;

impl FileDialog for RfdFileDialog {
    fn save_file(&self, default_name: &str, default_dir: Option<&Path>) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new().set_file_name(default_name);
        if let Some(dir) = default_dir {
            dialog = dialog.set_directory(dir);
        }
        dialog.save_file()
    }

    fn pick_file(&self, default_dir: Option<&Path>) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new().add_filter("usagio state (*.json)", &["json"]);
        if let Some(dir) = default_dir {
            dialog = dialog.set_directory(dir);
        }
        dialog.pick_file()
    }
}

/// Test-only capturing mock for [`FileDialog`], shared by any module that
/// needs to assert "the Save…/Restore… click handler called `FileDialog`
/// with X arguments" without popping a real native panel. `pub(crate)` so
/// `crate::menubar`'s `#[cfg(test)]` module can reach it.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockFileDialog {
    pub save_file_calls: std::cell::RefCell<Vec<(String, Option<PathBuf>)>>,
    pub save_file_returns: std::cell::RefCell<Option<PathBuf>>,
    pub pick_file_calls: std::cell::RefCell<Vec<Option<PathBuf>>>,
    pub pick_file_returns: std::cell::RefCell<Option<PathBuf>>,
}

#[cfg(test)]
impl FileDialog for MockFileDialog {
    fn save_file(&self, default_name: &str, default_dir: Option<&Path>) -> Option<PathBuf> {
        self.save_file_calls
            .borrow_mut()
            .push((default_name.to_string(), default_dir.map(Path::to_path_buf)));
        self.save_file_returns.borrow_mut().take()
    }

    fn pick_file(&self, default_dir: Option<&Path>) -> Option<PathBuf> {
        self.pick_file_calls
            .borrow_mut()
            .push(default_dir.map(Path::to_path_buf));
        self.pick_file_returns.borrow_mut().take()
    }
}

// ---------- current() -----------------------------------------------------

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Shared Unix chmod backing `Platform::secure_permissions` on both macOS and
/// Linux: `0700` for a directory, `0600` for a file, based on the path's
/// actual file type rather than a caller-supplied mode. Lives here (rather
/// than in `macos.rs`) because macOS and Linux are mutually-exclusive
/// `cfg(target_os = ...)` modules that never compile together — a helper
/// defined in one wouldn't be visible from the other.
#[cfg(unix)]
fn secure_permissions_unix(path: &Path) -> Result<()> {
    use anyhow::Context;
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)
        .with_context(|| format!("stat {} for secure_permissions", path.display()))?;
    let mode = if meta.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {} for secure_permissions", path.display()))
}

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

// ---------------------------------------------------------------------------
// secure_permissions_unix — the only guard on secret-file permissions on
// macOS + Linux. Pinned so a refactor that quietly stops chmod'ing (e.g.
// "simplify" to always return Ok) is caught by a red test, not by an
// incident.
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod tests {
    use super::secure_permissions_unix;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn secure_permissions_unix_chmods_regular_file_to_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, b"shh").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        secure_permissions_unix(&path).expect("chmod should succeed on an existing file");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "file mode should be exactly 0600");
    }

    #[test]
    fn secure_permissions_unix_chmods_directory_to_0700() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("secretdir");
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        secure_permissions_unix(&path).expect("chmod should succeed on an existing dir");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "dir mode should be exactly 0700");
    }

    #[test]
    fn secure_permissions_unix_errors_on_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");

        // Pinning current behavior: stat fails before any chmod is attempted,
        // so a missing path is a loud error, not a silent no-op success.
        let err = secure_permissions_unix(&missing)
            .expect_err("a missing path must not be treated as already-secure");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("secure_permissions"),
            "error should identify the operation: {msg}"
        );
    }
}
