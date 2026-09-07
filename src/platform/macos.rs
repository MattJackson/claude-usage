//! macOS platform impl.
//!
//! Deliberately keeps the existing subprocess contracts:
//! - Keychain via `security(1)` — no keychain-access FFI dep, avoids the
//!   launch-time keychain-prompt dialog the Rust `keychain-services` crate
//!   would trigger. See the note in the historical `keychain_write` helper
//!   in `main.rs`: SecItem access from an unsigned brew-installed binary
//!   makes macOS prompt on every launch (no stable identity for
//!   "Always Allow"). The CLI path doesn't prompt.
//! - Autostart via `launchctl` + a plist written to `~/Library/LaunchAgents/`.
//! - Menu backend delegates to the existing native NSMenu code in
//!   `crate::menubar` — this file exposes it via the `MenuBackend` trait
//!   without pulling the objc2 stack into the trait signatures. The trait
//!   impl is a placeholder for now (the existing free-function menu is still
//!   invoked directly from `main.rs`); the real trait-based rewrite lands in
//!   a follow-up commit per `platform/MIGRATION-DRAFT.md` step B.
//!
//! Copied surface from the pre-Platform-trait code in `src/main.rs` and
//! `src/providers/claude/mod.rs`. Preserve behavior bit-for-bit.

use super::*;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct MacOsPlatform {
    menu: MacOsMenu,
    secrets: MacOsSecrets,
    autostart: MacOsAutostart,
    paths: MacOsPaths,
}

impl MacOsPlatform {
    pub fn new() -> Self {
        Self {
            menu: MacOsMenu::default(),
            secrets: MacOsSecrets,
            autostart: MacOsAutostart,
            paths: MacOsPaths,
        }
    }
}

impl Platform for MacOsPlatform {
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
        "macOS"
    }
}

// ---- MenuBackend ---------------------------------------------------------

/// Placeholder MenuBackend impl. `crate::menubar` still owns the native NSMenu
/// loop and is invoked directly from `main.rs`; every trait method here bails
/// so a stray call surfaces a clear error instead of silently returning junk.
/// The real trait-based rewrite lands in a follow-up commit — see
/// `platform/MIGRATION-DRAFT.md` step B.
#[derive(Default)]
pub struct MacOsMenu {}

impl MenuBackend for MacOsMenu {
    fn create_status_item(
        &self,
        _initial_title: &str,
        _initial_icon: &[u8],
    ) -> Result<Box<dyn MenuHandle>> {
        bail!("MacOsMenu::create_status_item not yet wired — menubar.rs is still invoked directly")
    }
    fn on_click(&self, _cb: Box<dyn Fn(&str) + Send + Sync + 'static>) -> Result<()> {
        bail!("MacOsMenu::on_click not yet wired")
    }
    fn run_event_loop(&self) -> Result<()> {
        bail!("MacOsMenu::run_event_loop not yet wired")
    }
    fn request_quit(&self) {}
}

// ---- SecretStore ---------------------------------------------------------

pub struct MacOsSecrets;

impl SecretStore for MacOsSecrets {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        let out = Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .output()
            .context("running `security find-generic-password`")?;
        if !out.status.success() {
            return Ok(None);
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            Ok(None)
        } else {
            Ok(Some(s))
        }
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        let status = Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                service,
                "-a",
                account,
                "-w",
                secret,
            ])
            .status()
            .context("running `security add-generic-password`")?;
        if !status.success() {
            bail!("`security add-generic-password` failed for service={service} account={account}");
        }
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let status = Command::new("security")
            .args(["delete-generic-password", "-s", service, "-a", account])
            .status()
            .context("running `security delete-generic-password`")?;
        if !status.success() {
            // Not-found is not an error for our callers.
            return Ok(());
        }
        Ok(())
    }

    fn list(&self, _service: &str) -> Result<Vec<String>> {
        // `security dump-keychain -a` enumerates but is heavy and prompts.
        // Callers today track accounts via state.json; this stays empty
        // until a real caller needs it.
        Ok(Vec::new())
    }
}

// ---- Autostart -----------------------------------------------------------

pub struct MacOsAutostart;

impl MacOsAutostart {
    fn plist_path(label: &str) -> Result<PathBuf> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{label}.plist")))
    }
}

impl Autostart for MacOsAutostart {
    fn install(&self, label: &str, binary: &Path, args: &[&str]) -> Result<()> {
        let mut prog_args = format!("    <string>{}</string>\n", binary.display());
        for a in args {
            prog_args.push_str(&format!("    <string>{a}</string>\n"));
        }
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{prog_args}  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
</dict>
</plist>
"#
        );
        let path = Self::plist_path(label)?;
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&path, plist).context("writing LaunchAgent plist")?;

        // Unload first to allow reload without a stale process holding the label.
        let _ = Command::new("launchctl")
            .args(["unload", &path.to_string_lossy()])
            .status();
        let status = Command::new("launchctl")
            .args(["load", "-w", &path.to_string_lossy()])
            .status()
            .context("launchctl load")?;
        if !status.success() {
            bail!("launchctl load failed for {}", path.display());
        }
        Ok(())
    }

    fn uninstall(&self, label: &str) -> Result<()> {
        let path = Self::plist_path(label)?;
        let _ = Command::new("launchctl")
            .args(["unload", &path.to_string_lossy()])
            .status();
        if path.exists() {
            std::fs::remove_file(&path).context("removing plist")?;
        }
        Ok(())
    }

    fn is_installed(&self, label: &str) -> Result<bool> {
        Ok(Self::plist_path(label)?.exists())
    }

    fn restart(&self, label: &str) -> Result<()> {
        // launchctl kickstart -k restarts the currently-loaded job in place.
        // Requires gui/<uid>/<label> domain. Existing code uses this pattern
        // for the hot-swap on brew upgrade.
        let uid = unsafe { libc::getuid() };
        let target = format!("gui/{uid}/{label}");
        let status = Command::new("launchctl")
            .args(["kickstart", "-k", &target])
            .status()
            .context("launchctl kickstart")?;
        if !status.success() {
            bail!("launchctl kickstart failed for {target}");
        }
        Ok(())
    }
}

// ---- Paths ---------------------------------------------------------------

pub struct MacOsPaths;

impl Paths for MacOsPaths {
    fn config_dir(&self, app: &str) -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join(".config").join(app)
    }
    fn data_dir(&self, app: &str) -> PathBuf {
        self.config_dir(app)
    }
    fn log_dir(&self, app: &str) -> PathBuf {
        self.config_dir(app)
    }
    fn cache_dir(&self, app: &str) -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join("Library").join("Caches").join(app)
    }
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_config_dir_uses_home_dot_config() {
        // Snapshot + restore HOME so the process-global env change stays local
        // to this test. cargo test threads share HOME, but we only assert on
        // the value we set here, so a racing sibling test that reads HOME sees
        // its own snapshot.
        let orig = std::env::var_os("HOME");
        // SAFETY: single-threaded env mutation, restored on exit.
        std::env::set_var("HOME", "/tmp/platform-test-home");
        let got = MacOsPaths.config_dir("claude-usage");
        assert_eq!(
            got,
            std::path::PathBuf::from("/tmp/platform-test-home/.config/claude-usage")
        );
        let cache = MacOsPaths.cache_dir("claude-usage");
        assert_eq!(
            cache,
            std::path::PathBuf::from("/tmp/platform-test-home/Library/Caches/claude-usage")
        );
        match orig {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    /// SecretStore round-trip against the real login keychain. `#[ignore]`d by
    /// default so `cargo test` doesn't touch the developer's keychain — run
    /// explicitly with `cargo test -- --ignored secret_store_roundtrip`.
    /// Uses a per-run unique service name and cleans up on both success and
    /// failure paths.
    #[test]
    #[ignore = "touches the real login keychain; run with --ignored"]
    fn secret_store_roundtrip() {
        let service = format!(
            "claude-usage-platform-test-{}",
            std::process::id()
        );
        let account = "roundtrip";
        let secret = "hunter2";
        let ss = MacOsSecrets;

        // Ensure a clean starting point.
        let _ = ss.delete(&service, account);

        ss.set(&service, account, secret).expect("set");
        let got = ss.get(&service, account).expect("get");
        assert_eq!(got.as_deref(), Some(secret));

        ss.delete(&service, account).expect("delete");
        let after = ss.get(&service, account).expect("get after delete");
        assert!(after.is_none(), "secret still present after delete");
    }
}
