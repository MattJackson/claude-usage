//! Linux platform impl — skeleton only.
//!
//! Every method is `unimplemented!()` behind a `cfg(target_os = "linux")`
//! guard so cross-compile targets link cleanly. Real impls (ksni tray,
//! secret-service, XDG autostart, XDG paths) land in a follow-up commit
//! once the macOS impl has proven the trait shape on real users. See
//! `platform/MIGRATION-DRAFT.md` for the planned surface.

use super::*;
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct LinuxPlatform {
    menu: LinuxMenu,
    secrets: LinuxSecrets,
    autostart: LinuxAutostart,
    paths: LinuxPaths,
}

impl LinuxPlatform {
    pub fn new() -> Self {
        Self {
            menu: LinuxMenu,
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
}

pub struct LinuxMenu;

impl MenuBackend for LinuxMenu {
    fn create_status_item(&self, _title: &str, _icon: &[u8]) -> Result<Box<dyn MenuHandle>> {
        unimplemented!("LinuxMenu::create_status_item — ksni tray impl lands in a follow-up")
    }
    fn on_click(&self, _cb: Box<dyn Fn(&str) + Send + Sync + 'static>) -> Result<()> {
        unimplemented!("LinuxMenu::on_click")
    }
    fn run_event_loop(&self) -> Result<()> {
        unimplemented!("LinuxMenu::run_event_loop")
    }
    fn request_quit(&self) {
        unimplemented!("LinuxMenu::request_quit")
    }
}

pub struct LinuxSecrets;

impl SecretStore for LinuxSecrets {
    fn get(&self, _service: &str, _account: &str) -> Result<Option<String>> {
        unimplemented!("LinuxSecrets::get — secret-service impl lands in a follow-up")
    }
    fn set(&self, _service: &str, _account: &str, _secret: &str) -> Result<()> {
        unimplemented!("LinuxSecrets::set")
    }
    fn delete(&self, _service: &str, _account: &str) -> Result<()> {
        unimplemented!("LinuxSecrets::delete")
    }
    fn list(&self, _service: &str) -> Result<Vec<String>> {
        unimplemented!("LinuxSecrets::list")
    }
}

pub struct LinuxAutostart;

impl Autostart for LinuxAutostart {
    fn install(&self, _label: &str, _binary: &Path, _args: &[&str]) -> Result<()> {
        unimplemented!("LinuxAutostart::install — XDG autostart impl lands in a follow-up")
    }
    fn uninstall(&self, _label: &str) -> Result<()> {
        unimplemented!("LinuxAutostart::uninstall")
    }
    fn is_installed(&self, _label: &str) -> Result<bool> {
        unimplemented!("LinuxAutostart::is_installed")
    }
    fn restart(&self, _label: &str) -> Result<()> {
        unimplemented!("LinuxAutostart::restart")
    }
}

pub struct LinuxPaths;

impl Paths for LinuxPaths {
    fn config_dir(&self, _app: &str) -> PathBuf {
        unimplemented!("LinuxPaths::config_dir — XDG lookup lands in a follow-up")
    }
    fn data_dir(&self, _app: &str) -> PathBuf {
        unimplemented!("LinuxPaths::data_dir")
    }
    fn log_dir(&self, _app: &str) -> PathBuf {
        unimplemented!("LinuxPaths::log_dir")
    }
    fn cache_dir(&self, _app: &str) -> PathBuf {
        unimplemented!("LinuxPaths::cache_dir")
    }
}
