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
    // Reachable through the `MenuBackend` trait method below; the compiler's
    // dead-code analysis doesn't see the vtable dispatch. The concrete impl
    // stays a placeholder (see comments below) until the trait-based rewrite
    // in v0.5.0.
    #[allow(dead_code)]
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

// ---------------------------------------------------------------------------
// Real (subprocess-backed) implementation. Lives in ordinary free functions —
// not gated on `cfg(not(test))` themselves — so the one `#[ignore]`d test
// below that deliberately exercises the real login keychain can still call
// them directly by name. Only the `SecretStore for MacOsSecrets` *trait impl*
// is compiled differently between test and non-test builds (see below); that
// split is what keeps `cargo test` from ever invoking `security(1)` through
// the trait-dispatched path every other caller in the crate uses
// (`platform().secrets()`).
// ---------------------------------------------------------------------------

fn real_get(service: &str, account: &str) -> Result<Option<String>> {
    // Only exit code 44 ("SecItem not found" per <Security/SecBase.h> /
    // `security(1)` conventions) is a genuine "not present". Any other
    // non-zero — keychain locked, permission denied, IPC failure —
    // surfaces as Err so callers like `sync_active_from_keychain` treat
    // it as "don't touch anything" rather than "assume gone" (which
    // would let a subsequent switch overwrite a still-valid token).
    // See H7 in the round-1 codeaudit findings.
    let out = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .context("running `security find-generic-password`")?;
    if !out.status.success() {
        match out.status.code() {
            Some(44) => return Ok(None),
            other => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                bail!(
                    "`security find-generic-password` failed \
                     (exit {other:?}) for service={service} account={account}: {}",
                    stderr.trim(),
                );
            }
        }
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        Ok(None)
    } else {
        Ok(Some(s))
    }
}

fn real_set(service: &str, account: &str, secret: &str) -> Result<()> {
    // L5 (round-1 codeaudit): passing the secret via argv (`-w <secret>`)
    // briefly exposes it in `ps(1)` output. Acknowledged limitation of
    // `security(1)` — its stdin variant does not exist for
    // add-generic-password. Alternative (Security.framework FFI) triggers
    // "always allow?" keychain prompts on every launch of an unsigned
    // brew-installed binary (see header comment at the top of this file).
    // Kept as-is; documented as SEC-2 in security posture notes.
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

fn real_delete(service: &str, account: &str) -> Result<()> {
    // Same class as H7 (round-1 codeaudit): distinguish "item not found"
    // (exit 44, benign) from every other failure. Collapsing all non-zero
    // to Ok(()) masked keychain-locked / permission errors, which then
    // let callers assume the delete "succeeded" and move on.
    let out = Command::new("security")
        .args(["delete-generic-password", "-s", service, "-a", account])
        .output()
        .context("running `security delete-generic-password`")?;
    if !out.status.success() {
        match out.status.code() {
            Some(44) => return Ok(()),
            other => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                bail!(
                    "`security delete-generic-password` failed \
                     (exit {other:?}) for service={service} account={account}: {}",
                    stderr.trim(),
                );
            }
        }
    }
    Ok(())
}

/// Production `SecretStore` impl: shells out to `security(1)`. Compiled only
/// for non-test builds so `cargo test` can never reach the real login
/// keychain through `platform().secrets()` — see the `cfg(test)` impl below.
#[cfg(not(test))]
impl SecretStore for MacOsSecrets {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        real_get(service, account)
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        real_set(service, account, secret)
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        real_delete(service, account)
    }

    fn list(&self, _service: &str) -> Result<Vec<String>> {
        // `security dump-keychain -a` enumerates but is heavy and prompts.
        // Callers today track accounts via state.json; this stays empty
        // until a real caller needs it.
        Ok(Vec::new())
    }
}

/// Test-only `SecretStore` impl: a process-local in-memory map, never a
/// subprocess. Every unit / integration test that reaches `MacOsSecrets`
/// through the `SecretStore` trait (e.g. via `platform().secrets()`) lands
/// here instead of touching the developer's real login keychain — the crash
/// this module exists to fix (SecurityAgent prompts + keychain writes fired
/// by `cargo test`). The `#[ignore]`d `secret_store_roundtrip` test below
/// deliberately bypasses this impl (calling `real_get`/`real_set`/
/// `real_delete` directly) because its entire point is to exercise the real
/// keychain, on demand, under `--ignored`.
#[cfg(test)]
impl SecretStore for MacOsSecrets {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        Ok(in_memory::get(service, account))
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        in_memory::set(service, account, secret);
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        in_memory::delete(service, account);
        Ok(())
    }

    fn list(&self, _service: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// Backing store for the `cfg(test)` `SecretStore` impl above. A plain
/// `Mutex<HashMap>` keyed by `(service, account)` — process-local, cleared
/// only on process exit, which is exactly right for `cargo test` where each
/// test binary is its own process and tests within it may legitimately want
/// to observe state written by an earlier `set` in the same run (mirroring
/// how the real keychain persists across calls within a process).
#[cfg(test)]
mod in_memory {
    use std::collections::HashMap;
    use std::sync::Mutex;

    static STORE: Mutex<Option<HashMap<(String, String), String>>> = Mutex::new(None);

    fn with_store<T>(f: impl FnOnce(&mut HashMap<(String, String), String>) -> T) -> T {
        let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        f(guard.get_or_insert_with(HashMap::new))
    }

    pub(super) fn get(service: &str, account: &str) -> Option<String> {
        with_store(|m| m.get(&(service.to_string(), account.to_string())).cloned())
    }

    pub(super) fn set(service: &str, account: &str, secret: &str) {
        with_store(|m| {
            m.insert(
                (service.to_string(), account.to_string()),
                secret.to_string(),
            )
        });
    }

    pub(super) fn delete(service: &str, account: &str) {
        with_store(|m| m.remove(&(service.to_string(), account.to_string())));
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

/// XML-escape the five reserved chars so a label / path / arg containing
/// `&`, `<`, `>`, `"`, or `'` can't corrupt the plist. L2 (round-1 codeaudit).
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

impl Autostart for MacOsAutostart {
    fn install(&self, label: &str, binary: &Path, args: &[&str]) -> Result<()> {
        let mut prog_args = format!(
            "    <string>{}</string>\n",
            xml_escape(&binary.display().to_string())
        );
        for a in args {
            prog_args.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
        }
        let label_esc = xml_escape(label);
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label_esc}</string>
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
        // Serialised against every other in-crate `$HOME` mutator via the
        // crate-wide `env_lock::ENV_LOCK`. Before this was hoisted out of
        // per-module mutexes, this test raced other tests' `$HOME` swaps and
        // one of those interleavings wiped a live developer state.json
        // (see the never-re-login postmortem).
        crate::env_lock::scoped_env_var("HOME", Some("/tmp/platform-test-home"), || {
            let got = MacOsPaths.config_dir("usagio");
            assert_eq!(
                got,
                std::path::PathBuf::from("/tmp/platform-test-home/.config/usagio")
            );
            let cache = MacOsPaths.cache_dir("usagio");
            assert_eq!(
                cache,
                std::path::PathBuf::from("/tmp/platform-test-home/Library/Caches/usagio")
            );
        });
    }

    /// SecretStore round-trip against the real login keychain. `#[ignore]`d by
    /// default so `cargo test` doesn't touch the developer's keychain — run
    /// explicitly with `cargo test -- --ignored secret_store_roundtrip`.
    /// Uses a per-run unique service name and cleans up on both success and
    /// failure paths.
    ///
    /// Calls `real_get`/`real_set`/`real_delete` directly rather than going
    /// through `MacOsSecrets`'s `SecretStore` impl: under `cfg(test)` that
    /// impl is the in-memory mock (see above), so dispatching through the
    /// trait here would silently test the mock instead of the real
    /// `security(1)` subprocess path this test exists to exercise.
    #[test]
    #[ignore = "touches the real login keychain; run with --ignored"]
    fn secret_store_roundtrip() {
        let service = format!("usagio-platform-test-{}", std::process::id());
        let account = "roundtrip";
        let secret = "hunter2";

        // Ensure a clean starting point.
        let _ = real_delete(&service, account);

        real_set(&service, account, secret).expect("set");
        let got = real_get(&service, account).expect("get");
        assert_eq!(got.as_deref(), Some(secret));

        real_delete(&service, account).expect("delete");
        let after = real_get(&service, account).expect("get after delete");
        assert!(after.is_none(), "secret still present after delete");
    }
}
