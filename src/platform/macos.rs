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
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

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
    fn secure_permissions(&self, path: &Path) -> Result<()> {
        super::secure_permissions_unix(path)
    }
    fn register_notification_app(&self) {
        // Tell mac-notification-sys our real bundle id so it doesn't fall back
        // to the "use_default" literal that triggers the "Where is use_default?"
        // Choose-Application dialog. `set_application` is a one-shot global; a
        // second call (or an unregistered id) returns Err, which we log and
        // ignore — the worst case is the pre-existing behavior, never a crash.
        const BUNDLE_ID: &str = "com.mattjackson.usagio";
        match notify_rust::set_application(BUNDLE_ID) {
            Ok(()) => {
                crate::logging::log(&format!("notifications: registered bundle id {BUNDLE_ID}"));
            }
            Err(e) => {
                crate::logging::log(&format!(
                    "notifications: set_application({BUNDLE_ID}) failed: {e:?} \
                     (notifications still fire; the use_default dialog may appear)"
                ));
            }
        }
    }
    fn notify(&self, summary: &str, body: &str) -> Result<()> {
        notify_rust::Notification::new()
            .summary(summary)
            .body(body)
            .show()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("notify-rust show failed: {e}"))
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

// ---- security(1) hang guard (robustness-01) ------------------------------
//
// A locked keychain, or a SecurityAgent "Allow/Deny" dialog with no
// interactive user around to dismiss it (the common case: usagio runs as a
// headless menu-bar poller), makes `security(1)` block indefinitely. Every
// `security(1)` invocation in this module goes through `run_security`, which
// spawns the process and enforces a SECURITY_TIMEOUT deadline NATIVELY —
// polling `try_wait` and `kill`-ing the child if it overruns — so the poll
// thread can never hang forever on a keychain call. `security(1)` normally
// completes in well under 100ms; 5s is generous headroom while still bounding
// the worst case.
//
// v0.5.15: replaced the old `timeout(1)`-wrapper approach. macOS does not ship
// `/usr/bin/timeout` (it's GNU coreutils, only present if brew-installed
// unprefixed), so on a stock Mac the wrapper silently fell back to a bare
// `security` call with NO hang guard at all — exactly the machines most likely
// to hit a locked keychain after sleep. The native guard has no external
// dependency and behaves identically on every Mac.
const SECURITY_TIMEOUT_SECS: u64 = 5;
const SECURITY_TIMEOUT: Duration = Duration::from_secs(SECURITY_TIMEOUT_SECS);

/// How often `run_with_timeout` polls the child for exit. `security(1)`
/// finishes in well under 100ms, so a 10ms poll adds negligible latency to the
/// normal path while keeping the kill-on-overrun responsive.
const SECURITY_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Outcome of a `security(1)` invocation run under the native hang guard.
struct SecurityRun {
    /// True iff the call exceeded `SECURITY_TIMEOUT` and was killed. The
    /// caller treats this like any other failure ("don't touch anything"),
    /// but logs it distinctly so a locked keychain is diagnosable.
    timed_out: bool,
    /// The child's exit status, or `None` if it was killed for overrunning.
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl SecurityRun {
    fn success(&self) -> bool {
        self.status.map(|s| s.success()).unwrap_or(false)
    }
    fn code(&self) -> Option<i32> {
        self.status.and_then(|s| s.code())
    }
}

/// Run `security(1)` with `args` under the native hang guard. stdout/stderr
/// are captured; stdin is closed so an interactive prompt can't block on
/// input while we wait.
fn run_security(args: &[&str]) -> Result<SecurityRun> {
    let child = Command::new("security")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Spawn as its own process-group leader (pgid == child pid) so the
        // overrun path can SIGKILL the whole group — see `kill_group_and_reap`.
        .process_group(0)
        .spawn()
        .context("spawning `security`")?;
    run_with_timeout(child, SECURITY_TIMEOUT)
}

/// SIGKILL the child's entire process group, then reap the direct child.
///
/// Precondition: `child` was spawned as its own group leader (`process_group(0)`
/// in `run_security` and the test spawner), so its process-group id equals its
/// pid and we can never signal an unrelated group. Killing the whole group —
/// rather than just the direct child — means any descendant that inherited the
/// stdout/stderr pipe also dies, so its write end closes and the reader threads'
/// `read_to_end` can't block forever waiting on a surviving grandchild. `wait`
/// reaps the direct child; group descendants are reparented to launchd/init.
fn kill_group_and_reap(child: &mut Child) {
    // SAFETY: `killpg` with the child's own group id; harmless ESRCH if the
    // group has already exited. libc is a unix-target dependency.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.wait();
}

/// Wait for an already-spawned child up to `timeout`, killing it if it
/// overruns. stdout/stderr are drained on dedicated threads so a child that
/// fills a pipe buffer can't deadlock against the `try_wait` poll loop. On
/// overrun (or a `try_wait` error) the child's whole process GROUP is killed,
/// so a hung child AND any pipe-inheriting descendant die and the reader
/// threads unblock — the join below then can't hang. (The one residual case
/// the deadline can't bound is a child that exits cleanly but leaves a
/// backgrounded descendant holding the pipe open; `security(1)` never does
/// this — it reaches SecurityAgent over XPC, not via a fork/exec child.)
///
/// Split out from `run_security` so it can be unit-tested with ordinary
/// commands (`sleep`, `echo`) rather than the real keychain; the test spawner
/// mirrors `run_security`'s `process_group(0)` so the group-kill precondition
/// holds there too.
fn run_with_timeout(mut child: Child, timeout: Duration) -> Result<SecurityRun> {
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Overran: kill the whole group and reap. Closing every
                    // write end of the pipe lets the reader threads' read_to_end
                    // return so the joins below can't hang.
                    kill_group_and_reap(&mut child);
                    break None;
                }
                std::thread::sleep(SECURITY_POLL_INTERVAL);
            }
            Err(e) => {
                // Rare (try_wait handles EINTR internally). Don't leak the
                // child or the reader threads: kill the group so the pipes
                // close and the joins return, reap, then surface the error.
                kill_group_and_reap(&mut child);
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(e).context("waiting on `security`");
            }
        }
    };

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok(SecurityRun {
        timed_out: status.is_none(),
        status,
        stdout,
        stderr,
    })
}

fn real_get(service: &str, account: &str) -> Result<Option<String>> {
    // Only exit code 44 ("SecItem not found" per <Security/SecBase.h> /
    // `security(1)` conventions) is a genuine "not present". Any other
    // non-zero — keychain locked, permission denied, IPC failure —
    // surfaces as Err so callers like `sync_active_from_keychain` treat
    // it as "don't touch anything" rather than "assume gone" (which
    // would let a subsequent switch overwrite a still-valid token).
    // See H7 in the round-1 codeaudit findings.
    let out = run_security(&["find-generic-password", "-s", service, "-a", account, "-w"])
        .context("running `security find-generic-password`")?;
    let timed_out = out.timed_out;
    let result = if timed_out {
        // robustness-01: locked keychain / unattended SecurityAgent dialog.
        // Distinct error branch, but still just an Err to the caller — same
        // "don't touch anything" treatment as any other failure above.
        Err(anyhow::anyhow!(
            "`security find-generic-password` timed out after {SECURITY_TIMEOUT_SECS}s \
             (locked keychain or an unattended SecurityAgent dialog) for \
             service={service} account={account}",
        ))
    } else if !out.success() {
        match out.code() {
            Some(44) => Ok(None),
            other => {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Err(anyhow::anyhow!(
                    "`security find-generic-password` failed \
                     (exit {other:?}) for service={service} account={account}: {stderr}",
                ))
            }
        }
    } else {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if s.is_empty() { None } else { Some(s) })
    };
    // Belt-and-suspenders: log every keychain touch so a bug that leaves
    // nothing else logged still shows up here (M4-audit finding).
    crate::logging::log(&format!(
        "event={} svc={service} acct={account} result={}",
        if timed_out {
            "keychain_read_timeout"
        } else {
            "keychain_read"
        },
        match &result {
            Ok(Some(_)) => "ok".to_string(),
            Ok(None) => "not_found".to_string(),
            Err(e) => format!("err:{e:#}"),
        }
    ));
    result
}

fn real_set(service: &str, account: &str, secret: &str) -> Result<()> {
    // L5 (round-1 codeaudit): passing the secret via argv (`-w <secret>`)
    // briefly exposes it in `ps(1)` output. Acknowledged limitation of
    // `security(1)` — its stdin variant does not exist for
    // add-generic-password. Alternative (Security.framework FFI) triggers
    // "always allow?" keychain prompts on every launch of an unsigned
    // brew-installed binary (see header comment at the top of this file).
    // Kept as-is; documented as SEC-2 in security posture notes.
    //
    // M4-audit finding: `add-generic-password -U` (update-in-place) triggers
    // a SecurityAgent "Keychain Not Found"/"always allow?" prompt when the
    // existing item's ACL doesn't already list this (unsigned) binary — e.g.
    // an item Claude Code itself created. Delete the item first (ignoring
    // "not found", exit 44) and add it fresh WITHOUT `-U`, so the new item's
    // ACL only ever contains usagio and no update-ACL negotiation with a
    // differently-ACL'd existing item ever happens.
    let mut timed_out = false;
    let result = (|| -> Result<()> {
        let del = run_security(&["delete-generic-password", "-s", service, "-a", account])
            .context("running `security delete-generic-password` (pre-set)")?;
        if del.timed_out {
            timed_out = true;
            bail!(
                "`security delete-generic-password` (pre-set) timed out after \
                 {SECURITY_TIMEOUT_SECS}s (locked keychain or an unattended SecurityAgent \
                 dialog) for service={service} account={account}",
            );
        }
        if !del.success() && del.code() != Some(44) {
            let stderr = String::from_utf8_lossy(&del.stderr);
            bail!(
                "`security delete-generic-password` (pre-set) failed \
                 (exit {:?}) for service={service} account={account}: {}",
                del.code(),
                stderr.trim(),
            );
        }
        let out = run_security(&[
            "add-generic-password",
            "-s",
            service,
            "-a",
            account,
            "-w",
            secret,
        ])
        .context("running `security add-generic-password`")?;
        if out.timed_out {
            timed_out = true;
            bail!(
                "`security add-generic-password` timed out after {SECURITY_TIMEOUT_SECS}s \
                 (locked keychain or an unattended SecurityAgent dialog) for \
                 service={service} account={account}",
            );
        }
        if !out.success() {
            bail!("`security add-generic-password` failed for service={service} account={account}");
        }
        Ok(())
    })();
    crate::logging::log(&format!(
        "event={} svc={service} acct={account} result={}",
        if timed_out {
            "keychain_write_timeout"
        } else {
            "keychain_write"
        },
        match &result {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("err:{e:#}"),
        }
    ));
    result
}

fn real_delete(service: &str, account: &str) -> Result<()> {
    // Same class as H7 (round-1 codeaudit): distinguish "item not found"
    // (exit 44, benign) from every other failure. Collapsing all non-zero
    // to Ok(()) masked keychain-locked / permission errors, which then
    // let callers assume the delete "succeeded" and move on.
    let out = run_security(&["delete-generic-password", "-s", service, "-a", account])
        .context("running `security delete-generic-password`")?;
    if out.timed_out {
        // robustness-01: same hang guard as real_get/real_set.
        crate::logging::log(&format!(
            "event=keychain_delete_timeout svc={service} acct={account}"
        ));
        bail!(
            "`security delete-generic-password` timed out after {SECURITY_TIMEOUT_SECS}s \
             (locked keychain or an unattended SecurityAgent dialog) for \
             service={service} account={account}",
        );
    }
    if !out.success() {
        match out.code() {
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
        // Failure-injection seam (shared across platforms): a test can arm the
        // next `set` to fail so the apply_account keychain-write-failure ROLLBACK
        // path (the guard behind "never a half-applied switch") is exercisable —
        // the mock otherwise always succeeds and left that branch uncovered.
        if super::test_secret_seam::take_set_failure() {
            bail!("injected keychain set failure (test seam)");
        }
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

    /// Best-effort `lsregister -f -R -trusted <app.app>` so Launch Services
    /// knows about a freshly-installed bundle before anything (notably the
    /// first notification) tries to resolve it. `binary` is expected to be
    /// `<app>.app/Contents/MacOS/<name>`; walk up three levels to the `.app`
    /// directory. A from-source / bare-binary install has no such bundle —
    /// `parent()` chains bottom out to `None` (or the resolved dir doesn't
    /// end in `.app`) and this is a silent no-op, same as any other failure
    /// here (wrong lsregister path across macOS versions, missing bundle,
    /// etc.) — never fatal to `install`.
    fn register_with_launch_services(binary: &Path) {
        const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

        let Some(app_bundle) = binary
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
        else {
            return;
        };
        if app_bundle.extension().and_then(|e| e.to_str()) != Some("app") {
            return;
        }
        if !Path::new(LSREGISTER).is_file() {
            crate::logging::log(&format!(
                "lsregister not found at {LSREGISTER}; skipping Launch Services registration"
            ));
            return;
        }
        match Command::new(LSREGISTER)
            .args(["-f", "-R", "-trusted"])
            .arg(app_bundle)
            .status()
        {
            Ok(s) if s.success() => {
                crate::logging::log(&format!(
                    "registered {} with Launch Services",
                    app_bundle.display()
                ));
            }
            Ok(s) => crate::logging::log(&format!("lsregister exited with {s}")),
            Err(e) => crate::logging::log(&format!("lsregister could not run: {e}")),
        }
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

/// Build the argv for `launchctl bootstrap gui/<uid>/ <plist_path>`. Split out
/// so a hermetic test can pin the exact argv usagio's post-brew `install` path
/// hands to launchd, without ever invoking `launchctl` for real (v0.5.3 P0 #2:
/// after brew's `post_install` writes the plist, the LaunchAgent was never
/// bootstrapped and the menu bar app didn't run until the next reboot).
pub(crate) fn launchctl_bootstrap_argv(uid: u32, plist_path: &Path) -> Vec<String> {
    vec![
        "bootstrap".to_string(),
        format!("gui/{uid}"),
        plist_path.to_string_lossy().into_owned(),
    ]
}

/// Build the argv for `launchctl bootout gui/<uid>/<label>` (the modern
/// counterpart to `unload`). Symmetric with `launchctl_bootstrap_argv`.
pub(crate) fn launchctl_bootout_argv(uid: u32, label: &str) -> Vec<String> {
    vec!["bootout".to_string(), format!("gui/{uid}/{label}")]
}

/// Build the argv for `launchctl print gui/<uid>/<label>`, the probe used to
/// tell whether the agent is currently loaded before we bootout/unload it.
/// Split out so a hermetic test can pin its exact shape without invoking
/// `launchctl` for real (v0.5.7: `install`/`uninstall` used to run a
/// best-effort legacy `unload` unconditionally, which printed a scary
/// "Unload failed: 5: Input/output error" to stderr whenever nothing was
/// loaded — we now only unload/bootout when this probe says the agent is up).
pub(crate) fn launchctl_print_argv(uid: u32, label: &str) -> Vec<String> {
    vec!["print".to_string(), format!("gui/{uid}/{label}")]
}

/// Whether the LaunchAgent `label` is currently loaded in the caller's GUI
/// domain. `launchctl print gui/<uid>/<label>` exits 0 iff the service is
/// registered, so a successful exit is our "is it loaded?" signal. Any error
/// (not loaded, launchctl missing) is treated as "not loaded" — the caller
/// then skips bootout/unload entirely, which is exactly the safe default.
#[cfg(unix)]
fn is_agent_loaded(uid: u32, label: &str) -> bool {
    Command::new("launchctl")
        .args(launchctl_print_argv(uid, label))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Max attempts and backoff for the v0.5.4 bootstrap retry loop below.
/// v0.5.3 shipped a single-shot `launchctl bootstrap`; on a fresh
/// `brew install`, brew's `def post_install` invokes `system bin/"usagio",
/// "install"` BEFORE the keg is linked to `/opt/homebrew/opt/usagio`, so the
/// plist's `ProgramArguments[0]` path (which points at the opt symlink) does
/// not yet exist. launchd silently declines to register a plist whose target
/// binary is missing, and by the time the user's next shell notices, the
/// menu bar app isn't running. Retry with a brief backoff so the link-then-
/// post-install ordering has time to catch up — version-agnostic, cheap.
#[cfg(unix)]
const BOOTSTRAP_MAX_ATTEMPTS: u32 = 5;
#[cfg(unix)]
const BOOTSTRAP_BACKOFF_MS: u64 = 300;

/// Load a plist via `launchctl bootstrap` (modern macOS 10.10+ form). Retries
/// up to `BOOTSTRAP_MAX_ATTEMPTS` with a short backoff between attempts —
/// during a `brew install` the keg link into `/opt/homebrew/opt/usagio` isn't
/// yet in place when `post_install` runs, so a first bootstrap can fail with
/// the plist target missing (v0.5.4). Falls back to `launchctl load -w` on
/// persistent failure — some older macOS point releases still expect the
/// legacy form, and the fallback is cheap. Emits a structured token-lifecycle
/// log line for every attempt so a broken post-brew autostart shows up in
/// `usagio-log`.
#[cfg(unix)]
fn launchctl_bootstrap(plist_path: &Path) -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let argv = launchctl_bootstrap_argv(uid, plist_path);
    let mut last_exit: Option<i32> = None;
    let mut last_stderr = String::new();
    for attempt in 1..=BOOTSTRAP_MAX_ATTEMPTS {
        let out = Command::new("launchctl").args(&argv).output();
        match out {
            Ok(o) if o.status.success() => {
                crate::logging::log(&format!(
                    "event=launchagent_bootstrap result=ok uid={uid} attempt={attempt} plist={}",
                    plist_path.display(),
                ));
                return Ok(());
            }
            Ok(o) => {
                last_exit = o.status.code();
                last_stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                crate::logging::log(&format!(
                    "event=launchagent_bootstrap result=retry uid={uid} attempt={attempt}/{max} \
                     plist={} exit={:?} stderr={:?}",
                    plist_path.display(),
                    last_exit,
                    last_stderr,
                    max = BOOTSTRAP_MAX_ATTEMPTS,
                ));
            }
            Err(e) => {
                last_stderr = format!("spawn error: {e}");
                crate::logging::log(&format!(
                    "event=launchagent_bootstrap result=retry uid={uid} attempt={attempt}/{max} \
                     plist={} err={e}",
                    plist_path.display(),
                    max = BOOTSTRAP_MAX_ATTEMPTS,
                ));
            }
        }
        if attempt < BOOTSTRAP_MAX_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(BOOTSTRAP_BACKOFF_MS));
        }
    }
    crate::logging::log(&format!(
        "event=launchagent_bootstrap result=fallback_to_load uid={uid} plist={} \
         final_exit={:?} final_stderr={last_stderr:?}",
        plist_path.display(),
        last_exit,
    ));
    // Legacy fallback.
    let load = Command::new("launchctl")
        .args(["load", "-w", &plist_path.to_string_lossy()])
        .status()
        .context("launchctl load -w (bootstrap fallback)")?;
    if !load.success() {
        crate::logging::log(&format!(
            "event=launchagent_load result=err uid={uid} plist={} exit={:?}",
            plist_path.display(),
            load.code(),
        ));
        bail!(
            "launchctl bootstrap (x{BOOTSTRAP_MAX_ATTEMPTS}) AND launchctl load both failed for {}",
            plist_path.display()
        );
    }
    crate::logging::log(&format!(
        "event=launchagent_load result=ok uid={uid} plist={}",
        plist_path.display(),
    ));
    Ok(())
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

        // Register the app bundle with Launch Services before the LaunchAgent
        // starts it. Without this, a freshly-installed `.app` (from `usagio
        // install`) is unknown to LS, and the first `notify-rust` call (via
        // `mac-notification-sys`, whose sound-name literal is `"use_default"`)
        // pops a "Where is use_default?" Choose Application dialog instead of
        // sending the notification — LS is trying to resolve a bundle
        // identifier it's never seen. Best-effort: `lsregister`'s path can
        // move across macOS versions, and a from-source install's `binary`
        // isn't inside a `.app` at all, so any failure here just leaves the
        // (also best-effort) notification path unregistered — never fatal to
        // `install`.
        Self::register_with_launch_services(binary);

        // v0.5.3 P0 #2: after brew's `post_install` (or a manual `usagio
        // install`) writes the plist, we MUST also load it — otherwise the
        // menu bar app doesn't run until the next reboot. If a stale entry is
        // holding the label, bootout first; then bootstrap the fresh plist.
        // `launchctl_bootstrap` falls back to `load -w` if bootstrap fails
        // (some older macOS point releases still need the legacy form).
        //
        // v0.5.7: probe with `launchctl print` FIRST and only unload/bootout
        // when the agent is actually loaded. Running a best-effort `unload`
        // unconditionally printed a scary "Unload failed: 5: Input/output
        // error\nTry running `launchctl bootout` as root for richer errors."
        // to stderr on every fresh install (nothing was loaded yet). We
        // prevent that noise rather than suppress it — if a bootout genuinely
        // fails while the agent IS loaded, that error is worth surfacing.
        #[cfg(unix)]
        {
            let uid = unsafe { libc::getuid() };
            if is_agent_loaded(uid, label) {
                // The agent is up — tear it down before bootstrapping the
                // fresh plist. bootout is the modern form and should succeed
                // here; a real failure now is worth seeing (unlike the
                // spurious "nothing loaded" error we used to emit).
                let _ = Command::new("launchctl")
                    .args(launchctl_bootout_argv(uid, label))
                    .output();
            }
            // If NOT loaded we skip both bootout AND the legacy `unload`
            // entirely: there is nothing to unload, and the legacy `unload`
            // is exactly what emitted the "Input/output error" noise.
            launchctl_bootstrap(&path)?;
        }
        Ok(())
    }

    fn uninstall(&self, label: &str) -> Result<()> {
        let path = Self::plist_path(label)?;
        #[cfg(unix)]
        {
            let uid = unsafe { libc::getuid() };
            // v0.5.7: only tear down when the agent is actually loaded. Probe
            // with `launchctl print` first so we never run bootout/unload
            // against a label that isn't bootstrapped — an unconditional
            // legacy `unload` there prints a spurious "Unload failed: 5:
            // Input/output error" to stderr (same noise `install` used to
            // emit). When it IS loaded, a real failure is worth surfacing.
            if is_agent_loaded(uid, label) {
                // Bootout is the modern counterpart to `unload`.
                let _ = Command::new("launchctl")
                    .args(launchctl_bootout_argv(uid, label))
                    .output();
                // Also drop any legacy `load`-registered entry before removing
                // the file, so a switch from an older usagio that used the
                // legacy form doesn't leave a stale live job behind.
                let _ = Command::new("launchctl")
                    .args(["unload", &path.to_string_lossy()])
                    .status();
            }
        }
        if path.exists() {
            std::fs::remove_file(&path).context("removing plist")?;
        }
        // Also purge any legacy System Events login item that a pre-v0.4.3
        // usagio (or its `claude-usage` predecessor) registered via the
        // now-removed "Launch at login" menu toggle. Left in place, macOS
        // re-launches the stale binary at every login even after the plist
        // is gone — the exact bug that had usagio silently respawning
        // post-uninstall. Best-effort: osascript exits non-zero if the item
        // is already absent (the desired state), so all output is swallowed.
        // Wrap in `timeout(1) 3` so a first-run TCC Automation dialog can't
        // hang `brew uninstall` indefinitely — this login-item purge is
        // best-effort defense-in-depth; if TCC prompts and the user doesn't
        // dismiss it in 3 seconds, we move on (the plist deletion above is
        // what actually stops autostart).
        for name in ["usagio", "claude-usage"] {
            let _ = Command::new("timeout")
                .args([
                    "3",
                    "osascript",
                    "-e",
                    &format!("tell application \"System Events\" to delete login item \"{name}\""),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
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

    /// Helper: spawn an arbitrary command with captured pipes so the
    /// `run_with_timeout` tests can exercise the guard without the real
    /// keychain. Mirrors how `run_security` configures stdio.
    fn spawn_for_test(program: &str, args: &[&str]) -> Child {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Mirror run_security so kill_group_and_reap's group-leader
            // precondition holds — otherwise killpg could signal the test
            // runner's own group.
            .process_group(0)
            .spawn()
            .expect("spawn test child")
    }

    /// robustness-01: a command that finishes inside the deadline is NOT
    /// reported as timed out, exposes its real exit status, and its stdout is
    /// captured in full.
    #[test]
    fn run_with_timeout_captures_a_fast_success() {
        let child = spawn_for_test("sh", &["-c", "printf hello"]);
        let run = run_with_timeout(child, SECURITY_TIMEOUT).expect("run");
        assert!(!run.timed_out);
        assert!(run.success());
        assert_eq!(run.code(), Some(0));
        assert_eq!(String::from_utf8_lossy(&run.stdout), "hello");
    }

    /// robustness-01: a non-zero exit (e.g. `security`'s own "not found" code
    /// 44) is surfaced as its real code, NEVER confused with a timeout.
    #[test]
    fn run_with_timeout_surfaces_nonzero_exit_not_as_timeout() {
        let child = spawn_for_test("sh", &["-c", "exit 44"]);
        let run = run_with_timeout(child, SECURITY_TIMEOUT).expect("run");
        assert!(!run.timed_out);
        assert!(!run.success());
        assert_eq!(run.code(), Some(44));
    }

    /// robustness-01 (the whole point): a command that overruns the deadline
    /// is killed and reported as timed out — the poll thread does NOT hang.
    /// Bounds wall-clock so a regression that failed to kill would blow the
    /// test's own time budget rather than pass.
    #[test]
    fn run_with_timeout_kills_an_overrunning_child() {
        let started = Instant::now();
        let child = spawn_for_test("sleep", &["30"]);
        let run = run_with_timeout(child, Duration::from_millis(150)).expect("run");
        assert!(
            run.timed_out,
            "a 30s sleep under a 150ms deadline must time out"
        );
        assert!(run.status.is_none());
        assert!(!run.success());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the child must be killed promptly, not waited out"
        );
    }

    /// A child that writes more than a pipe buffer's worth of output must not
    /// deadlock the try_wait loop against an undrained pipe — the dedicated
    /// reader threads keep the pipe empty. 256 KiB comfortably exceeds the
    /// typical 64 KiB pipe capacity.
    #[test]
    fn run_with_timeout_drains_large_output_without_deadlock() {
        let child = spawn_for_test("sh", &["-c", "yes AAAAAAAA | head -c 262144"]);
        let run = run_with_timeout(child, SECURITY_TIMEOUT).expect("run");
        assert!(!run.timed_out);
        assert!(run.success());
        assert_eq!(run.stdout.len(), 262144);
    }

    /// robustness-01 (invariant hardening): an overrunning child that has
    /// spawned a descendant which INHERITED the stdout pipe must still return
    /// promptly — the group kill takes out the descendant too, so the reader
    /// threads' read_to_end unblocks and the join doesn't hang. Without the
    /// process-group kill, the backgrounded `sleep` would hold the pipe open
    /// and `run_with_timeout` would block for the full 30s.
    #[test]
    fn run_with_timeout_kills_pipe_inheriting_descendants_on_overrun() {
        let started = Instant::now();
        // The shell exits after backgrounding `sleep`, but the sleep inherits
        // the stdout pipe. The parent shell also `sleep`s so try_wait is None
        // at the deadline and we take the overrun/group-kill path.
        let child = spawn_for_test("sh", &["-c", "sleep 30 & sleep 30"]);
        let run = run_with_timeout(child, Duration::from_millis(150)).expect("run");
        assert!(run.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "group kill must reap the pipe-inheriting descendant, not wait it out"
        );
    }

    /// v0.5.3 P0 #2: `launchctl_bootstrap_argv` MUST produce the exact
    /// `["bootstrap", "gui/<uid>", "<plist>"]` argv brew's `post_install →
    /// usagio install` path hands to `launchctl`. Regression-guards a merge
    /// that could drop the bootstrap step (which is exactly what happened
    /// between v0.4.x and v0.5.2 — the plist was being written but never
    /// loaded, so the menu bar didn't come up until reboot).
    #[test]
    fn launchctl_bootstrap_argv_matches_expected_shape() {
        let argv = super::launchctl_bootstrap_argv(
            501,
            std::path::Path::new(
                "/Users/x/Library/LaunchAgents/com.mattjackson.usagio.menubar.plist",
            ),
        );
        assert_eq!(
            argv,
            vec![
                "bootstrap".to_string(),
                "gui/501".to_string(),
                "/Users/x/Library/LaunchAgents/com.mattjackson.usagio.menubar.plist".to_string(),
            ],
        );
    }

    /// v0.5.3 P0 #2: `launchctl_bootout_argv` must be the symmetric shutdown
    /// form — `["bootout", "gui/<uid>/<label>"]` — so `usagio uninstall`
    /// tears down the running job before deleting the plist.
    #[test]
    fn launchctl_bootout_argv_matches_expected_shape() {
        let argv = super::launchctl_bootout_argv(501, "com.mattjackson.usagio.menubar");
        assert_eq!(
            argv,
            vec![
                "bootout".to_string(),
                "gui/501/com.mattjackson.usagio.menubar".to_string(),
            ],
        );
    }

    /// v0.5.7: `launchctl_print_argv` must be the exact `["print",
    /// "gui/<uid>/<label>"]` probe `install`/`uninstall` use to decide whether
    /// to bootout/unload at all — the guard that stops the spurious
    /// "Unload failed: 5: Input/output error" on a fresh install. Hermetic:
    /// pins the argv shape without invoking `launchctl` for real.
    #[test]
    fn launchctl_print_argv_matches_expected_shape() {
        let argv = super::launchctl_print_argv(501, "com.mattjackson.usagio.menubar");
        assert_eq!(
            argv,
            vec![
                "print".to_string(),
                "gui/501/com.mattjackson.usagio.menubar".to_string(),
            ],
        );
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
        // real_get/set/delete log through crate::logging, which resolves
        // store::config_dir(); the test harness forbids that without a
        // HOME_OVERRIDE, so install a tempdir one for the duration.
        let _g = crate::store::ScopedConfigDir::new();
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

    /// M4-audit: `set` must delete-then-add rather than `add-generic-password
    /// -U`, so the second `set` doesn't hit the "-U update" ACL-negotiation
    /// path that can prompt SecurityAgent. Calling `set` twice in a row (the
    /// exact CAS-write pattern) must succeed both times and leave the LATEST
    /// value in place. `#[ignore]`d for the same reason as
    /// `secret_store_roundtrip` — touches the real login keychain.
    #[test]
    #[ignore = "touches the real login keychain; run with --ignored"]
    fn macos_secrets_set_deletes_then_adds() {
        let _g = crate::store::ScopedConfigDir::new();
        let service = format!("usagio-platform-test-cas-{}", std::process::id());
        let account = "cas-write";
        // Call real_* directly, NOT through MacOsSecrets: under cfg(test) the
        // SecretStore impl is the in-memory mock, so dispatching through it
        // would test the mock's trivial last-write-wins map rather than the
        // real `security(1)` delete-then-add this test exists to prove.
        let _ = real_delete(&service, account);

        real_set(&service, account, "generation-1").expect("first set");
        assert_eq!(
            real_get(&service, account).unwrap().as_deref(),
            Some("generation-1")
        );

        // The second `set` is the one that would have hit `-U`'s "update an
        // existing item" path under the old implementation. It must succeed
        // cleanly and leave the newest value in place.
        real_set(&service, account, "generation-2").expect("second set");
        assert_eq!(
            real_get(&service, account).unwrap().as_deref(),
            Some("generation-2")
        );

        real_delete(&service, account).expect("cleanup delete");
    }

    /// M4-audit: `set` must succeed even when an item under the same
    /// service/account already exists and was created by a DIFFERENT
    /// process/tool (simulated here by seeding with a raw `security`
    /// invocation rather than going through `MacOsSecrets`) — this is exactly
    /// the "Claude Code already wrote this keychain item" scenario the
    /// delete-then-add rewrite exists to handle without an `-U` ACL prompt.
    #[test]
    #[ignore = "touches the real login keychain; run with --ignored"]
    fn macos_secrets_set_after_existing_item_created_by_another_process() {
        let service = format!("usagio-platform-test-foreign-{}", std::process::id());
        let account = "foreign-owner";
        let _ = Command::new("security")
            .args(["delete-generic-password", "-s", &service, "-a", account])
            .output();

        // Seed as a plain (non-`-U`) item, standing in for "some other tool
        // created this keychain entry".
        let seed = Command::new("security")
            .args([
                "add-generic-password",
                "-s",
                &service,
                "-a",
                account,
                "-w",
                "seeded-by-another-process",
            ])
            .status()
            .expect("seed add-generic-password");
        assert!(seed.success(), "failed to seed the foreign-owned item");

        // real_* directly, NOT MacOsSecrets: the cfg(test) trait impl is the
        // in-memory mock, which would neither see the foreign item seeded above
        // (so the overwrite is never actually tested) nor delete the real item
        // in cleanup (leaking it into the login keychain on every run).
        real_set(&service, account, "usagio-owned-now")
            .expect("set must succeed over a foreign-created item");
        assert_eq!(
            real_get(&service, account).unwrap().as_deref(),
            Some("usagio-owned-now")
        );

        real_delete(&service, account).expect("cleanup delete");
    }
}
