//! Windows platform impl — skeleton only.
//!
//! Every method is `unimplemented!()` behind a `cfg(target_os = "windows")`
//! guard so cross-compile targets link cleanly. Real impls (tray-icon +
//! muda, Credential Manager, registry Run key, APPDATA / LOCALAPPDATA
//! paths) land in a follow-up commit once the macOS impl has proven the
//! trait shape on real users. See `platform/MIGRATION-DRAFT.md` for the
//! planned surface.

use super::*;
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct WindowsPlatform {
    menu: WindowsMenu,
    secrets: WindowsSecrets,
    autostart: WindowsAutostart,
    paths: WindowsPaths,
}

impl WindowsPlatform {
    pub fn new() -> Self {
        Self {
            menu: WindowsMenu,
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
}

pub struct WindowsMenu;

impl MenuBackend for WindowsMenu {
    fn create_status_item(&self, _title: &str, _icon: &[u8]) -> Result<Box<dyn MenuHandle>> {
        unimplemented!("WindowsMenu::create_status_item — tray-icon + muda impl lands in a follow-up")
    }
    fn on_click(&self, _cb: Box<dyn Fn(&str) + Send + Sync + 'static>) -> Result<()> {
        unimplemented!("WindowsMenu::on_click")
    }
    fn run_event_loop(&self) -> Result<()> {
        unimplemented!("WindowsMenu::run_event_loop")
    }
    fn request_quit(&self) {
        unimplemented!("WindowsMenu::request_quit")
    }
}

pub struct WindowsSecrets;

impl SecretStore for WindowsSecrets {
    fn get(&self, _service: &str, _account: &str) -> Result<Option<String>> {
        unimplemented!("WindowsSecrets::get — Credential Manager impl lands in a follow-up")
    }
    fn set(&self, _service: &str, _account: &str, _secret: &str) -> Result<()> {
        unimplemented!("WindowsSecrets::set")
    }
    fn delete(&self, _service: &str, _account: &str) -> Result<()> {
        unimplemented!("WindowsSecrets::delete")
    }
    fn list(&self, _service: &str) -> Result<Vec<String>> {
        unimplemented!("WindowsSecrets::list")
    }
}

pub struct WindowsAutostart;

impl Autostart for WindowsAutostart {
    fn install(&self, _label: &str, _binary: &Path, _args: &[&str]) -> Result<()> {
        unimplemented!("WindowsAutostart::install — registry Run key impl lands in a follow-up")
    }
    fn uninstall(&self, _label: &str) -> Result<()> {
        unimplemented!("WindowsAutostart::uninstall")
    }
    fn is_installed(&self, _label: &str) -> Result<bool> {
        unimplemented!("WindowsAutostart::is_installed")
    }
    fn restart(&self, _label: &str) -> Result<()> {
        unimplemented!("WindowsAutostart::restart")
    }
}

pub struct WindowsPaths;

impl Paths for WindowsPaths {
    fn config_dir(&self, _app: &str) -> PathBuf {
        unimplemented!("WindowsPaths::config_dir — APPDATA lookup lands in a follow-up")
    }
    fn data_dir(&self, _app: &str) -> PathBuf {
        unimplemented!("WindowsPaths::data_dir")
    }
    fn log_dir(&self, _app: &str) -> PathBuf {
        unimplemented!("WindowsPaths::log_dir")
    }
    fn cache_dir(&self, _app: &str) -> PathBuf {
        unimplemented!("WindowsPaths::cache_dir")
    }
}
