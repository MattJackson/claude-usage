//! claude-usage — usage/limits across multiple Claude accounts, keyed by the
//! account email, and account switching by writing the shared keychain login
//! plus the `~/.claude.json` identity Claude Code reads. New `claude` sessions
//! use the switched account; already-running sessions keep theirs until
//! restarted.

mod countdown;
#[cfg(target_os = "macos")]
mod icons;
mod logging;
#[cfg(target_os = "macos")]
mod menubar;
mod notifications;
mod providers;
mod store;
mod usage_log;

use providers::claude::{oauth, usage};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};

use providers::Provider;
use store::{Account, CachedUsage, State};

/// Slug of the sole first-class provider in v1 (state.json is still keyed as
/// a flat list of Claude accounts). Any code that needs to resolve "the
/// provider for this account" today uses this; phase 3 (state v2) replaces
/// the constant with a per-account lookup keyed on the containing bucket.
const CLAUDE_SLUG: &str = "claude";

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
/// Proactive flip-back: when the active account is healthy (below trigger), only
/// swap to a better one if that better account's weekly window resets sooner, or
/// — on an equal reset — it has at least this many more points of headroom. The
/// weekly-reset primary key is stable; the headroom tiebreak fluctuates as you
/// work, so this margin keeps two near-equal accounts from ping-ponging.
const PROACTIVE_HEADROOM_MARGIN: f64 = 10.0;
/// Bundle id / label for the launchd agent (runs the menu-bar app at login).
pub(crate) const LAUNCHD_LABEL: &str = "com.claude-usage.menubar";

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // Populate the provider registry once, before any command handler runs.
    // Cheap (a `Vec::push` per feature-gated provider) and idempotent, so
    // handlers that never touch the registry (today: all of them) pay
    // nothing meaningful. Later phases route `menubar::run` through
    // `providers::get`, at which point this call is load-bearing — do it
    // here so it always precedes the dispatch below.
    providers::init();

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
        let existing = state.find(&email);
        let existed = existing.is_some();
        // Re-capturing only refreshes identity/tokens — keep the usage snapshot
        // the scheduler already fetched, so `list`/menu don't blank to "no data".
        acct.cached_usage = merged_cached_usage(existing);
        state.upsert(acct);
        state.active = Some(email.clone());
        state.save()?;
        Ok(existed)
    })?;
    Ok((email, existed))
}

/// The cached usage to keep when (re)capturing an account: the snapshot we
/// already had for this email, if any (capture only refreshes identity/tokens; a
/// freshly captured account has no snapshot of its own). Pure, for tests.
fn merged_cached_usage(existing: Option<&Account>) -> Option<CachedUsage> {
    existing.and_then(|a| a.cached_usage.clone())
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
            match kind {
                Launch::Continue => {
                    cmd.arg("--continue");
                }
                Launch::Fresh => {}
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

/// Resolve a provider by slug, mapping "not registered" to a clear error.
fn provider_by_slug(slug: &str) -> Result<&'static dyn Provider> {
    providers::get(slug)
        .with_context(|| format!("provider '{slug}' is not registered in this build"))
}

/// True iff the provider registered under `slug` is eligible as an auto-swap
/// candidate — i.e. it exposes both a usage signal (so we can compare
/// candidates) and a way to switch to it. An unknown slug returns `false`:
/// the safest thing when a Row's `provider_id` doesn't correspond to any
/// registered provider is to leave it out of the auto-swap decision.
fn provider_supports_swap(slug: &str) -> bool {
    match providers::get(slug) {
        Some(p) => {
            let c = p.capabilities();
            c.supports_usage && c.supports_switching
        }
        None => false,
    }
}

/// Env var whose presence forces the Claude CLI to use a specific OAuth token,
/// bypassing the keychain login. When set, any swap we perform is silently
/// ignored by `claude`, so we skip Claude from auto-swap candidacy AND surface
/// a disabled "env override active — swap disabled" row in the menu section.
pub(crate) const CLAUDE_ENV_OVERRIDE_VAR: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The env var (if any) whose presence overrides `slug`'s stored login.
pub(crate) fn env_override_var_for(slug: &str) -> Option<&'static str> {
    match slug {
        CLAUDE_SLUG => Some(CLAUDE_ENV_OVERRIDE_VAR),
        _ => None,
    }
}

/// Whether the given provider currently has its env-override active in this
/// process's environment. `#[cfg(test)]` builds also consult a thread-local
/// hook (`set_env_override_hook_for_test`) so tests can toggle overrides
/// deterministically without racing on the real environment.
pub(crate) fn env_override_active(slug: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(v) = env_override_hook_lookup(slug) {
            return v;
        }
    }
    env_override_var_for(slug)
        .map(|var| {
            std::env::var_os(var)
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

#[cfg(test)]
thread_local! {
    /// Set of slugs to report as env-overridden. `None` means "consult the
    /// real environment", `Some(set)` means "report only these slugs".
    static ENV_OVERRIDE_TEST_HOOK: std::cell::RefCell<Option<std::collections::HashSet<String>>>
        = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn env_override_hook_lookup(slug: &str) -> Option<bool> {
    ENV_OVERRIDE_TEST_HOOK.with(|h| h.borrow().as_ref().map(|s| s.contains(slug)))
}

/// Run `f` with the env-override lookup answering `true` only for `overrides`.
/// Restores the previous state on exit (including a panicking `f`, via `Drop`).
#[cfg(test)]
pub(crate) fn with_env_override_hook<F, R>(overrides: &[&str], f: F) -> R
where
    F: FnOnce() -> R,
{
    struct Guard(Option<std::collections::HashSet<String>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            let prev = self.0.take();
            ENV_OVERRIDE_TEST_HOOK.with(|h| *h.borrow_mut() = prev);
        }
    }
    let prev = ENV_OVERRIDE_TEST_HOOK.with(|h| h.borrow().clone());
    let set: std::collections::HashSet<String> =
        overrides.iter().map(|s| s.to_string()).collect();
    ENV_OVERRIDE_TEST_HOOK.with(|h| *h.borrow_mut() = Some(set));
    let _g = Guard(prev);
    f()
}

/// Make `email` the active login. Does all network work (token refresh, identity
/// backfill) OUTSIDE the state lock, then commits keychain + ~/.claude.json +
/// state under the lock with a fresh reload. Returns the display label.
pub(crate) fn switch_to(email: &str) -> Result<String> {
    // v1 state is Claude-only; look up the "claude" provider now so later
    // phases can key the switch on the account's containing bucket instead.
    let provider = provider_by_slug(CLAUDE_SLUG)?;
    let (acct, identity, backfilled) = prepare_switch(provider, email)?;
    let label = acct.email.clone().unwrap_or_else(|| email.to_string());
    Ok(switch_to_guarded(provider, email, &acct, &identity, backfilled, None)?.unwrap_or(label))
}

/// Like `switch_to`, but only if `expect_active` is still the active account
/// when the lock is taken (a compare-and-set). Used by the auto-swap daemon so a
/// concurrent manual switch isn't overridden by a stale decision. Returns the
/// label on a real switch, or `None` if it was skipped.
fn switch_to_if_still_active(
    provider: &'static dyn Provider,
    email: &str,
    expect_active: &str,
) -> Result<Option<String>> {
    let (acct, identity, backfilled) = prepare_switch(provider, email)?;
    switch_to_guarded(provider, email, &acct, &identity, backfilled, Some(expect_active))
}

/// Phase 1 of a switch (no lock): refresh the token and resolve the identity
/// over the network, returning what the locked commit phase needs.
fn prepare_switch(
    provider: &'static dyn Provider,
    email: &str,
) -> Result<(Account, serde_json::Value, bool)> {
    let state = State::load()?;
    let mut acct = state
        .find(email)
        .cloned()
        .with_context(|| format!("no account matches '{email}'"))?;
    oauth::ensure_fresh(&mut acct, REFRESH_SKEW_SECS)?;
    let (identity, backfilled) = resolve_identity(provider, &acct)?;
    Ok((acct, identity, backfilled))
}

/// The locked phase of a switch, optionally guarded by `expect_active`: if given
/// and the reloaded active account no longer matches it, the switch is skipped
/// (returns `Ok(None)`). The auto-swap daemon uses this so a manual switch that
/// lands between its decision and this commit isn't silently overridden by a now
/// stale choice. `acct`/`identity`/`backfilled` come from the caller's unlocked
/// phase-1 network work. Returns the display label on a real switch.
fn switch_to_guarded(
    provider: &'static dyn Provider,
    email: &str,
    acct: &Account,
    identity: &serde_json::Value,
    backfilled: bool,
    expect_active: Option<&str>,
) -> Result<Option<String>> {
    let label = acct.email.clone().unwrap_or_else(|| email.to_string());
    let switched = with_state_lock(|| {
        let mut st = State::load()?;
        if let Some(exp) = expect_active {
            if st.active.as_deref() != Some(exp) {
                logging::log(&format!(
                    "swap to {label} skipped: active changed to {:?} since the decision",
                    st.active
                ));
                return Ok(false);
            }
        }
        // Preserve any token rotation the currently-active session picked up.
        sync_active_from_keychain(provider, &mut st);
        // A concurrent poll may have rotated this account's token after our
        // phase-1 snapshot; use whichever tokens are fresher so we never write a
        // stale (possibly already-superseded) refresh token to the keychain.
        let mut acct = acct.clone();
        if let Some(cur) = st.find(email) {
            if cur.expires_at > acct.expires_at {
                acct.set_tokens(
                    cur.access_token.clone(),
                    cur.refresh_token.clone(),
                    cur.expires_at,
                );
            }
        }
        // ~/.claude.json first, keychain last (the commit point), rollback on fail.
        apply_account(provider, &acct, identity)?;
        if let Some(a) = st.find_mut(email) {
            a.set_tokens_if_newer(
                acct.access_token.clone(),
                acct.refresh_token.clone(),
                acct.expires_at,
            );
            if backfilled {
                a.oauth_account = Some(identity.clone());
            }
        }
        st.active = Some(email.to_string());
        // The login is already committed to the keychain + ~/.claude.json at this
        // point; if only the state.json bookkeeping write fails, say so clearly.
        if let Err(e) = st.save() {
            return Err(e).context("the login was switched but recording it in state.json failed");
        }
        Ok(true)
    })?;
    if switched {
        logging::log(&format!("switch -> {label}"));
        Ok(Some(label))
    } else {
        Ok(None)
    }
}

/// Resolve the identity to write for this account. Returns (identity,
/// backfilled_from_network). Errors if it can't be resolved (e.g. offline and
/// never captured) — the caller then does NOT switch, rather than half-applying
/// one. `provider` is threaded through so later phases can dispatch off it; the
/// v1 body still uses the Claude-specific profile endpoint (behavior-preserving).
fn resolve_identity(
    provider: &'static dyn Provider,
    acct: &Account,
) -> Result<(serde_json::Value, bool)> {
    let _ = provider;
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
/// `provider` is threaded through for the same forward-compat reason as
/// `resolve_identity`; the v1 body is the Claude-specific keychain path.
fn apply_account(
    provider: &'static dyn Provider,
    acct: &Account,
    identity: &serde_json::Value,
) -> Result<()> {
    let _ = provider;
    let prior = read_claude_json_raw();
    write_claude_identity(identity, acct.user_id.as_deref())?;
    if let Err(e) = keychain_write(&acct.keychain_blob) {
        if let Some((bytes, mode)) = &prior {
            // If the rollback ALSO fails we're half-applied (~/.claude.json points
            // at the new account, keychain still holds the old) — surface that
            // explicitly rather than silently swallowing the rollback error.
            if let Err(re) = restore_claude_json_raw(bytes, *mode) {
                return Err(e).context(format!(
                    "writing the account into the keychain, and rolling back \
                     ~/.claude.json failed too ({re:#}); it may now point at the new \
                     account while the keychain holds the old — run `claude-usage \
                     switch` again to reconcile"
                ));
            }
        }
        return Err(e).context("writing the account into the keychain");
    }
    Ok(())
}

/// Whether the keychain / `~/.claude.json` identity is our tracked account:
/// match on `accountUuid` when both sides have one, otherwise case-insensitive
/// email. An account with no known identity yet adopts (self-heal); a known
/// email with nothing to compare against does NOT (stay safe). Pure, for tests.
fn identity_matches(
    acct_uuid: Option<&str>,
    acct_email: Option<&str>,
    json_uuid: Option<&str>,
    json_email: Option<&str>,
) -> bool {
    match (acct_uuid, json_uuid) {
        (Some(a), Some(b)) => a == b,
        _ => match (acct_email, json_email) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            (None, _) => true,
            _ => false,
        },
    }
}

/// If the account currently in the keychain is genuinely our active account,
/// adopt any token rotation a live `claude` session performed. Verified by
/// identity: `/login` into a *different* account rewrites ~/.claude.json's
/// oauthAccount, so a mismatch means the keychain is not our active account and
/// we must NOT overwrite its stored tokens. In-place rotation keeps the same
/// account uuid/email, so legitimate rotation is still captured.
fn sync_active_from_keychain(provider: &'static dyn Provider, state: &mut State) {
    let _ = provider;
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
    let matches = identity_matches(
        acct.identity_uuid().as_deref(),
        acct.email.as_deref(),
        json_uuid.as_deref(),
        json_email.as_deref(),
    );
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
    // Phase 1 (no lock): refresh if near expiry. Network I/O stays OUTSIDE the
    // cross-process lock so a slow/hung token endpoint can't stall the daemon
    // poll or a concurrent switch (which all take the same lock).
    let mut acct = State::load()?
        .find(&email)
        .cloned()
        .with_context(|| format!("no account matches '{email}'"))?;
    let refreshed = oauth::ensure_fresh(&mut acct, REFRESH_SKEW_SECS)?;
    let token = acct.access_token.clone();
    // Phase 2 (locked): persist a rotation without clobbering a fresher one.
    if refreshed {
        with_state_lock(|| {
            let mut st = State::load()?;
            if let Some(a) = st.find_mut(&email) {
                a.set_tokens_if_newer(
                    acct.access_token.clone(),
                    acct.refresh_token.clone(),
                    acct.expires_at,
                );
            }
            // The token was already rotated server-side (single-use); if we can't
            // persist it, say so — the stored refresh token is now stale.
            st.save().context(
                "the token was refreshed but recording the rotation in state.json \
                 failed; if refreshes start failing, run `claude-usage capture` for \
                 this account",
            )
        })?;
    }
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
    /// Slug of the provider that owns this row's account (matches a registered
    /// `Provider::provider_id`). Threaded through swap decisions so we can
    /// consult `providers::get(...).capabilities()` without re-reading state.
    /// In v1 every row is `"claude"`; a Row whose slug isn't registered is
    /// simply skipped as an auto-swap candidate.
    provider_id: String,
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
        // v1 state stores only Claude accounts; phase 3 (state v2) tags each
        // account with its containing provider slug and this becomes a real
        // per-account lookup.
        provider_id: CLAUDE_SLUG.to_string(),
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

/// Order accounts for the menu the way auto-pick prioritizes them: the account
/// you'd switch to first on top, then the rest by the same rule, with unusable
/// ones (maxed out, or no data yet) sinking to the bottom. Display-only — the
/// CLI keeps insertion order.
pub(crate) fn menu_order(a: &Row, b: &Row) -> std::cmp::Ordering {
    // Usable (has data + room) before unusable; then accounts with data before
    // those without; then the normal auto-pick priority within each group.
    let usable = |r: &Row| r.has_data() && r.available();
    usable(b)
        .cmp(&usable(a))
        .then_with(|| b.has_data().cmp(&a.has_data()))
        .then_with(|| candidate_order(a, b))
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

    /// Eligible as a swap / return target: the **session** has room to spare (so
    /// landing here won't immediately re-trigger a swap) and the **weekly** is
    /// still below the trigger. Weekly is deliberately allowed to run right up to
    /// the trigger — returning to drain each account's weekly before it resets is
    /// the whole point, so a high weekly must not disqualify a fresh-session one.
    fn eligible_target(&self, session_ceiling: f64, weekly_trigger: f64) -> bool {
        self.session.pct.unwrap_or(0.0) <= session_ceiling
            && self.weekly.pct.unwrap_or(0.0) < weekly_trigger
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
    // Fully-qualified to fs2's trait: std 1.89 added an inherent `File::unlock`
    // that would otherwise shadow it and break the 1.88 MSRV (fs2 has no such
    // requirement). Keep this qualified so the call can't drift onto std's.
    let _ = fs2::FileExt::unlock(&file);
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

/// Raw bytes of ~/.claude.json plus its permission mode, for rollback.
fn read_claude_json_raw() -> Option<(Vec<u8>, u32)> {
    let path = claude_json_path().ok()?;
    let bytes = std::fs::read(&path).ok()?;
    let mode = claude_json_mode(&path);
    Some((bytes, mode))
}

/// The file's permission bits (0o600 fallback so a rollback never loosens perms).
#[cfg(unix)]
fn claude_json_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o600)
}

#[cfg(not(unix))]
fn claude_json_mode(_path: &std::path::Path) -> u32 {
    0o600
}

/// Restore ~/.claude.json to prior raw bytes (atomic tmp+rename), re-applying the
/// original mode so the rollback preserves the file's permissions (matching the
/// documented "rewrites preserve the original file mode" invariant). Cleans up
/// the temp file on any failure.
fn restore_claude_json_raw(bytes: &[u8], mode: u32) -> Result<()> {
    let path = claude_json_path()?;
    write_bytes_atomic_mode(&path, bytes, mode)
}

/// Atomically write `bytes` to `path` (tmp + rename) ending at permission
/// `mode`, cleaning up the temp file on any failure. The temp is created
/// owner-only from the start (via `store::write_private`, no umask window) since
/// these files carry OAuth tokens; `mode` is then applied before the rename.
/// Shared by the `~/.claude.json` identity write and rollback, and unit-tested.
fn write_bytes_atomic_mode(path: &std::path::Path, bytes: &[u8], mode: u32) -> Result<()> {
    let tmp = path.with_extension("json.claude-usage.tmp");
    if let Err(e) = store::write_private(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("writing temp file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = mode;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("renaming file into place");
    }
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
    // Preserve the file's existing mode; the shared writer creates the temp
    // owner-only first, so tokens are never briefly world-readable.
    let mode = claude_json_mode(&path);
    write_bytes_atomic_mode(&path, &json, mode).context("replacing ~/.claude.json")
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
    // v1 state is Claude-only; resolve the provider once at the top of the
    // cycle. Phase 3 (state v2) makes this a per-account bucket lookup — the
    // per-account body below already runs inside a loop, so the transition is
    // additive rather than restructural.
    let provider = match provider_by_slug(CLAUDE_SLUG) {
        Ok(p) => p,
        Err(e) => {
            logging::log(&format!("poll: provider unavailable: {e:#}"));
            return RefreshOutcome {
                rate_limited: false,
            };
        }
    };
    sync_active_from_keychain(provider, &mut state);
    logging::log("poll: refreshing usage cache");

    let mut rate_limited = false;
    // (email, refreshed account after ensure_fresh, new cached usage or None,
    // updated notif state or None)
    let mut updates: Vec<(String, Account, Option<CachedUsage>, Option<notifications::NotifState>)> =
        Vec::new();
    let emails: Vec<String> = state.accounts.iter().map(|a| a.key().to_string()).collect();
    let notif_cfg = notifications::NotificationConfig::default();
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
        // Persist a per-tick snapshot into the long-form history log, then run
        // notifications against the (prev, curr) pair. Best-effort throughout:
        // a full disk / permission failure never breaks the poll cycle.
        let mut new_notif_state: Option<notifications::NotifState> = None;
        if let Some(cu) = &cu {
            let account_key = usage_log::AccountKey::new(CLAUDE_SLUG, email.clone());
            let prev_snap = usage_log::last_snapshot(&account_key);
            let curr_snap = usage_log::Snapshot {
                ts: Utc::now(),
                provider: CLAUDE_SLUG.to_string(),
                account: email.clone(),
                session_pct: cu.session_pct.map(|p| p as f32),
                weekly_pct: cu.weekly_pct.map(|p| p as f32),
                active_model: None,
            };
            if let Err(e) = usage_log::append(&curr_snap) {
                logging::log(&format!("usage_log: append failed for {email}: {e:#}"));
            }
            if let Some(prev) = prev_snap {
                let mut ns = acct.notif_state.clone();
                let raw = notifications::evaluate(&prev, &curr_snap, &notif_cfg);
                let mut kept =
                    notifications::dedup_and_apply(&mut ns, &prev, &curr_snap, raw);
                // Pace check (default off) — feeds through the same dedup path.
                let pace = usage_log::pace(&account_key);
                if let Some(pt) = notifications::evaluate_pace(
                    pace.as_ref(),
                    &curr_snap,
                    &notif_cfg,
                    Utc::now(),
                ) {
                    let extra = notifications::dedup_and_apply(
                        &mut ns,
                        &prev,
                        &curr_snap,
                        vec![pt],
                    );
                    kept.extend(extra);
                }
                for trig in &kept {
                    if let Err(e) = notifications::fire(trig, &account_key) {
                        logging::log(&format!("notify: {email}: {e:#}"));
                    }
                }
                if ns != acct.notif_state {
                    new_notif_state = Some(ns);
                }
            }
        }
        updates.push((email.clone(), acct, cu, new_notif_state));
    }

    // Merge under the lock with a fresh reload so we don't clobber a switch.
    let merged = with_state_lock(|| {
        let mut st = State::load()?;
        for (email, acct, cu, ns) in &updates {
            if let Some(a) = st.find_mut(email) {
                // Recency-guarded (matching switch_to): a concurrent switch that
                // adopted a keychain rotation while we were fetching must not be
                // clobbered by our older phase-1 snapshot.
                a.set_tokens_if_newer(
                    acct.access_token.clone(),
                    acct.refresh_token.clone(),
                    acct.expires_at,
                );
                if let Some(cu) = cu {
                    a.cached_usage = Some(cu.clone());
                }
                if let Some(ns) = ns {
                    a.notif_state = ns.clone();
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
                if outcome.rate_limited {
                    logging::log(&format!("rate limited; backing off to {current}s"));
                }
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
        (current.max(base) * 2).min(WATCH_MAX_INTERVAL_SECS)
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

/// Drop no-return entries past their window so `left_at` can't grow without
/// bound over a daemon running for weeks (e.g. accounts later `rm`'d).
fn prune_swap_guard(guard: &mut SwapGuard) {
    guard
        .left_at
        .retain(|_, t| t.elapsed().as_secs() < NO_RETURN_SECS);
}

/// Result of one poll: the swap it made (if any) and the rate-limited flag.
pub(crate) struct CycleOutcome {
    pub swapped: Option<(String, String)>,
    pub rate_limited: bool,
}

/// True if `cand` is a strictly better place to be than the healthy `act`:
/// a sooner weekly reset (use-it-or-lose-it), or — on an equal reset — a
/// meaningful headroom lead. Used only on the proactive (active-not-in-trouble)
/// path; the reactive path moves regardless of how the active account ranks.
fn worth_returning_to(cand: &Row, act: &Row) -> bool {
    let kc = cand.weekly.resets_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
    let ka = act.weekly.resets_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
    match kc.cmp(&ka) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => cand.headroom() - act.headroom() >= PROACTIVE_HEADROOM_MARGIN,
    }
}

/// A pure auto-swap decision over cached rows. Returns the email to swap to, or
/// None (with `guard` consulted for cooldown / no-return). Extracted for tests.
///
/// Two paths: **reactive** — the active account has reached `trigger`, so move to
/// the best healthy candidate; and **proactive** — the active account is still
/// healthy, but a better account has since freed up (e.g. its 5h session reset),
/// so flip back to it. The proactive path additionally requires the candidate to
/// be `worth_returning_to` the active account, so we don't swap sideways.
fn choose_swap_target(
    rows: &[Row],
    active: &str,
    trigger: f64,
    ceiling: f64,
    guard: &SwapGuard,
) -> Option<String> {
    let act = rows.iter().find(|r| r.email == active)?;
    if !act.has_data() {
        return None;
    }
    // If the active account's provider has its env-override active, the CLI
    // ignores whatever token we install into the keychain — swapping is a
    // no-op. Bail before we churn the state file.
    if env_override_active(&act.provider_id) {
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
        // A row is only a valid swap candidate if its owning provider has both
        // a usage signal (so we can compare its cached utilization to the
        // ceiling) and a way to actually switch to it. This is the sole
        // capability gate for auto-swap: reporting-only or stub providers
        // never surface here, even if their rows carry a stale utilization.
        .filter(|r| provider_supports_swap(&r.provider_id))
        // Skip candidates whose provider has its env-override active — a
        // switch to them would be silently ignored by the vendor CLI.
        .filter(|r| !env_override_active(&r.provider_id))
        .filter(|r| r.has_data() && r.email != active && r.eligible_target(ceiling, trigger))
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
    let best = candidates[0];
    // Active in trouble → move to the best candidate. Active still healthy →
    // only move if the best candidate is genuinely a better place to be.
    if act.max_pct() >= trigger || worth_returning_to(best, act) {
        Some(best.email.clone())
    } else {
        None
    }
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
                let target_row = rows
                    .iter()
                    .find(|r| r.email == target)
                    .expect("choose_swap_target returned an email absent from `rows`");
                let (pick_s, pick_w) = (
                    target_row.session.pct.unwrap_or(0.0),
                    target_row.weekly.pct.unwrap_or(0.0),
                );
                // Proactive flip-back if the account we're leaving wasn't itself
                // over the trigger — a better account simply freed up.
                let proactive = rows
                    .iter()
                    .find(|r| r.email == active_email)
                    .map(|r| r.max_pct() < trigger)
                    .unwrap_or(false);
                // Resolve the target row's provider. `choose_swap_target`
                // already filtered on `provider_supports_swap`, so this must
                // succeed for any row it returned; a mismatch is a bug.
                let target_provider = match provider_by_slug(&target_row.provider_id) {
                    Ok(p) => p,
                    Err(e) => {
                        logging::log(&format!(
                            "swap to {target} skipped: provider unavailable: {e:#}"
                        ));
                        return Ok(CycleOutcome {
                            swapped: None,
                            rate_limited: refresh.rate_limited,
                        });
                    }
                };
                // Compare-and-set on the active account: if a manual switch landed
                // since choose_swap_target read the snapshot, skip this swap.
                let Some(label) =
                    switch_to_if_still_active(target_provider, &target, &active_email)?
                else {
                    return Ok(CycleOutcome {
                        swapped: None,
                        rate_limited: refresh.rate_limited,
                    });
                };
                guard
                    .left_at
                    .insert(active_email.clone(), std::time::Instant::now());
                guard.last_swap = Some(std::time::Instant::now());
                guard.stuck_notified = false;
                prune_swap_guard(guard);
                log_event(&serde_json::json!({
                    "ts": Utc::now().timestamp(),
                    "event": "swap",
                    "reason": if proactive { "proactive" } else { "trigger" },
                    "from": active_email,
                    "to": target,
                    "session": pick_s,
                    "weekly": pick_w,
                }));
                let verb = if proactive {
                    "Flipped back to"
                } else {
                    "Switched to"
                };
                notify(&format!("{verb} {label} — {pick_s:.0}% / {pick_w:.0}%"));
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
                    // Pick the soonest reset by TIMESTAMP, then humanize — taking
                    // min() of humanized strings sorts lexicographically ("3d 12h"
                    // < "3d 9h"), which is not chronological.
                    let soonest = rows
                        .iter()
                        .filter(|r| r.has_data())
                        .filter_map(|r| r.weekly.resets_at)
                        .min()
                        .map(humanize_until)
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

fn history_path() -> Result<std::path::PathBuf> {
    Ok(store::config_dir()?.join("history.jsonl"))
}

fn log_event(v: &serde_json::Value) {
    use std::io::Write;
    let Ok(path) = history_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Rotate if it has grown too large (keep one previous generation), using the
    // same threshold as the debug log so the policy can't drift.
    logging::rotate_if_large(&path, logging::MAX_BYTES);
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

/// Positive session-% deltas between consecutive SAME-account active samples —
/// the quantity `cmd_report` buckets by weekday/hour. A delta across an account
/// switch would subtract two unrelated accounts' percentages, so those are
/// skipped; only increases count. Returns `(timestamp, delta)`. Pure, for tests.
fn consumption_deltas(active: &[&Sample]) -> Vec<(i64, f64)> {
    let mut out = Vec::new();
    let mut prev: Option<&Sample> = None;
    for s in active {
        if let Some(p) = prev {
            if p.account == s.account {
                if let (Some(pv), Some(cur)) = (p.session, s.session) {
                    let delta = cur - pv;
                    if delta > 0.0 {
                        out.push((s.ts, delta));
                    }
                }
            }
        }
        prev = Some(s);
    }
    out
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
    for (ts, delta) in consumption_deltas(&active) {
        if let Some(dt) = Local.timestamp_opt(ts, 0).single() {
            by_weekday[dt.weekday().num_days_from_monday() as usize] += delta;
            by_hour[dt.hour() as usize] += delta;
        }
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
