//! claude-usage — usage/limits across multiple Claude accounts, keyed by the
//! account email, and account switching by writing the shared keychain login
//! plus the `~/.claude.json` identity Claude Code reads. New `claude` sessions
//! use the switched account; already-running sessions keep theirs until
//! restarted.

mod config;
mod logging;
#[cfg(target_os = "macos")]
mod menubar;
mod oauth;
mod store;
mod usage;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};

use store::{Account, CachedUsage, State};

const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
/// Refresh a token if it expires within this many seconds.
const REFRESH_SKEW_SECS: i64 = 300;

// --- watch (auto-swap daemon) defaults ---
/// How often the watcher polls, in seconds.
const WATCH_INTERVAL_SECS: u64 = 150;
/// Upper bound for the poll interval when backing off after a 429.
const WATCH_MAX_INTERVAL_SECS: u64 = 1200;
/// Swap away from the active account when it reaches this utilization.
const TRIGGER_PCT: f64 = 95.0;
/// Only swap to an account at or below this utilization (hysteresis band).
const TARGET_CEILING_PCT: f64 = 85.0;
/// Never swap more often than this.
const SWAP_COOLDOWN_SECS: u64 = 300;
/// Don't return to an account we just left for this long.
const NO_RETURN_SECS: u64 = 1200;
/// Bundle id / label for the launchd agent (runs the menu-bar app at login).
pub(crate) const LAUNCHD_LABEL: &str = "com.claude-usage.menubar";

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => cmd_list(&[]),
        Some("list") | Some("ls") => cmd_list(&args[1..]),
        Some("capture") | Some("add") => cmd_capture(),
        Some("switch") | Some("use") => cmd_switch(args.get(1).map(String::as_str), None),
        Some("start") => cmd_switch(args.get(1).map(String::as_str), Some(Launch::Fresh)),
        Some("continue") | Some("cont") | Some("c") => {
            cmd_switch(args.get(1).map(String::as_str), Some(Launch::Continue))
        }
        Some("token") => cmd_token(args.get(1).map(String::as_str)),
        Some("watch") => cmd_watch(&args[1..]),
        #[cfg(target_os = "macos")]
        Some("menubar") => menubar::run(),
        Some("report") => cmd_report(&args[1..]),
        Some("install") => cmd_install(),
        Some("uninstall") => cmd_uninstall(),
        Some("rm") | Some("remove") => cmd_rm(args.get(1).map(String::as_str)),
        Some("-h") | Some("--help") | Some("help") => {
            print_help();
            Ok(())
        }
        Some("-V") | Some("--version") | Some("version") => {
            println!("claude-usage {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!(
        "claude-usage — usage & instant account switching for Claude\n\n\
         Accounts are identified by their email; commands accept a full email or a\n\
         unique prefix (e.g. `dev` for dev@example.com).\n\n\
         USAGE:\n  \
         claude-usage                   Show cached usage for every account (default)\n  \
         claude-usage list --refresh    Fetch usage now, then show it\n  \
         claude-usage capture           Save the account you're currently logged into\n  \
         claude-usage switch [email]    Make <email> the active login (no launch)\n  \
         claude-usage start [email]     Switch, then launch a fresh `claude`\n  \
         claude-usage continue [email]  Switch, then launch `claude --continue`\n  \
         claude-usage token [email]     Print a fresh access token\n  \
         claude-usage watch             Auto-swap at 95%, keep working (foreground)\n  \
         claude-usage menubar           Run the macOS menu-bar app (usage + auto-swap)\n  \
         claude-usage install           Run the menu-bar app at every login (via launchd)\n  \
         claude-usage uninstall         Stop running the menu-bar app at login\n  \
         claude-usage report            Usage patterns by weekday / hour / account\n  \
         claude-usage rm <email>        Forget an account\n\n\
         With no [email], switch/start/continue auto-pick the account that has room\n  \
         and whose weekly limit resets soonest (use it before the quota resets).\n\n\
         Onboarding: log into an account with `claude` as usual, then\n  \
         `claude-usage capture`. Repeat once per account.\n"
    );
}

#[derive(Clone, Copy)]
enum Launch {
    Fresh,
    Continue,
}

// ---------------------------------------------------------------------------
// capture — snapshot the current keychain login, keyed by its email
// ---------------------------------------------------------------------------

fn cmd_capture() -> Result<()> {
    let (email, existed) = capture_current()?;
    if existed {
        println!("Refreshed {email} — it's the active login.");
    } else {
        println!("Captured {email} — it's the active login.");
    }
    Ok(())
}

/// Capture the account currently in the keychain, keyed by its email. Returns
/// (email, existed_already). Shared by the CLI and the menu bar.
pub(crate) fn capture_current() -> Result<(String, bool)> {
    let blob = keychain_read()
        .context("no claude.ai login found in the keychain — run `claude` and /login first")?;
    let mut acct = Account::from_keychain_blob(&blob)?;
    // Snapshot the account identity Claude stores in ~/.claude.json, so a later
    // switch can restore it (the keychain token alone doesn't set the account).
    let (oauth_account, user_id) = read_claude_identity();
    // Resolve the email (the identity key): prefer the profile API, fall back to
    // the identity object we just read from ~/.claude.json.
    let email = usage::fetch_email(&acct.access_token)
        .or_else(|| {
            oauth_account
                .as_ref()
                .and_then(|o| o.get("emailAddress"))
                .and_then(|x| x.as_str())
                .map(String::from)
        })
        .context(
            "could not determine this account's email (offline?) — connect and try `capture` again",
        )?;
    acct.email = Some(email.clone());
    acct.oauth_account = oauth_account;
    acct.user_id = user_id;

    let existed = with_state_lock(|| {
        let mut state = State::load()?;
        let existed = state.find(&email).is_some();
        state.upsert(acct);
        state.active = Some(email.clone());
        state.save()?;
        Ok(existed)
    })?;
    Ok((email, existed))
}

/// Remove an account by email (used by the CLI `rm` and the menu bar).
pub(crate) fn remove_account(email: &str) -> Result<()> {
    with_state_lock(|| {
        let mut state = State::load()?;
        state.remove(email);
        if state
            .active
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(email))
        {
            state.active = None;
        }
        state.save()
    })
}

// ---------------------------------------------------------------------------
// list (default) — dashboard
// ---------------------------------------------------------------------------

fn cmd_list(args: &[String]) -> Result<()> {
    let refresh = args.iter().any(|a| a == "--refresh" || a == "-r");
    // By default read the cache (no network); --refresh does exactly one fetch.
    if refresh {
        refresh_usage_cache();
    }
    let state = State::load()?;
    if state.accounts.is_empty() {
        println!("No accounts yet. Log into one with `claude`, then: claude-usage capture");
        return Ok(());
    }
    let rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
    render_table(&rows, state.active.as_deref());
    Ok(())
}

// ---------------------------------------------------------------------------
// switch / start / continue
// ---------------------------------------------------------------------------

fn cmd_switch(selector: Option<&str>, launch: Option<Launch>) -> Result<()> {
    let state = State::load()?;
    if state.accounts.is_empty() {
        bail!("no accounts yet; capture one with: claude-usage capture");
    }
    let email = select_email(&state, selector)?;
    let label = switch_to(&email)?;

    println!("Active login is now {label}.");
    println!("New `claude` sessions will use it. Already-running sessions keep their");
    println!("current account until they're restarted.");

    match launch {
        None => Ok(()),
        Some(kind) => {
            println!("\nLaunching claude…\n");
            let mut cmd = std::process::Command::new("claude");
            if let Launch::Continue = kind {
                cmd.arg("--continue");
            }
            match cmd.status() {
                Ok(s) if s.success() => Ok(()),
                Ok(s) => std::process::exit(s.code().unwrap_or(1)),
                Err(e) => bail!("could not launch `claude`: {e}"),
            }
        }
    }
}

/// Resolve `selector` to an account email, or auto-pick when none is given.
fn select_email(state: &State, selector: Option<&str>) -> Result<String> {
    match selector {
        Some(sel) => state.resolve(sel),
        None => {
            let rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
            auto_pick(&rows)
        }
    }
}

/// Make `email` the active login. Does all network work (token refresh, identity
/// backfill) OUTSIDE the state lock, then commits keychain + ~/.claude.json +
/// state under the lock with a fresh reload. Returns the display label.
pub(crate) fn switch_to(email: &str) -> Result<String> {
    // Phase 1 (no lock): refresh the token and resolve the identity over the net.
    let state = State::load()?;
    let mut acct = state
        .find(email)
        .cloned()
        .with_context(|| format!("no account matches '{email}'"))?;
    oauth::ensure_fresh(&mut acct, REFRESH_SKEW_SECS)?;
    let (identity, backfilled) = resolve_identity(&acct)?;
    let label = acct.email.clone().unwrap_or_else(|| email.to_string());

    // Phase 2 (locked): reload, apply the mutation, save. Keeps a concurrent
    // daemon poll or another switch from clobbering this write.
    with_state_lock(|| {
        let mut st = State::load()?;
        // Preserve any token rotation the currently-active session picked up.
        sync_active_from_keychain(&mut st);
        // ~/.claude.json first, keychain last (the commit point), rollback on fail.
        apply_account(&acct, &identity)?;
        if let Some(a) = st.find_mut(email) {
            a.set_tokens(
                acct.access_token.clone(),
                acct.refresh_token.clone(),
                acct.expires_at,
            );
            if backfilled {
                a.oauth_account = Some(identity.clone());
            }
        }
        st.active = Some(email.to_string());
        st.save()?;
        Ok(())
    })?;
    logging::log(&format!("switch -> {label}"));
    Ok(label)
}

/// Resolve the `oauthAccount` identity to write for this account.
/// Returns (identity, backfilled_from_network). Errors if it can't be resolved
/// (e.g. offline and never captured) — the caller then does NOT switch, rather
/// than half-applying one.
fn resolve_identity(acct: &Account) -> Result<(serde_json::Value, bool)> {
    if let Some(v) = &acct.oauth_account {
        return Ok((v.clone(), false));
    }
    let built = usage::fetch_profile(&acct.access_token)
        .as_ref()
        .and_then(usage::oauth_account_from_profile);
    built.map(|v| (v, true)).ok_or_else(|| {
        anyhow!(
            "could not resolve this account's identity (offline?) — \
             run `claude-usage capture` for it while logged in"
        )
    })
}

/// Apply an account as the active login: write ~/.claude.json identity FIRST
/// (atomic tmp+rename), then the keychain token LAST (the flaky commit point).
/// If the keychain write fails, roll ~/.claude.json back to its prior contents
/// so both halves stay consistent, and return Err (never a half-applied switch).
fn apply_account(acct: &Account, identity: &serde_json::Value) -> Result<()> {
    let prior = read_claude_json_raw();
    write_claude_identity(identity, acct.user_id.as_deref())?;
    if let Err(e) = keychain_write(&acct.keychain_blob) {
        if let Some(bytes) = prior {
            let _ = restore_claude_json_raw(&bytes);
        }
        return Err(e).context("writing the account into the keychain");
    }
    Ok(())
}

/// If the account currently in the keychain is genuinely our active account,
/// adopt any token rotation a live `claude` session performed. Verified by
/// identity: `/login` into a *different* account rewrites ~/.claude.json's
/// oauthAccount, so a mismatch means the keychain is not our active account and
/// we must NOT overwrite its stored tokens. In-place rotation keeps the same
/// account uuid/email, so legitimate rotation is still captured.
fn sync_active_from_keychain(state: &mut State) {
    let Some(active) = state.active.clone() else {
        return;
    };
    let Some(blob) = keychain_read() else { return };
    let Ok(fresh) = Account::from_keychain_blob(&blob) else {
        return;
    };
    let (json_oauth, _) = read_claude_identity();
    let json_uuid = json_oauth
        .as_ref()
        .and_then(|o| o.get("accountUuid"))
        .and_then(|x| x.as_str())
        .map(String::from);
    let json_email = json_oauth
        .as_ref()
        .and_then(|o| o.get("emailAddress"))
        .and_then(|x| x.as_str())
        .map(str::to_ascii_lowercase);

    let Some(acct) = state.find_mut(&active) else {
        return;
    };
    // Determine whether the keychain/.claude.json identity matches this account.
    let matches = match (&acct.identity_uuid(), &json_uuid) {
        (Some(a), Some(b)) => a == b,
        _ => match (&acct.email, &json_email) {
            (Some(a), Some(b)) => a.to_ascii_lowercase() == *b,
            // Account has no known identity yet: adopt and self-heal below.
            (None, _) => true,
            // We have an email but .claude.json has none to compare: be safe, skip.
            _ => false,
        },
    };
    if !matches {
        logging::log(
            "sync: keychain identity does not match the active account; not adopting tokens",
        );
        return;
    }
    acct.access_token = fresh.access_token;
    acct.refresh_token = fresh.refresh_token;
    acct.expires_at = fresh.expires_at;
    acct.keychain_blob = fresh.keychain_blob;
    // Self-heal: if we had no identity but .claude.json has one, record it.
    if acct.oauth_account.is_none() {
        if let Some(o) = json_oauth {
            acct.oauth_account = Some(o);
        }
    }
}

// ---------------------------------------------------------------------------
// token
// ---------------------------------------------------------------------------

fn cmd_token(selector: Option<&str>) -> Result<()> {
    let state = State::load()?;
    let email = match selector {
        Some(sel) => state.resolve(sel)?,
        None => match state.accounts.as_slice() {
            [only] => only.key().to_string(),
            [] => bail!("no accounts; capture one with: claude-usage capture"),
            _ => bail!("multiple accounts; specify one by email or prefix"),
        },
    };
    // A token refresh is a network call, but on the token endpoint (not the
    // rate-limited usage endpoint) and only when actually near expiry.
    let token = with_state_lock(|| {
        let mut st = State::load()?;
        let acct = st
            .find_mut(&email)
            .with_context(|| format!("no account matches '{email}'"))?;
        let refreshed = oauth::ensure_fresh(acct, REFRESH_SKEW_SECS)?;
        let token = acct.access_token.clone();
        if refreshed {
            st.save()?;
        }
        Ok(token)
    })?;
    println!("{token}");
    Ok(())
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

fn cmd_rm(selector: Option<&str>) -> Result<()> {
    let selector = selector.context("usage: claude-usage rm <email>")?;
    let state = State::load()?;
    let email = state.resolve(selector)?;
    remove_account(&email)?;
    println!("Removed {email}.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Usage rows + auto-pick
// ---------------------------------------------------------------------------

struct Row {
    email: String,
    session: Cell,
    weekly: Cell,
    opus: Option<Cell>,
    error: Option<String>,
    /// Unix epoch seconds when the cached usage was fetched (None = no data yet).
    fetched_at: Option<i64>,
}

struct Cell {
    pct: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
}

impl Cell {
    fn resets_in(&self) -> String {
        match self.resets_at {
            Some(dt) => humanize_until(dt),
            None => String::new(),
        }
    }
}

/// Build a display row from an account's cached usage (never fetches).
fn row_from_account(a: &Account) -> Row {
    let c = a.cached_usage.as_ref();
    Row {
        email: a.key().to_string(),
        session: cell_from_parts(
            c.and_then(|c| c.session_pct),
            c.and_then(|c| c.session_reset.as_deref()),
        ),
        weekly: cell_from_parts(
            c.and_then(|c| c.weekly_pct),
            c.and_then(|c| c.weekly_reset.as_deref()),
        ),
        opus: c.and_then(|c| {
            c.opus_pct
                .map(|p| cell_from_parts(Some(p), c.opus_reset.as_deref()))
        }),
        error: None,
        fetched_at: c.map(|c| c.fetched_at),
    }
}

/// Convert a fetched `Usage` into the cacheable snapshot.
fn cached_from_usage(u: &usage::Usage) -> CachedUsage {
    let parts = |w: &Option<usage::Window>| {
        w.as_ref()
            .map(|w| (w.utilization, w.resets_at.clone()))
            .unwrap_or((None, None))
    };
    let (session_pct, session_reset) = parts(&u.five_hour);
    let (weekly_pct, weekly_reset) = parts(&u.seven_day);
    let (opus_pct, opus_reset) = parts(&u.seven_day_opus);
    CachedUsage {
        session_pct,
        weekly_pct,
        session_reset,
        weekly_reset,
        opus_pct,
        opus_reset,
        fetched_at: Utc::now().timestamp(),
    }
}

/// Compare two candidate rows for auto-pick / auto-swap: soonest weekly reset
/// first, then MORE headroom (lower usage) first.
fn candidate_order(a: &Row, b: &Row) -> std::cmp::Ordering {
    let ka = a.weekly.resets_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
    let kb = b.weekly.resets_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
    ka.cmp(&kb).then(
        b.headroom()
            .partial_cmp(&a.headroom())
            .unwrap_or(std::cmp::Ordering::Equal),
    )
}

/// Switch to the account auto-pick considers best right now (the one with room
/// whose weekly window resets soonest, so its quota is used before it resets).
/// Uses cached usage only — no network. Returns `Some(email)` if it switched,
/// or `None` if the active account is already the best choice. Used by the
/// menu bar's "Now" item.
pub(crate) fn optimize_now() -> Result<Option<String>> {
    let state = State::load()?;
    let active = state.active.clone();
    let rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
    let best = auto_pick(&rows)?;
    if active.as_deref() == Some(best.as_str()) {
        return Ok(None);
    }
    switch_to(&best)?;
    Ok(Some(best))
}

/// Pick the account with room to spare whose weekly window resets soonest.
/// Operates entirely on cached rows — callers must not fetch first.
fn auto_pick(rows: &[Row]) -> Result<String> {
    if !rows.iter().any(|r| r.has_data()) {
        bail!(
            "no usage data yet — let the menu-bar app or `claude-usage watch` \
             populate it, or pass an explicit account email"
        );
    }
    let mut candidates: Vec<&Row> = rows
        .iter()
        .filter(|r| r.has_data() && r.available())
        .collect();
    if candidates.is_empty() {
        let soonest = rows
            .iter()
            .filter(|r| r.has_data())
            .filter_map(|r| r.weekly.resets_at.map(|dt| (r, dt)))
            .min_by_key(|(_, dt)| *dt);
        match soonest {
            Some((r, dt)) => bail!(
                "all accounts are maxed out; {} resets soonest, in {}",
                r.email,
                humanize_until(dt)
            ),
            None => bail!("no account currently has room"),
        }
    }
    candidates.sort_by(|a, b| candidate_order(a, b));
    let pick = candidates[0];
    println!(
        "Auto-picked {} — weekly resets in {}, {:.0}% headroom.",
        pick.email,
        pick.weekly.resets_in(),
        pick.headroom()
    );
    Ok(pick.email.clone())
}

impl Row {
    /// True once we have a cached usage sample to reason about.
    fn has_data(&self) -> bool {
        self.error.is_none() && self.fetched_at.is_some()
    }

    /// Not blocked and both session and weekly have headroom.
    fn available(&self) -> bool {
        let ok = |c: &Cell| c.pct.map(|p| p < 100.0).unwrap_or(true);
        ok(&self.session) && ok(&self.weekly)
    }

    /// Remaining percent on the tightest of session/weekly.
    fn headroom(&self) -> f64 {
        100.0 - self.max_pct()
    }

    /// Utilization of the tightest of session/weekly.
    fn max_pct(&self) -> f64 {
        self.session
            .pct
            .unwrap_or(0.0)
            .max(self.weekly.pct.unwrap_or(0.0))
    }
}

fn cell_from_parts(pct: Option<f64>, reset: Option<&str>) -> Cell {
    Cell {
        pct,
        resets_at: reset
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
    }
}

// ---------------------------------------------------------------------------
// Cross-process state lock
// ---------------------------------------------------------------------------

/// Run `f` holding an exclusive advisory lock on ~/.config/claude-usage/lock,
/// serializing state read-modify-write across processes (the daemon poll and
/// concurrent CLI/menu commands). The lock is fd-scoped, so the kernel releases
/// it if the holder dies. Do NOT do network I/O inside `f`.
fn with_state_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    use fs2::FileExt;
    let dir = store::config_dir()?;
    std::fs::create_dir_all(&dir).context("creating ~/.config/claude-usage")?;
    let lock_path = dir.join("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .context("opening state lock")?;
    file.lock_exclusive().context("acquiring state lock")?;
    let r = f();
    let _ = file.unlock();
    r
}

// ---------------------------------------------------------------------------
// Keychain helpers (macOS, via Security.framework — no token in argv)
// ---------------------------------------------------------------------------

fn keychain_account() -> String {
    std::env::var("USER").unwrap_or_else(|_| "claude".to_string())
}

// NOTE: we use the `security` CLI rather than the Security.framework
// (`security-framework`) API on purpose. SecItem access from this unsigned,
// brew-installed binary makes macOS prompt on every launch (an unsigned binary
// has no stable identity for "Always Allow" to pin to). The CLI path doesn't
// prompt. The only downside is the write blob appears in `security`'s argv,
// which is LOW risk under this tool's single-user threat model. TODO: switch
// back to security-framework once we ship a code-signed (Developer ID) build,
// where "Always Allow" persists.
#[cfg(target_os = "macos")]
fn keychain_read() -> Option<String> {
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &keychain_account(),
            "-w",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(target_os = "macos")]
fn keychain_write(blob: &str) -> Result<()> {
    let status = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-U", // update if it already exists
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &keychain_account(),
            "-w",
            blob,
        ])
        .status()
        .context("running `security`")?;
    if !status.success() {
        return Err(anyhow!("`security add-generic-password` failed"));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn keychain_read() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
fn keychain_write(_blob: &str) -> Result<()> {
    Err(anyhow!("keychain is only supported on macOS"))
}

fn claude_json_path() -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(std::path::PathBuf::from(home).join(".claude.json"))
}

/// Read the current `oauthAccount` + `userID` from `~/.claude.json`.
fn read_claude_identity() -> (Option<serde_json::Value>, Option<String>) {
    let Ok(path) = claude_json_path() else {
        return (None, None);
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return (None, None);
    };
    let oauth = v.get("oauthAccount").cloned();
    let uid = v.get("userID").and_then(|u| u.as_str()).map(String::from);
    (oauth, uid)
}

/// Raw bytes of ~/.claude.json, for rollback.
fn read_claude_json_raw() -> Option<Vec<u8>> {
    let path = claude_json_path().ok()?;
    std::fs::read(&path).ok()
}

/// Restore ~/.claude.json to prior raw bytes (atomic tmp+rename).
fn restore_claude_json_raw(bytes: &[u8]) -> Result<()> {
    let path = claude_json_path()?;
    let tmp = path.with_extension("json.claude-usage.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Patch `~/.claude.json` so its active identity is this account: set
/// `oauthAccount`, set or (when unknown) REMOVE `userID`, and drop the stale
/// `cachedUsageUtilization`. Atomic, preserves the file mode, cleans the tmp on
/// failure.
fn write_claude_identity(oauth_account: &serde_json::Value, user_id: Option<&str>) -> Result<()> {
    let path = claude_json_path()?;
    let bytes = std::fs::read(&path).context("reading ~/.claude.json")?;
    let mut v: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing ~/.claude.json")?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow!("~/.claude.json is not a JSON object"))?;
    obj.insert("oauthAccount".into(), oauth_account.clone());
    match user_id {
        Some(uid) => {
            obj.insert("userID".into(), serde_json::Value::String(uid.to_string()));
        }
        // Don't leave the previous account's userID paired with a new identity.
        None => {
            obj.remove("userID");
        }
    }
    obj.remove("cachedUsageUtilization");
    let json = serde_json::to_vec_pretty(&v)?;
    let tmp = path.with_extension("json.claude-usage.tmp");
    if let Err(e) = std::fs::write(&tmp, &json) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("writing ~/.claude.json.tmp");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("replacing ~/.claude.json");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Usage refresh (the ONE network path)
// ---------------------------------------------------------------------------

/// Outcome of a usage refresh.
struct RefreshOutcome {
    /// True if any account got HTTP 429 (caller should back off).
    rate_limited: bool,
}

/// The ONE place that calls the usage API. Does all network work (token refresh
/// if near expiry, usage fetch) OUTSIDE the state lock, then takes the lock,
/// reloads state, merges the fresh tokens + `cached_usage` by email, and saves —
/// so a concurrent switch is never clobbered. On 429/transient error it KEEPS
/// the existing cache.
fn refresh_usage_cache() -> RefreshOutcome {
    let mut state = match State::load() {
        Ok(s) => s,
        Err(e) => {
            logging::log(&format!("poll: state load failed: {e}"));
            return RefreshOutcome {
                rate_limited: false,
            };
        }
    };
    sync_active_from_keychain(&mut state);
    logging::log("poll: refreshing usage cache");

    let mut rate_limited = false;
    // (email, refreshed account after ensure_fresh, new cached usage or None)
    let mut updates: Vec<(String, Account, Option<CachedUsage>)> = Vec::new();
    let emails: Vec<String> = state.accounts.iter().map(|a| a.key().to_string()).collect();
    for email in &emails {
        let Some(acct) = state.find(email).cloned() else {
            continue;
        };
        let mut acct = acct;
        if let Err(e) = oauth::ensure_fresh(&mut acct, REFRESH_SKEW_SECS) {
            logging::log(&format!(
                "token refresh failed for {email}: {e} (keeping cache)"
            ));
            continue;
        }
        let cu = match usage::fetch(&acct.access_token) {
            Ok(u) => Some(cached_from_usage(&u)),
            Err(usage::FetchError::RateLimited) => {
                rate_limited = true;
                logging::log(&format!("usage 429 for {email}; keeping cache"));
                None
            }
            Err(e) => {
                logging::log(&format!("usage error for {email}: {e}; keeping cache"));
                None
            }
        };
        updates.push((email.clone(), acct, cu));
    }

    // Merge under the lock with a fresh reload so we don't clobber a switch.
    let merged = with_state_lock(|| {
        let mut st = State::load()?;
        for (email, acct, cu) in &updates {
            if let Some(a) = st.find_mut(email) {
                a.set_tokens(
                    acct.access_token.clone(),
                    acct.refresh_token.clone(),
                    acct.expires_at,
                );
                if let Some(cu) = cu {
                    a.cached_usage = Some(cu.clone());
                }
            }
        }
        st.save()?;
        Ok(())
    });
    if let Err(e) = merged {
        logging::log(&format!("poll: saving refreshed cache failed: {e:#}"));
    }
    logging::log("poll: done");
    RefreshOutcome { rate_limited }
}

// ---------------------------------------------------------------------------
// watch — auto-swap daemon
// ---------------------------------------------------------------------------

fn cmd_watch(args: &[String]) -> Result<()> {
    let mut interval = WATCH_INTERVAL_SECS;
    let mut trigger = TRIGGER_PCT;
    let mut ceiling = TARGET_CEILING_PCT;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--interval" => interval = it.next().and_then(|s| s.parse().ok()).unwrap_or(interval),
            "--trigger" => trigger = it.next().and_then(|s| s.parse().ok()).unwrap_or(trigger),
            "--ceiling" => ceiling = it.next().and_then(|s| s.parse().ok()).unwrap_or(ceiling),
            other => bail!("unknown watch option: {other}"),
        }
    }

    eprintln!(
        "claude-usage watch: every {interval}s, swap at {trigger:.0}%, target <= {ceiling:.0}%"
    );

    let base = interval;
    let mut current = base;
    let mut guard = SwapGuard::default();
    loop {
        match watch_cycle(trigger, ceiling, &mut guard) {
            Ok(outcome) => {
                if let Some((from, to)) = outcome.swapped {
                    eprintln!("[{}] swapped {from} -> {to}", Utc::now().to_rfc3339());
                }
                current = next_interval(current, base, outcome.rate_limited);
            }
            Err(e) => eprintln!("watch cycle error: {e:#}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(current));
    }
}

/// Compute the next poll interval: exponential backoff (doubling, capped) after
/// a rate limit, reset to the base cadence on a clean cycle.
fn next_interval(current: u64, base: u64, rate_limited: bool) -> u64 {
    if rate_limited {
        let next = (current.max(base) * 2).min(WATCH_MAX_INTERVAL_SECS);
        logging::log(&format!("rate limited; backing off to {next}s"));
        next
    } else {
        base
    }
}

/// Anti-thrash state carried across watch cycles.
#[derive(Default)]
pub(crate) struct SwapGuard {
    last_swap: Option<std::time::Instant>,
    left_at: std::collections::HashMap<String, std::time::Instant>,
    stuck_notified: bool,
}

/// Result of one poll: the swap it made (if any) and the rate-limited flag.
pub(crate) struct CycleOutcome {
    pub swapped: Option<(String, String)>,
    pub rate_limited: bool,
}

/// A pure auto-swap decision over cached rows. Returns the email to swap to, or
/// None (with `guard` consulted for cooldown / no-return). Extracted for tests.
fn choose_swap_target(
    rows: &[Row],
    active: &str,
    trigger: f64,
    ceiling: f64,
    guard: &SwapGuard,
) -> Option<String> {
    let act = rows.iter().find(|r| r.email == active)?;
    if !act.has_data() || act.max_pct() < trigger {
        return None;
    }
    if guard
        .last_swap
        .map(|t| t.elapsed().as_secs() < SWAP_COOLDOWN_SECS)
        .unwrap_or(false)
    {
        return None;
    }
    let mut candidates: Vec<&Row> = rows
        .iter()
        .filter(|r| r.has_data() && r.email != active && r.available() && r.max_pct() <= ceiling)
        .filter(|r| {
            guard
                .left_at
                .get(&r.email)
                .map(|t| t.elapsed().as_secs() >= NO_RETURN_SECS)
                .unwrap_or(true)
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| candidate_order(a, b));
    Some(candidates[0].email.clone())
}

/// Poll usage for every account (the only network path), record history, and
/// auto-swap away from the active account if it has reached `trigger` and a
/// healthy target exists. Shared by `claude-usage watch` and the menu-bar poller.
fn watch_cycle(trigger: f64, ceiling: f64, guard: &mut SwapGuard) -> Result<CycleOutcome> {
    let refresh = refresh_usage_cache();
    let state = State::load()?;
    if state.accounts.is_empty() {
        return Ok(CycleOutcome {
            swapped: None,
            rate_limited: refresh.rate_limited,
        });
    }
    let rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
    append_history(&rows, state.active.as_deref());

    let active = state.active.clone();
    let mut swapped = None;

    if let Some(active_email) = active.clone() {
        match choose_swap_target(&rows, &active_email, trigger, ceiling, guard) {
            Some(target) => {
                let (pick_s, pick_w) = rows
                    .iter()
                    .find(|r| r.email == target)
                    .map(|r| (r.session.pct.unwrap_or(0.0), r.weekly.pct.unwrap_or(0.0)))
                    .unwrap_or((0.0, 0.0));
                let label = switch_to(&target)?;
                guard
                    .left_at
                    .insert(active_email.clone(), std::time::Instant::now());
                guard.last_swap = Some(std::time::Instant::now());
                guard.stuck_notified = false;
                log_event(&serde_json::json!({
                    "ts": Utc::now().timestamp(),
                    "event": "swap",
                    "from": active_email,
                    "to": target,
                    "session": pick_s,
                    "weekly": pick_w,
                }));
                notify(&format!(
                    "Switched to {label} — {pick_s:.0}% / {pick_w:.0}%"
                ));
                swapped = Some((active_email, target));
            }
            None => {
                // If the active account is over trigger but nothing is eligible,
                // notify once that we're stuck.
                let act_over = rows
                    .iter()
                    .find(|r| r.email == active_email)
                    .map(|r| r.has_data() && r.max_pct() >= trigger)
                    .unwrap_or(false);
                if act_over && !guard.stuck_notified {
                    let soonest = rows
                        .iter()
                        .filter(|r| r.has_data())
                        .filter_map(|r| r.weekly.resets_at.map(humanize_until))
                        .min()
                        .unwrap_or_else(|| "unknown".to_string());
                    notify(&format!(
                        "All accounts high — staying on {active_email}, soonest reset in {soonest}"
                    ));
                    guard.stuck_notified = true;
                } else if !act_over {
                    guard.stuck_notified = false;
                }
            }
        }
    }

    Ok(CycleOutcome {
        swapped,
        rate_limited: refresh.rate_limited,
    })
}

/// Fire a native macOS notification (best effort).
fn notify(msg: &str) {
    let script = format!("display notification {msg:?} with title \"claude-usage\"");
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status();
}

// ---------------------------------------------------------------------------
// History logging + reporting
// ---------------------------------------------------------------------------

/// Rotate a log file once it grows past this size (~1 MB).
const HISTORY_MAX_BYTES: u64 = 1_000_000;

fn history_path() -> Result<std::path::PathBuf> {
    Ok(store::config_dir()?.join("history.jsonl"))
}

fn log_event(v: &serde_json::Value) {
    use std::io::Write;
    let Ok(path) = history_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Rotate if it has grown too large (keep one previous generation).
    if let Ok(m) = std::fs::metadata(&path) {
        if m.len() > HISTORY_MAX_BYTES {
            let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{v}");
    }
}

fn append_history(rows: &[Row], active: Option<&str>) {
    let ts = Utc::now().timestamp();
    for r in rows {
        if !r.has_data() {
            continue;
        }
        log_event(&serde_json::json!({
            "ts": ts,
            "account": r.email,
            "active": active == Some(r.email.as_str()),
            "session": r.session.pct,
            "weekly": r.weekly.pct,
        }));
    }
}

#[derive(serde::Deserialize)]
struct Sample {
    ts: i64,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    session: Option<f64>,
    #[serde(default)]
    weekly: Option<f64>,
    #[serde(default)]
    event: Option<String>,
}

fn cmd_report(_args: &[String]) -> Result<()> {
    use chrono::{Datelike, Local, TimeZone, Timelike};

    let path = history_path()?;
    let data = std::fs::read_to_string(&path)
        .context("no history yet — run `claude-usage watch` (or `install`) to collect it")?;
    let samples: Vec<Sample> = data
        .lines()
        .filter_map(|l| serde_json::from_str::<Sample>(l).ok())
        .collect();
    if samples.is_empty() {
        println!("No usage samples recorded yet.");
        return Ok(());
    }

    let mut active: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.event.is_none() && s.active == Some(true))
        .collect();
    active.sort_by_key(|s| s.ts);

    let mut by_weekday = [0f64; 7];
    let mut by_hour = [0f64; 24];
    let mut prev: Option<&Sample> = None;
    for s in &active {
        if let (Some(p), Some(cur)) = (prev.and_then(|p| p.session), s.session) {
            let delta = cur - p;
            if delta > 0.0 {
                if let Some(dt) = Local.timestamp_opt(s.ts, 0).single() {
                    by_weekday[dt.weekday().num_days_from_monday() as usize] += delta;
                    by_hour[dt.hour() as usize] += delta;
                }
            }
        }
        prev = Some(s);
    }

    let mut peak: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    for s in &samples {
        if let (Some(a), Some(w)) = (&s.account, s.weekly) {
            let e = peak.entry(a.clone()).or_insert(0.0);
            if w > *e {
                *e = w;
            }
        }
    }
    let swaps = samples
        .iter()
        .filter(|s| s.event.as_deref() == Some("swap"))
        .count();
    let span_start = samples
        .first()
        .and_then(|s| Local.timestamp_opt(s.ts, 0).single());
    let span_end = samples
        .last()
        .and_then(|s| Local.timestamp_opt(s.ts, 0).single());

    println!("\nUsage report");
    if let (Some(a), Some(b)) = (span_start, span_end) {
        println!(
            "  period: {} → {}",
            a.format("%Y-%m-%d %H:%M"),
            b.format("%Y-%m-%d %H:%M")
        );
    }
    println!("  samples: {}   swaps: {swaps}\n", samples.len());

    let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    println!("Consumption by weekday (relative):");
    print_bars(
        &days.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        &by_weekday,
    );
    println!("\nConsumption by hour of day (relative):");
    let hours: Vec<String> = (0..24).map(|h| format!("{h:02}")).collect();
    print_bars(&hours, &by_hour);

    println!("\nPeak weekly utilization per account:");
    for (email, p) in &peak {
        println!("  {:<28} {}", email, bar(Some(*p)));
    }
    let maxpeak = peak.values().cloned().fold(0.0_f64, f64::max);
    println!();
    if maxpeak < 80.0 {
        println!(
            "One account peaked at only {maxpeak:.0}% weekly — a single subscription likely covers your usage."
        );
    } else if swaps == 0 {
        println!("You approached your weekly limit but never needed a swap — one account is close to enough.");
    } else {
        println!("You hit {swaps} swap(s) — multiple accounts are earning their keep.");
    }
    println!();
    Ok(())
}

fn print_bars(labels: &[String], values: &[f64]) {
    let max = values.iter().cloned().fold(0.0_f64, f64::max).max(1e-9);
    for (label, v) in labels.iter().zip(values.iter()) {
        let filled = ((v / max) * 30.0).round() as usize;
        let b: String = "█".repeat(filled) + &" ".repeat(30 - filled);
        println!("  {label:<4} |{b}| {v:>5.1}");
    }
}

// ---------------------------------------------------------------------------
// launchd install / uninstall
// ---------------------------------------------------------------------------

fn plist_path() -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(std::path::PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

/// Path to invoke for the login item and for a post-upgrade relaunch, chosen to
/// survive `brew upgrade`. `current_exe()` resolves symlinks to the versioned
/// Homebrew Cellar path, which an upgrade deletes; map that back to the stable
/// `<prefix>/bin/claude-usage` symlink brew keeps repointing. For a from-source
/// install the resolved path is already stable.
pub(crate) fn stable_exe_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let s = exe.to_string_lossy();
    if let Some(idx) = s.find("/Cellar/claude-usage/") {
        let stable = std::path::PathBuf::from(format!("{}/bin/claude-usage", &s[..idx]));
        if stable.exists() {
            return stable;
        }
    }
    exe
}

fn cmd_install() -> Result<()> {
    let exe = stable_exe_path();
    // No StandardOut/ErrorPath: the app has its own rotated ~/.config log; letting
    // launchd capture stdout/stderr would accumulate an uncapped file forever.
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>menubar</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        exe = exe.display(),
    );
    let path = plist_path()?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, plist).context("writing LaunchAgent plist")?;

    let _ = std::process::Command::new("launchctl")
        .args(["unload", &path.to_string_lossy()])
        .status();
    let status = std::process::Command::new("launchctl")
        .args(["load", "-w", &path.to_string_lossy()])
        .status()
        .context("launchctl load")?;
    if !status.success() {
        bail!("launchctl load failed for {}", path.display());
    }
    println!("Installed and started the claude-usage menu bar app — it now runs at every login.");
    println!(
        "Logs: {}",
        store::config_dir()?.join("claude-usage.log").display()
    );
    Ok(())
}

fn cmd_uninstall() -> Result<()> {
    let path = plist_path()?;
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &path.to_string_lossy()])
        .status();
    if path.exists() {
        std::fs::remove_file(&path).context("removing plist")?;
    }
    println!("Uninstalled — the menu-bar app will no longer start at login.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_table(rows: &[Row], active: Option<&str>) {
    let has_opus = rows.iter().any(|r| r.opus.is_some());
    println!();
    let mut header = format!(
        "{:<2} {:<28} {:<22} {:<11}",
        "", "ACCOUNT", "SESSION (5h)", "RESETS IN"
    );
    header.push_str(&format!("  {:<22} {:<11}", "WEEKLY (7d)", "RESETS IN"));
    if has_opus {
        header.push_str(&format!("  {:<22} {:<11}", "WEEKLY OPUS", "RESETS IN"));
    }
    header.push_str(&format!("  {:<10}", "UPDATED"));
    println!("{header}");
    println!("{}", "-".repeat(header.len()));

    for r in rows {
        let marker = if active == Some(r.email.as_str()) {
            "▶"
        } else {
            " "
        };
        if !r.has_data() {
            println!(
                "{marker}  {:<28} no data yet (run the menu-bar app or `claude-usage watch`)",
                truncate(&r.email, 28),
            );
            continue;
        }
        let mut line = format!(
            "{marker}  {:<28} {:<22} {:<11}",
            truncate(&r.email, 28),
            bar(r.session.pct),
            r.session.resets_in(),
        );
        line.push_str(&format!(
            "  {:<22} {:<11}",
            bar(r.weekly.pct),
            r.weekly.resets_in()
        ));
        if has_opus {
            match &r.opus {
                Some(c) => line.push_str(&format!("  {:<22} {:<11}", bar(c.pct), c.resets_in())),
                None => line.push_str(&format!("  {:<22} {:<11}", "-", "")),
            }
        }
        line.push_str(&format!("  {:<10}", age_str(r.fetched_at)));
        println!("{line}");
    }
    println!();
    if active.is_none() {
        println!("(no active account tracked yet — `capture` the one you're on)");
    }
    println!(
        "Usage updates on a schedule (menu-bar app / `claude-usage watch`). \
         Run `claude-usage list --refresh` to fetch now.\n"
    );
}

/// Human-friendly "time since" for a cached-usage timestamp.
fn age_str(fetched_at: Option<i64>) -> String {
    let Some(ts) = fetched_at else {
        return "never".to_string();
    };
    let secs = Utc::now().timestamp().saturating_sub(ts);
    if secs < 0 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// A compact text bar like `[####------]  40%`.
fn bar(pct: Option<f64>) -> String {
    match pct {
        Some(p) => {
            let p = p.clamp(0.0, 100.0);
            let filled = ((p / 10.0).round() as usize).min(10);
            let b: String = "#".repeat(filled) + &"-".repeat(10 - filled);
            format!("[{b}] {p:>3.0}%")
        }
        None => "-".to_string(),
    }
}

fn humanize_until(dt: DateTime<Utc>) -> String {
    let secs = dt.timestamp() - Utc::now().timestamp();
    if secs <= 0 {
        return "now".to_string();
    }
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
