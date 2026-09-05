//! claude-usage — usage/limits across multiple Claude accounts, and instant
//! account switching by writing the shared keychain login (the same thing
//! `/login` persists), which every running `claude` adopts on its next request.

mod config;
mod logging;
#[cfg(target_os = "macos")]
mod menubar;
mod oauth;
mod store;
mod usage;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use std::io::Write;

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
const LAUNCHD_LABEL: &str = "com.claude-usage.menubar";

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
        Some("capture") | Some("add") => cmd_capture(args.get(1)),
        Some("switch") | Some("use") => cmd_switch(args.get(1).map(String::as_str), None),
        Some("start") => cmd_switch(args.get(1).map(String::as_str), Some(Launch::Fresh)),
        Some("continue") | Some("cont") | Some("c") => {
            cmd_switch(args.get(1).map(String::as_str), Some(Launch::Continue))
        }
        Some("token") => cmd_token(args.get(1)),
        Some("watch") => cmd_watch(&args[1..]),
        #[cfg(target_os = "macos")]
        Some("menubar") => menubar::run(),
        Some("report") => cmd_report(&args[1..]),
        Some("install") => cmd_install(),
        Some("uninstall") => cmd_uninstall(),
        Some("rm") | Some("remove") => cmd_rm(args.get(1)),
        Some("rename") | Some("mv") => cmd_rename(args.get(1), args.get(2)),
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
         USAGE:\n  \
         claude-usage                   Show cached usage for every account (default)\n  \
         claude-usage list --refresh    Fetch usage now, then show it\n  \
         claude-usage capture <name>    Save the account you're currently logged into\n  \
         claude-usage switch [name]     Make <name> the active login (no launch)\n  \
         claude-usage start [name]      Switch, then launch a fresh `claude`\n  \
         claude-usage continue [name]   Switch, then launch `claude --continue`\n  \
         claude-usage token <name>      Print a fresh access token\n  \
         claude-usage watch             Auto-swap at 95%, keep working (foreground)\n  \
         claude-usage menubar           Run the macOS menu-bar app (usage + auto-swap)\n  \
         claude-usage install           Run the menu-bar app at every login (via launchd)\n  \
         claude-usage uninstall         Stop running the menu-bar app at login\n  \
         claude-usage report            Usage patterns by weekday / hour / account\n  \
         claude-usage rename <old> <new>  Rename an account\n  \
         claude-usage rm <name>         Forget an account\n\n\
         With no [name], switch/start/continue auto-pick the account that has room\n  \
         and whose weekly limit resets soonest (use it before the quota resets).\n\n\
         Onboarding: log into an account with `claude` as usual, then\n  \
         `claude-usage capture <name>`. Repeat once per account.\n"
    );
}

#[derive(Clone, Copy)]
enum Launch {
    Fresh,
    Continue,
}

// ---------------------------------------------------------------------------
// capture — snapshot the current keychain login under a name
// ---------------------------------------------------------------------------

fn cmd_capture(name: Option<&String>) -> Result<()> {
    let name = name
        .context("usage: claude-usage capture <name>")?
        .trim()
        .to_string();
    if name.is_empty() {
        bail!("account name cannot be empty");
    }
    let blob = keychain_read()
        .context("no claude.ai login found in the keychain — run `claude` and /login first")?;
    let mut acct = Account::from_keychain_blob(name.clone(), &blob)?;
    acct.email = usage::fetch_email(&acct.access_token);
    // Snapshot the account identity Claude stores in ~/.claude.json, so a later
    // switch can restore it (the keychain token alone doesn't set the account).
    let (oauth_account, user_id) = read_claude_identity();
    acct.oauth_account = oauth_account;
    acct.user_id = user_id;

    let mut state = State::load()?;
    state.upsert(acct);
    state.active = Some(name.clone());
    state.save()?;

    match state.find(&name).and_then(|a| a.email.clone()) {
        Some(email) => println!("Captured '{name}' ({email}) — it's the active login."),
        None => println!("Captured '{name}' — it's the active login."),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// list (default) — dashboard
// ---------------------------------------------------------------------------

fn cmd_list(args: &[String]) -> Result<()> {
    let refresh = args.iter().any(|a| a == "--refresh" || a == "-r");
    let mut state = State::load()?;
    if state.accounts.is_empty() {
        println!("No accounts yet. Log into one with `claude`, then: claude-usage capture <name>");
        return Ok(());
    }
    // By default read the cache (no network); --refresh does exactly one fetch.
    if refresh {
        refresh_usage_cache(&mut state);
    }
    let rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
    render_table(&rows, state.active.as_deref());
    Ok(())
}

// ---------------------------------------------------------------------------
// switch / start / continue
// ---------------------------------------------------------------------------

fn cmd_switch(name: Option<&str>, launch: Option<Launch>) -> Result<()> {
    let mut state = State::load()?;
    if state.accounts.is_empty() {
        bail!("no accounts yet; capture one with: claude-usage capture <name>");
    }

    // Before changing the keychain, save any token rotation the currently
    // active account picked up while it was live, so we never lose its login.
    sync_active_from_keychain(&mut state);

    let name = match name {
        Some(n) => n.to_string(),
        None => {
            // Auto-pick reads the cached usage only — never hits the network.
            let rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
            auto_pick(&rows)?
        }
    };

    let acct = state
        .find_mut(&name)
        .with_context(|| format!("no account named '{name}'"))?;
    // Make sure we hand Claude a token that isn't about to expire.
    oauth::ensure_fresh(acct, REFRESH_SKEW_SECS)?;
    let acct = acct.clone();
    let label = format!("{}{}", acct.name, email_suffix(&acct));

    // Write keychain + ~/.claude.json identity. If the identity had to be
    // backfilled from the profile API, persist it so future switches are
    // fully network-free.
    let backfilled = apply_account(&acct).context("switching the active account")?;
    if let Some(v) = backfilled {
        if let Some(a) = state.find_mut(&name) {
            a.oauth_account = Some(v);
        }
    }
    state.active = Some(acct.name.clone());
    state.save()?;
    logging::log(&format!("switch -> {}", acct.name));

    println!("Active login is now '{label}'.");
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

/// If the account currently in the keychain is one we track, refresh our
/// stored blob from it (captures token rotation done by live sessions).
fn sync_active_from_keychain(state: &mut State) {
    let Some(active) = state.active.clone() else {
        return;
    };
    let Some(blob) = keychain_read() else { return };
    if let Ok(fresh) = Account::from_keychain_blob(active.clone(), &blob) {
        if let Some(acct) = state.find_mut(&active) {
            // Only adopt if it's really the same account (refresh token lineage
            // changes on rotation, but the keychain is the source of truth here).
            acct.access_token = fresh.access_token;
            acct.refresh_token = fresh.refresh_token;
            acct.expires_at = fresh.expires_at;
            acct.keychain_blob = fresh.keychain_blob;
        }
    }
}

// ---------------------------------------------------------------------------
// token
// ---------------------------------------------------------------------------

fn cmd_token(name: Option<&String>) -> Result<()> {
    let name = resolve_name(name)?;
    let mut state = State::load()?;
    let acct = state
        .find_mut(&name)
        .with_context(|| format!("no account named '{name}'"))?;
    if oauth::ensure_fresh(acct, REFRESH_SKEW_SECS)? {
        let token = acct.access_token.clone();
        state.save()?;
        println!("{token}");
    } else {
        println!("{}", acct.access_token);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

fn cmd_rm(name: Option<&String>) -> Result<()> {
    let name = name.context("usage: claude-usage rm <name>")?;
    let mut state = State::load()?;
    if state.remove(name) {
        if state
            .active
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(name))
        {
            state.active = None;
        }
        state.save()?;
        println!("Removed '{name}'.");
    } else {
        bail!("no account named '{name}'");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rename
// ---------------------------------------------------------------------------

fn cmd_rename(old: Option<&String>, new: Option<&String>) -> Result<()> {
    let old = old.context("usage: claude-usage rename <old> <new>")?;
    let new = new.context("usage: claude-usage rename <old> <new>")?;
    let mut state = State::load()?;
    state.rename(old, new)?;
    state.save()?;
    println!("Renamed '{old}' to '{}'.", new.trim());
    Ok(())
}

// ---------------------------------------------------------------------------
// Usage collection + auto-pick
// ---------------------------------------------------------------------------

struct Row {
    name: String,
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
        name: a.name.clone(),
        email: a.email.clone().unwrap_or_default(),
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

/// Pick the account with room to spare whose weekly window resets soonest.
/// Operates entirely on cached rows — callers must not fetch first.
fn auto_pick(rows: &[Row]) -> Result<String> {
    if !rows.iter().any(|r| r.has_data()) {
        bail!(
            "no usage data yet — let the menu-bar app or `claude-usage watch` \
             populate it, or pass an explicit account name"
        );
    }
    let mut candidates: Vec<&Row> = rows
        .iter()
        .filter(|r| r.has_data() && r.available())
        .collect();
    if candidates.is_empty() {
        // Nothing has room; report the soonest reset so the user knows the wait.
        let soonest = rows
            .iter()
            .filter(|r| r.has_data())
            .filter_map(|r| r.weekly.resets_at.map(|dt| (r, dt)))
            .min_by_key(|(_, dt)| *dt);
        match soonest {
            Some((r, dt)) => bail!(
                "all accounts are maxed out; '{}' resets soonest, in {}",
                r.name,
                humanize_until(dt)
            ),
            None => bail!("no account currently has room"),
        }
    }
    // Soonest weekly reset first; tie-break on lower usage.
    candidates.sort_by(|a, b| {
        let ka = a.weekly.resets_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
        let kb = b.weekly.resets_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
        ka.cmp(&kb).then(
            a.headroom()
                .partial_cmp(&b.headroom())
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    let pick = candidates[0];
    println!(
        "Auto-picked '{}' — weekly resets in {}, {:.0}% headroom.",
        pick.name,
        pick.weekly.resets_in(),
        pick.headroom()
    );
    Ok(pick.name.clone())
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
// Keychain helpers (macOS)
// ---------------------------------------------------------------------------

fn keychain_account() -> String {
    std::env::var("USER").unwrap_or_else(|_| "claude".to_string())
}

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

fn claude_json_path() -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(std::path::PathBuf::from(home).join(".claude.json"))
}

/// Read the current `oauthAccount` + `userID` from `~/.claude.json` (used at
/// capture time to snapshot the account's identity).
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

/// Patch `~/.claude.json` so its active identity is this account: set
/// `oauthAccount` and `userID`, and drop the now-stale `cachedUsageUtilization`.
/// This is the half of a switch that actually changes which account Claude uses.
fn write_claude_identity(oauth_account: &serde_json::Value, user_id: Option<&str>) -> Result<()> {
    let path = claude_json_path()?;
    let bytes = std::fs::read(&path).context("reading ~/.claude.json")?;
    let mut v: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing ~/.claude.json")?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow!("~/.claude.json is not a JSON object"))?;
    obj.insert("oauthAccount".into(), oauth_account.clone());
    if let Some(uid) = user_id {
        obj.insert("userID".into(), serde_json::Value::String(uid.to_string()));
    }
    obj.remove("cachedUsageUtilization");
    // Write atomically to avoid corrupting the file if we're interrupted, and
    // preserve the original file's permissions so we never widen them.
    let json = serde_json::to_vec_pretty(&v)?;
    let tmp = path.with_extension("json.claude-usage.tmp");
    std::fs::write(&tmp, &json).context("writing ~/.claude.json.tmp")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
    }
    std::fs::rename(&tmp, &path).context("replacing ~/.claude.json")?;
    Ok(())
}

/// Apply an account as the active login: write its keychain token AND its
/// `~/.claude.json` identity (backfilling the identity from the profile API if
/// it wasn't captured). Both halves are required for the switch to take effect.
///
/// Returns `Some(identity)` when the identity had to be backfilled from the
/// profile API, so the caller can persist it and avoid future network calls.
fn apply_account(acct: &Account) -> Result<Option<serde_json::Value>> {
    keychain_write(&acct.keychain_blob)?;
    match &acct.oauth_account {
        Some(v) => {
            write_claude_identity(v, acct.user_id.as_deref())?;
            Ok(None)
        }
        None => {
            let built = usage::fetch_profile(&acct.access_token)
                .as_ref()
                .and_then(usage::oauth_account_from_profile);
            match &built {
                Some(v) => write_claude_identity(v, acct.user_id.as_deref())?,
                None => eprintln!(
                    "warning: could not set the account identity in ~/.claude.json \
                     (re-run `claude-usage capture {}` after logging in)",
                    acct.name
                ),
            }
            Ok(built)
        }
    }
}

/// Write an account into the keychain + .claude.json and mark it active.
/// Returns its label. Persists a backfilled identity so later switches never
/// touch the network.
fn perform_switch(state: &mut State, name: &str) -> Result<String> {
    let acct = state
        .find_mut(name)
        .with_context(|| format!("no account named '{name}'"))?;
    oauth::ensure_fresh(acct, REFRESH_SKEW_SECS)?;
    let acct = acct.clone();
    let label = acct.email.clone().unwrap_or_else(|| acct.name.clone());
    let backfilled = apply_account(&acct)?;
    if let Some(v) = backfilled {
        if let Some(a) = state.find_mut(name) {
            a.oauth_account = Some(v);
        }
    }
    state.active = Some(acct.name.clone());
    state.save()?;
    logging::log(&format!("switch(auto) -> {}", acct.name));
    Ok(label)
}

/// Outcome of a usage refresh.
struct RefreshOutcome {
    /// True if any account got HTTP 429 (caller should back off).
    rate_limited: bool,
}

/// The ONE place that calls the usage API. Refreshes each account's token (only
/// if near expiry), fetches usage, and writes it to `cached_usage`. On a 429 or
/// transient error it KEEPS the existing cache instead of clearing it. Saves the
/// state and returns whether a rate limit was hit.
fn refresh_usage_cache(state: &mut State) -> RefreshOutcome {
    // Keep the active account's stored tokens honest before we call the API.
    sync_active_from_keychain(state);

    logging::log("poll: refreshing usage cache");
    let mut rate_limited = false;
    let names: Vec<String> = state.accounts.iter().map(|a| a.name.clone()).collect();
    for name in &names {
        let Some(acct) = state.find_mut(name) else {
            continue;
        };
        if let Err(e) = oauth::ensure_fresh(acct, REFRESH_SKEW_SECS) {
            logging::log(&format!(
                "token refresh failed for {name}: {e} (keeping cache)"
            ));
            continue;
        }
        let token = acct.access_token.clone();
        match usage::fetch(&token) {
            Ok(u) => {
                let cu = cached_from_usage(&u);
                if let Some(a) = state.find_mut(name) {
                    a.cached_usage = Some(cu);
                }
                logging::log(&format!("usage ok for {name}"));
            }
            Err(usage::FetchError::RateLimited) => {
                rate_limited = true;
                logging::log(&format!("usage 429 for {name}; keeping cache"));
            }
            Err(e) => {
                logging::log(&format!("usage error for {name}: {e}; keeping cache"));
            }
        }
    }
    let _ = state.save();
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
struct SwapGuard {
    last_swap: Option<std::time::Instant>,
    left_at: std::collections::HashMap<String, std::time::Instant>,
    stuck_notified: bool,
}

/// Result of one poll: the current rows, the active account, the swap it made
/// (if any), and whether it was rate limited. Callers render/log this.
struct CycleOutcome {
    rows: Vec<Row>,
    active: Option<String>,
    swapped: Option<(String, String)>,
    rate_limited: bool,
}

/// Poll usage for every account (the only network path), record history, and
/// auto-swap away from the active account if it has reached `trigger` and a
/// healthy target exists. Shared by `claude-usage watch` and the menu-bar poller.
fn watch_cycle(trigger: f64, ceiling: f64, guard: &mut SwapGuard) -> Result<CycleOutcome> {
    let mut state = State::load()?;
    if state.accounts.is_empty() {
        return Ok(CycleOutcome {
            rows: Vec::new(),
            active: None,
            swapped: None,
            rate_limited: false,
        });
    }
    let refresh = refresh_usage_cache(&mut state);
    let rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
    append_history(&rows, state.active.as_deref());

    let active = state.active.clone();
    let mut swapped = None;

    'decide: {
        let Some(active_name) = active.clone() else {
            break 'decide;
        };
        // Extract the active account's scalars so the borrow of `rows` ends here.
        let Some((act_nodata, act_max, act_s, act_w)) =
            rows.iter().find(|r| r.name == active_name).map(|r| {
                (
                    !r.has_data(),
                    r.max_pct(),
                    r.session.pct.unwrap_or(0.0),
                    r.weekly.pct.unwrap_or(0.0),
                )
            })
        else {
            break 'decide;
        };

        if act_nodata || act_max < trigger {
            guard.stuck_notified = false;
            break 'decide;
        }
        // Respect the global swap cooldown.
        if guard
            .last_swap
            .map(|t| t.elapsed().as_secs() < SWAP_COOLDOWN_SECS)
            .unwrap_or(false)
        {
            break 'decide;
        }

        // Eligible targets: healthy headroom, not the active one, not just-left.
        let pick: Option<(String, f64, f64)> = {
            let mut candidates: Vec<&Row> = rows
                .iter()
                .filter(|r| {
                    r.has_data() && r.name != active_name && r.available() && r.max_pct() <= ceiling
                })
                .filter(|r| {
                    guard
                        .left_at
                        .get(&r.name)
                        .map(|t| t.elapsed().as_secs() >= NO_RETURN_SECS)
                        .unwrap_or(true)
                })
                .collect();
            if candidates.is_empty() {
                None
            } else {
                candidates.sort_by(|a, b| {
                    let ka = a.weekly.resets_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
                    let kb = b.weekly.resets_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
                    ka.cmp(&kb).then(
                        a.headroom()
                            .partial_cmp(&b.headroom())
                            .unwrap_or(std::cmp::Ordering::Equal),
                    )
                });
                let p = candidates[0];
                Some((
                    p.name.clone(),
                    p.session.pct.unwrap_or(0.0),
                    p.weekly.pct.unwrap_or(0.0),
                ))
            }
        };

        match pick {
            None => {
                if !guard.stuck_notified {
                    let soonest = rows
                        .iter()
                        .filter(|r| r.has_data())
                        .filter_map(|r| r.weekly.resets_at.map(humanize_until))
                        .min()
                        .unwrap_or_else(|| "unknown".to_string());
                    notify(&format!(
                        "All accounts high — staying on {} ({act_s:.0}%/{act_w:.0}%), soonest reset in {soonest}",
                        active_label(&state, &active_name),
                    ));
                    guard.stuck_notified = true;
                }
            }
            Some((pick_name, pick_s, pick_w)) => {
                let label = perform_switch(&mut state, &pick_name)?;
                guard
                    .left_at
                    .insert(active_name.clone(), std::time::Instant::now());
                guard.last_swap = Some(std::time::Instant::now());
                guard.stuck_notified = false;
                log_event(&serde_json::json!({
                    "ts": Utc::now().timestamp(),
                    "event": "swap",
                    "from": active_name,
                    "to": pick_name,
                    "session": pick_s,
                    "weekly": pick_w,
                }));
                notify(&format!(
                    "Switched to {label} — {pick_s:.0}% / {pick_w:.0}%"
                ));
                swapped = Some((active_name.clone(), pick_name));
            }
        }
    }

    Ok(CycleOutcome {
        rows,
        active,
        swapped,
        rate_limited: refresh.rate_limited,
    })
}

fn active_label(state: &State, name: &str) -> String {
    state
        .find(name)
        .and_then(|a| a.email.clone())
        .unwrap_or_else(|| name.to_string())
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

fn history_path() -> Result<std::path::PathBuf> {
    Ok(store::config_dir()?.join("history.jsonl"))
}

fn log_event(v: &serde_json::Value) {
    let Ok(path) = history_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
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
            "account": r.name,
            "active": active == Some(r.name.as_str()),
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

    // Consumption = positive change in the active account's session% over time.
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
                let dt = Local.timestamp_opt(s.ts, 0).single();
                if let Some(dt) = dt {
                    by_weekday[dt.weekday().num_days_from_monday() as usize] += delta;
                    by_hour[dt.hour() as usize] += delta;
                }
            }
        }
        prev = Some(s);
    }

    // Per-account peak weekly utilization.
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
    let span_start = Local.timestamp_opt(samples.first().unwrap().ts, 0).single();
    let span_end = Local.timestamp_opt(samples.last().unwrap().ts, 0).single();

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
    for (name, p) in &peak {
        println!("  {:<14} {}", name, bar(Some(*p)));
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

fn cmd_install() -> Result<()> {
    let exe = std::env::current_exe().context("locating this binary")?;
    let dir = store::config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let out_log = dir.join("watch.out.log");
    let err_log = dir.join("watch.err.log");
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
  <key>StandardOutPath</key><string>{out}</string>
  <key>StandardErrorPath</key><string>{err}</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        exe = exe.display(),
        out = out_log.display(),
        err = err_log.display(),
    );
    let path = plist_path()?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, plist).context("writing LaunchAgent plist")?;

    // Reload if already present, then start.
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
    println!("Logs: {}", err_log.display());
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
        "{:<2} {:<12} {:<26} {:<22} {:<11}",
        "", "ACCOUNT", "EMAIL", "SESSION (5h)", "RESETS IN"
    );
    header.push_str(&format!("  {:<22} {:<11}", "WEEKLY (7d)", "RESETS IN"));
    if has_opus {
        header.push_str(&format!("  {:<22} {:<11}", "WEEKLY OPUS", "RESETS IN"));
    }
    header.push_str(&format!("  {:<10}", "UPDATED"));
    println!("{header}");
    println!("{}", "-".repeat(header.len()));

    for r in rows {
        let marker = if active == Some(r.name.as_str()) {
            "▶"
        } else {
            " "
        };
        if !r.has_data() {
            println!(
                "{marker}  {:<12} {:<26} no data yet (run the menu-bar app or `claude-usage watch`)",
                r.name,
                truncate(&r.email, 26),
            );
            continue;
        }
        let mut line = format!(
            "{marker}  {:<12} {:<26} {:<22} {:<11}",
            r.name,
            truncate(&r.email, 26),
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
    let secs = Utc::now().timestamp() - ts;
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

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn email_suffix(acct: &Account) -> String {
    match &acct.email {
        Some(e) => format!(" ({e})"),
        None => String::new(),
    }
}

fn resolve_name(name: Option<&String>) -> Result<String> {
    if let Some(n) = name {
        return Ok(n.clone());
    }
    let state = State::load()?;
    match state.accounts.as_slice() {
        [only] => Ok(only.name.clone()),
        [] => bail!("no accounts; capture one with: claude-usage capture <name>"),
        _ => bail!("multiple accounts; specify one by name"),
    }
}

#[allow(dead_code)]
fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .context("reading input")?;
    Ok(buf.trim_end_matches(['\n', '\r']).to_string())
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
