//! usagio — usage/limits across multiple Claude / Codex accounts (and every
//! AI-coding CLI we grow support for), keyed by the account email, and
//! account switching by writing the shared keychain login plus the
//! `~/.claude.json` identity Claude Code reads. New `claude` sessions use the
//! switched account; already-running sessions keep theirs until restarted.
//!
//! Renamed from `claude-usage` in v0.4.0. Config dir, launchd label, and
//! Login Items entry migrate transparently on first run; the macOS Keychain
//! service string is intentionally frozen at "claude-usage" to preserve
//! existing tokens (see `providers/claude/mod.rs`).

mod burn_rate;
mod context_ledger;
mod cost_tracking;
mod countdown;
mod credentials;
#[cfg(test)]
mod env_lock;
mod icons;
mod logging;
mod menubar;
mod notifications;
mod paths;
mod platform;
mod pricing;
mod providers;
mod store;
// Custom tray-anchored popup UI (Phase 1). Whole module compiled only on macOS
// under the off-by-default `custom-popup` feature; this single gated `mod ui;`
// is the ONLY target_os cfg it needs (the module body has none), per
// docs/design/custom-tray-popup.md and the strict_cfg allowlist note.
#[cfg(all(target_os = "macos", feature = "custom-popup"))]
mod ui;
mod usage_log;
mod watchdog;

use providers::claude::{oauth, usage};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, Utc};

use providers::trait_def::{TokenGrant, UsageSnapshot};
use providers::Provider;
use store::{Account, CachedUsage, ProviderAccount, State};

/// Slug of the sole first-class provider in v1 (state.json is still keyed as
/// a flat list of Claude accounts). Any code that needs to resolve "the
/// provider for this account" today uses this; phase 3 (state v2) replaces
/// the constant with a per-account lookup keyed on the containing bucket.
const CLAUDE_SLUG: &str = "claude";

const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
/// App slug used everywhere we ask the platform for a per-app directory
/// (`~/.config/usagio` on macOS, `$XDG_CONFIG_HOME/usagio` on Linux,
/// `%APPDATA%\usagio` on Windows). The one-shot rename from `claude-usage`
/// → `usagio` migrates the on-disk directory transparently at first run
/// (see `paths::migrate_config_dir_if_needed`).
pub(crate) const APP_SLUG: &str = "usagio";
/// Previous slug (pre-v0.4.0). Referenced only by the one-shot migration
/// in `paths::migrate_config_dir_if_needed` — do not use for any live
/// path construction.
pub(crate) const LEGACY_APP_SLUG: &str = "claude-usage";
/// Refresh a token if it expires within this many seconds. Sourced from the
/// credential-sync module so the reactive (switch / cmd_token / poll) path
/// and the proactive (fsnotify + inactive-refresh) path can never drift.
const REFRESH_SKEW_SECS: i64 = credentials::REFRESH_SKEW_SECS;

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
/// Reverse-DNS label for the OS's autostart registration (runs the menu-bar
/// app at login). Reused across install / uninstall on every platform: it's
/// the launchd Label on macOS, the .desktop filename on Linux, and the
/// registry value name on Windows.
pub(crate) const AUTOSTART_LABEL: &str = "com.mattjackson.usagio.menubar";
/// Previous launchd label (pre-v0.4.0). Referenced only by the one-shot
/// migration in `cmd_install` — unload + remove the old plist before
/// registering the new label so upgraded users don't run two agents.
pub(crate) const LEGACY_AUTOSTART_LABEL: &str = "com.claude-usage.menubar";

/// Process-global handle to the platform impl. `platform::current()` builds
/// once at first use; every host-OS call (keychain, autostart, paths) routes
/// through this so we never sprinkle `#[cfg(target_os = ...)]` across
/// business logic.
static PLATFORM: std::sync::OnceLock<Box<dyn platform::Platform>> = std::sync::OnceLock::new();

/// Return the process-global platform impl, initializing it on first call.
pub(crate) fn platform() -> &'static dyn platform::Platform {
    PLATFORM.get_or_init(platform::current).as_ref()
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// True if `exe` looks like it was launched from inside a macOS `.app`
/// bundle (a Finder double-click), as opposed to a bare CLI invocation
/// (`/opt/homebrew/bin/usagio`, or a shell alias). Checked by path shape
/// only — no `cfg(target_os)` needed, since `.app/Contents/MacOS/` simply
/// never appears in a non-bundle exe path on any OS. Mirrors the same
/// pattern `launch_agent_exe_path` already checks for the install path.
fn is_app_bundle_launch(exe: &std::path::Path) -> bool {
    exe.to_string_lossy().contains(".app/Contents/MacOS/")
}

/// Resolve the effective "first CLI argument" `run()`'s dispatch match
/// switches on: the real first arg if one was given, otherwise `"menubar"`
/// when we were launched from a `.app` bundle (so double-clicking
/// usagio.app in Finder starts the menu bar, not a one-shot `list` that
/// prints to a terminal nobody's watching), otherwise `None` (the bare
/// binary's existing default: `cmd_list`).
///
/// Pure function of `args`/`exe` so a test can inject both without touching
/// real argv or a real bundle.
fn effective_first_arg<'a>(args: &'a [String], exe: &std::path::Path) -> Option<&'a str> {
    args.first()
        .map(String::as_str)
        .or_else(|| is_app_bundle_launch(exe).then_some("menubar"))
}

fn run() -> Result<()> {
    // One-shot rename migration: if `~/.config/claude-usage/` still exists
    // and `~/.config/usagio/` doesn't, atomically move it (with a
    // cross-device copy+delete fallback). Idempotent and safe to run on
    // every subsequent boot. Must run BEFORE the first state-load or
    // logging init so the new dir is populated before anyone reads it.
    match paths::migrate_config_dir_if_needed() {
        Ok(paths::MigrationResult::Migrated { from, to }) => {
            eprintln!(
                "usagio: migrated config directory {} -> {}",
                from.display(),
                to.display()
            );
        }
        Ok(paths::MigrationResult::BothExisted { new, old }) => {
            eprintln!(
                "usagio: both {} and {} exist; using {} and leaving the old one \
                 in place for inspection",
                old.display(),
                new.display(),
                new.display()
            );
        }
        Ok(_) => {}
        Err(paths::MigrationError::OldRemovalFailed { new, source }) => {
            // The NEW tree is populated and mode-correct — proceed as if the
            // migration succeeded. Log the leftover so the user can inspect.
            // See H6 in the round-1 codeaudit findings and M1 for the variant.
            eprintln!(
                "usagio: migrated config to {}; old tree left in place ({source})",
                new.display()
            );
        }
        Err(e) => {
            // Any other migration failure means the new tree is NOT populated.
            // Falling straight through to State::default (an ENOENT from
            // State::load) would look like a fresh install and prompt every
            // user to re-login. Fire a menu-bar notification AND log loudly.
            // We still proceed (State::default), but the user is at least
            // told and can retry.
            let msg = format!(
                "usagio: config migration FAILED — accounts may appear missing. \
                 See ~/.config/usagio/logs; try again or copy the old dir manually. \
                 Details: {e:?}"
            );
            eprintln!("{msg}");
            logging::log(&msg);
            notify(&msg);
        }
    }

    // Raise RLIMIT_NOFILE from macOS's stock 256 to something a long-lived
    // menubar can actually survive. Under 256, an fsnotify watcher whose
    // parent lands on a big directory (see spawn_watchers) plus normal state
    // reads exhaust FDs within a day, and every subsequent open — state.json,
    // DNS resolution, keychain lookups — starts failing with EMFILE. Best
    // effort; ignored on non-Unix or if the shell's hard cap is lower.
    #[cfg(unix)]
    raise_nofile_limit();

    // Populate the provider registry once, before any command handler runs.
    // Cheap (a `Vec::push` per feature-gated provider) and idempotent. This is
    // load-bearing: `menubar::run` and the switch/refresh/capture handlers
    // resolve providers via `providers::get`/`providers::all`, so `init()` must
    // precede the dispatch below (and the `providers::all()` call right after).
    providers::init();

    let provider_slugs: Vec<&'static str> =
        providers::all().iter().map(|p| p.provider_id()).collect();
    logging::log(&format!(
        "event=startup version={} pid={} config_dir={} providers=[{}]",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        store::config_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unresolved>".to_string()),
        provider_slugs.join(","),
    ));

    // Spawn fsnotify watchers on every provider's credential paths so
    // rotations the vendor CLI writes (or another usagio process makes)
    // are absorbed as they happen, not just on the next 150s watch tick.
    // Only long-running command paths (watch / menubar) actually benefit;
    // one-shot commands complete before any event fires, but the setup is
    // cheap and self-contained.
    // Leaked deliberately: the watcher owns a thread and must outlive the
    // process. `Box::leak` on a heap allocation is the standard trick for
    // "live for program lifetime" without introducing a global mutex.
    // Stored in a process-global registry (not `Box::leak`'d) so the fd-watchdog
    // can drop + rebuild the watcher at runtime to release leaked descriptors —
    // the runtime safety net for the fd-leak class of outage (see
    // `src/watchdog.rs`). The watcher still lives for the process lifetime; the
    // registry just gives the watchdog a handle on it.
    let providers_static: Vec<&'static dyn Provider> =
        providers::all().iter().map(|b| &**b).collect();
    credentials::install_watchers(providers_static);

    // Register our bundle id with the OS notification system before any
    // notification can fire, so macOS doesn't pop the "Where is use_default?"
    // dialog on the first notify (mac-notification-sys otherwise falls back to
    // the "use_default" literal). One-shot, best-effort; no-op off macOS.
    platform::current().register_notification_app();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = std::env::current_exe().unwrap_or_default();
    match effective_first_arg(&args, &exe) {
        None => cmd_list(&[]),
        Some("list") | Some("ls") => cmd_list(&args[1..]),
        Some("capture") | Some("add") => cmd_capture(&args[1..]),
        Some("switch") | Some("use") => cmd_switch(args.get(1).map(String::as_str), None),
        Some("start") => cmd_switch(args.get(1).map(String::as_str), Some(Launch::Fresh)),
        Some("continue") | Some("cont") | Some("c") => {
            cmd_switch(args.get(1).map(String::as_str), Some(Launch::Continue))
        }
        Some("token") => cmd_token(args.get(1).map(String::as_str)),
        Some("watch") => cmd_watch(&args[1..]),
        Some("menubar") => {
            menubar::set_theme_override_from_args(&args[1..]);
            menubar::run()
        }
        Some("report") => cmd_report(&args[1..]),
        Some("context") => cmd_context(&args[1..]),
        Some("install") => cmd_install(),
        Some("uninstall") => cmd_uninstall(),
        Some("rm") | Some("remove") => cmd_rm(args.get(1).map(String::as_str)),
        // Internal, undocumented — see `cmd_secrets_selftest` doc comment.
        Some("__secrets_selftest") => cmd_secrets_selftest(&args[1..]),
        Some("-h") | Some("--help") | Some("help") => {
            print_help();
            Ok(())
        }
        Some("-V") | Some("--version") | Some("version") => {
            println!("usagio {}", env!("CARGO_PKG_VERSION"));
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
        "usagio — usage & instant account switching for Claude\n\n\
         Accounts are identified by their email; commands accept a full email or a\n\
         unique prefix (e.g. `dev` for dev@example.com).\n\n\
         USAGE:\n  \
         usagio                   Show cached usage for every account (default)\n  \
         usagio list --refresh    Fetch usage now, then show it\n  \
         usagio capture [prov]    Save the account you're logged into (default claude; e.g. `capture codex`)\n  \
         usagio switch [email]    Make <email> the active login (no launch)\n  \
         usagio start [email]     Switch, then launch a fresh `claude`\n  \
         usagio continue [email]  Switch, then launch `claude --continue`\n  \
         usagio token [email]     Print a fresh access token\n  \
         usagio watch             Auto-swap at 95%, keep working (foreground)\n  \
         usagio menubar           Run the macOS menu-bar app (usage + auto-swap)\n  \
         usagio menubar --theme <os>  Force an OEM look for a UI audit: windows|macos|gnome|system\n  \
         usagio install           Run the menu-bar app at every login (via launchd)\n  \
         usagio uninstall         Stop running the menu-bar app at login\n  \
         usagio report            Usage patterns by weekday / hour / account\n  \
         usagio report --pace     Per-account burn-rate forecast (empty-in ETA)\n  \
         usagio report --pricing  Model → USD-per-1M-tokens lookup table\n  \
         usagio report --verdict  Cancel/downgrade/keep/upgrade classifier\n  \
         usagio context [OPTS]    Audit CLI auto-injected context (per turn)\n  \
                                        --provider <slug>  claude|codex|opencode\n  \
                                        --project  <path>  scope in-tree instructions to this project\n  \
         usagio rm <email>        Forget an account\n\n\
         With no [email], switch/start/continue auto-pick the account that has room\n  \
         and whose weekly limit resets soonest (use it before the quota resets).\n\n\
         Onboarding: log into an account with `claude` as usual, then\n  \
         `usagio capture`. Repeat once per account.\n"
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

fn cmd_capture(args: &[String]) -> Result<()> {
    // Mirror the menu's `menubar::handle_capture` dispatch exactly — the CLI is
    // the headless twin of the "Capture ▸ <provider>" menu items, sharing the
    // same `capture_current` / `capture_current_generic` code. Claude uses the
    // dedicated `state.accounts` bucket; every other registered provider uses
    // the generic `state.providers[slug]` slot. No arg defaults to Claude
    // (back-compat with the original Claude-only `usagio capture`).
    let slug = args.first().map(String::as_str).unwrap_or(CLAUDE_SLUG);
    if slug == CLAUDE_SLUG {
        let (email, existed) = capture_current()?;
        println!(
            "{} {email} — it's the active login.",
            if existed { "Refreshed" } else { "Captured" }
        );
        return Ok(());
    }
    // Validate the provider up front for a clean error (and its display name)
    // before touching the keychain — same guard the menu path applies.
    let provider = provider_by_slug(slug)?;
    let (key, existed) = capture_current_generic(slug)?;
    println!(
        "{} {} {key} — it's the active {} login.",
        if existed { "Refreshed" } else { "Captured" },
        provider.display_name(),
        slug
    );
    Ok(())
}

/// Capture the account currently in the keychain, keyed by its email. Returns
/// (email, existed_already). Shared by the CLI and the menu bar.
///
/// Delegates identity resolution (email lookup + `~/.claude.json` snapshot)
/// to `providers::claude`'s `Provider::capture_current_login` — the same
/// logic every other provider's capture path (`capture_current_generic`)
/// already dispatches through — instead of re-deriving it inline (v0.5.2
/// simplification-02). Persistence still targets `state.accounts` (the
/// dedicated Claude bucket, epoch-MILLIS `expires_at`) rather than
/// `state.providers["claude"]` (the generic v2 slot, epoch-SECONDS
/// `expires_at`): those are different on-disk shapes, and Claude's rows are
/// read from `state.accounts` everywhere else (`build_snapshot`, `switch_to`,
/// `remove_account`, …), so routing storage through
/// `capture_current_generic` as well would silently split a Claude account
/// across two incompatible buckets.
pub(crate) fn capture_current() -> Result<(String, bool)> {
    let provider = provider_by_slug(CLAUDE_SLUG)?;
    let captured = provider
        .capture_current_login()
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| {
            anyhow!("no claude.ai login found in the keychain — run `claude` and /login first")
        })?;
    // `Account::from_keychain_blob` parses `expires_at` (epoch millis) out of
    // the verbatim blob exactly as before — reusing it here keeps this an
    // identity-resolution dedup, not a token-parsing rewrite.
    let mut acct = Account::from_keychain_blob(&captured.secret_blob)?;
    let email = captured.identity.email.clone().context(
        "could not determine this account's email (offline?) — connect and try `capture` again",
    )?;
    acct.email = Some(email.clone());
    // `capture_current_login` packs both fields into one native_blob (see its
    // doc comment in `providers/claude/mod.rs`); unpack them back into
    // `Account`'s dedicated fields.
    acct.oauth_account = captured
        .identity
        .native_blob
        .get("oauthAccount")
        .cloned()
        .filter(|v| !v.is_null());
    acct.user_id = captured
        .identity
        .native_blob
        .get("userID")
        .and_then(|v| v.as_str())
        .map(String::from);

    let (at_prefix, rt_prefix, expires_at) = (
        logging::tok_prefix(&acct.access_token),
        logging::tok_prefix(&acct.refresh_token),
        acct.expires_at,
    );
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
    logging::log(&format!(
        "event=capture account={email} existed={existed} at_prefix={at_prefix} \
         rt_prefix={rt_prefix} expires_at={expires_at}"
    ));
    Ok((email, existed))
}

/// The cached usage to keep when (re)capturing an account: the snapshot we
/// already had for this email, if any (capture only refreshes identity/tokens; a
/// freshly captured account has no snapshot of its own). Pure, for tests.
fn merged_cached_usage(existing: Option<&Account>) -> Option<CachedUsage> {
    existing.and_then(|a| a.cached_usage.clone())
}

/// Capture whatever `slug` (a non-Claude provider) currently has logged in
/// on this host and persist it into state v2's `State::providers[slug]`
/// slot, exactly like `capture_current` does for Claude's `accounts` list.
/// Closes the gap documented in `menubar.rs::handle_capture`'s prior
/// "persistence lands in a later phase" notification. Returns the account
/// key and whether it already existed (so callers can say "Captured" vs.
/// "Refreshed").
pub(crate) fn capture_current_generic(slug: &str) -> Result<(String, bool)> {
    let provider = provider_by_slug(slug)?;
    let captured = provider
        .capture_current_login()
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| anyhow!("nothing to capture — is {slug} logged in on this host?"))?;
    let key = provider.account_identifier(&captured.identity);
    let expires_at = Utc::now().timestamp() + captured.tokens.expires_in_secs;

    let existed = with_state_lock(|| {
        let mut state = State::load()?;
        let existing = state.find_provider_account(slug, &key).cloned();
        let existed = existing.is_some();
        let acct = ProviderAccount {
            key: key.clone(),
            secret_blob: captured.secret_blob.clone(),
            access_token: captured.tokens.access.clone(),
            refresh_token: captured.tokens.refresh.clone().unwrap_or_default(),
            expires_at,
            identity_email: captured.identity.email.clone(),
            identity_uuid: captured.identity.uuid.clone(),
            identity_display_name: captured.identity.display_name.clone(),
            identity_native_blob: captured.identity.native_blob.clone(),
            // Re-capturing only refreshes identity/tokens — keep any usage
            // snapshot the scheduler already fetched (mirrors
            // `merged_cached_usage` above for Claude).
            cached_usage: existing.as_ref().and_then(|a| a.cached_usage.clone()),
            notif_state: existing
                .as_ref()
                .map(|a| a.notif_state.clone())
                .unwrap_or_default(),
            needs_relogin: false,
        };
        state.upsert_provider_account(slug, acct);
        // Capture always reflects whatever is currently logged in, so it
        // also becomes the active account for this provider — matching
        // Claude's `capture_current`, which sets `state.active` the same way.
        state.provider_accounts_mut(slug).active = Some(key.clone());
        state.save()?;
        Ok(existed)
    })?;
    logging::log(&format!(
        "event=capture provider={slug} account={key} existed={existed}"
    ));
    Ok((key, existed))
}

/// Remove a captured account from a non-Claude provider's state v2 slot
/// (`State::providers[slug]`). Mirrors `remove_account` below for Claude —
/// added alongside state v2 so the "Remove…" row that now appears for a
/// provider whose accounts render in the menu (state v2 makes that possible
/// for the first time) doesn't dead-end.
pub(crate) fn remove_provider_account_generic(slug: &str, key: &str) -> Result<()> {
    with_state_lock(|| {
        let mut state = State::load()?;
        if !state.remove_provider_account(slug, key) {
            bail!("no {slug} account matches '{key}'");
        }
        state.save()
    })
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
    providers::init();
    let state = State::load()?;
    let no_claude = state.accounts.is_empty();
    let no_providers = state.providers.values().all(|p| p.accounts.is_empty());
    if no_claude && no_providers {
        println!("No accounts yet. Log into one with `claude` (or `codex`), then: usagio capture");
        return Ok(());
    }

    // Claude accounts (state v1 `accounts` bucket). Order by the same auto-pick
    // priority the menu uses (best switch target first, maxed/no-data accounts
    // sinking) instead of raw insertion order, so `usagio list` and the menu
    // bar agree on account order (user report: "account ordering seems off").
    if !no_claude {
        let mut rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
        rows.sort_by(menu_order);
        render_table(&rows, state.active.as_deref());
    }

    // Every other registered provider's captured accounts (state v2
    // `providers[slug]` slots), one section per provider in `providers::all()`
    // order — mirroring the menu-bar's per-provider sections so `usagio list`
    // and the menu agree (the CLI used to be Claude-only, so a captured Codex
    // account showed in the menu but never in `list`). A provider's own
    // `active` key marks its active row (keys can collide across providers —
    // e.g. the same email is both a Claude account and a Codex key — so each
    // section must be marked independently, not by a single global active).
    for provider in providers::all() {
        let slug = provider.provider_id();
        if slug == CLAUDE_SLUG {
            continue;
        }
        let Some(pa) = state.providers.get(slug) else {
            continue;
        };
        if pa.accounts.is_empty() {
            continue;
        }
        let mut rows: Vec<Row> = pa
            .accounts
            .iter()
            .map(|a| row_from_provider_account(slug, a))
            .collect();
        rows.sort_by(menu_order);
        println!("\n{}:", provider.display_name());
        render_table(&rows, pa.active.as_deref());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// switch / start / continue
// ---------------------------------------------------------------------------

fn cmd_switch(selector: Option<&str>, launch: Option<Launch>) -> Result<()> {
    let state = State::load()?;
    if state.accounts.is_empty() && state.providers.values().all(|p| p.accounts.is_empty()) {
        bail!("no accounts yet; capture one with: usagio capture");
    }

    // Explicit selector: try Claude's accounts first (unchanged v1 behavior,
    // including `start`/`continue` launching `claude` afterward). If it
    // doesn't resolve there, fall back to a provider-owned account slot
    // (state v2's `State::providers`) — this is the CLI half of routing a
    // switch by provider slug instead of assuming Claude, matching
    // `menubar.rs::handle_switch`.
    if let Some(sel) = selector {
        if !state.accounts.is_empty() {
            if let Ok(email) = state.resolve(sel) {
                return finish_claude_switch(&email, launch);
            }
        }
        if let Some((slug, key)) = resolve_provider_selector(&state, sel) {
            let label = switch_to_provider_account(&slug, &key)?;
            println!("Active login is now {label} ({slug}).");
            println!(
                "New `{slug}` sessions will use it. Already-running sessions keep their \
                 current account until they're restarted."
            );
            if launch.is_some() {
                println!(
                    "\nNote: `usagio start`/`continue` only launches the `claude` CLI; \
                     launch `{slug}` yourself to pick up this switch."
                );
            }
            return Ok(());
        }
        bail!("no account matches '{sel}'");
    }

    // No selector: unchanged v1 auto-pick behavior, Claude-only.
    if state.accounts.is_empty() {
        bail!("no accounts yet; capture one with: usagio capture");
    }
    let email = select_email(&state, None)?;
    finish_claude_switch(&email, launch)
}

/// Shared tail of `cmd_switch` for a resolved Claude email: perform the
/// switch, print the confirmation, and optionally launch `claude`.
fn finish_claude_switch(email: &str, launch: Option<Launch>) -> Result<()> {
    let label = switch_to(email)?;

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

/// Resolve `selector` (a full account key, or a unique case-insensitive
/// prefix) against every non-Claude provider's captured accounts in state.
/// Returns `(provider_slug, account_key)` on an unambiguous match. Mirrors
/// `State::resolve`'s prefix-matching contract, scoped across all provider
/// slots instead of one flat list, since account keys are only unique within
/// a single provider (a Codex and a future provider could share an email).
pub(crate) fn resolve_provider_selector(state: &State, selector: &str) -> Option<(String, String)> {
    let sel = selector.trim();
    if sel.is_empty() {
        return None;
    }
    let sel_lc = sel.to_lowercase();
    let mut exact: Vec<(String, String)> = Vec::new();
    let mut prefix: Vec<(String, String)> = Vec::new();
    for (slug, pa) in &state.providers {
        for a in &pa.accounts {
            let key_lc = a.key.to_lowercase();
            if key_lc == sel_lc {
                exact.push((slug.clone(), a.key.clone()));
            } else if key_lc.starts_with(&sel_lc) {
                prefix.push((slug.clone(), a.key.clone()));
            }
        }
    }
    if exact.len() == 1 {
        return Some(exact.into_iter().next().unwrap());
    }
    if exact.is_empty() && prefix.len() == 1 {
        return Some(prefix.into_iter().next().unwrap());
    }
    None
}

/// Make `key` the active login for the non-Claude provider `slug` (state
/// v2's `State::providers[slug]`). Mirrors `switch_to`'s contract for Claude:
/// rewrites the vendor's on-disk credential file via
/// `Provider::write_active_account`, then commits the new `active` selection
/// to state.json under the state lock. Returns the account key as the
/// display label (non-Claude providers have no separate display-name
/// resolution step the way Claude's identity backfill does).
pub(crate) fn switch_to_provider_account(slug: &str, key: &str) -> Result<String> {
    let provider = provider_by_slug(slug)?;
    if !provider.capabilities().supports_switching {
        bail!("switching is not supported for {slug}");
    }
    with_state_lock(|| {
        // Absorb any lagging on-disk rotation for the OUTGOING account BEFORE we
        // overwrite the vendor's credential file — the same guard Claude's
        // `switch_to_guarded` applies (main.rs, `absorb_before_switch`). Without
        // it, a single-use refresh token the vendor CLI rotated since our last
        // poll is left at its stale (now-dead) value, and a later switch-BACK
        // writes that dead token → forced interactive login, defeating the
        // "capture once, never re-login" guarantee. absorb rewrites state.json
        // under the reentrant lock, so load AFTER it to pick up the rotation.
        credentials::absorb_before_switch(provider);
        let mut state = State::load()?;
        let acct = state
            .find_provider_account(slug, key)
            .cloned()
            .ok_or_else(|| anyhow!("no {slug} account matches '{key}'"))?;
        let identity = acct.identity_snapshot();
        provider
            .write_active_account(&acct.secret_blob, &identity)
            .map_err(|e| anyhow!("switching {slug} account: {e}"))?;
        state.provider_accounts_mut(slug).active = Some(acct.key.clone());
        // The vendor credential file is already switched at this point; if
        // recording it in state.json fails, say so explicitly rather than
        // surfacing a bare io error that reads as "the whole switch failed"
        // (matches the Claude sibling path's context).
        state.save().context(
            "the login was switched but recording it in state.json failed; \
             run `usagio switch` again to reconcile the bookkeeping",
        )?;
        Ok(acct.key)
    })
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
    let set: std::collections::HashSet<String> = overrides.iter().map(|s| s.to_string()).collect();
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
    switch_to_guarded(
        provider,
        email,
        &acct,
        &identity,
        backfilled,
        Some(expect_active),
    )
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
    if state.active.as_deref() == Some(email) {
        // GUARANTEE: usagio must NEVER POST /token for the ACTIVE account.
        // Anthropic/OpenAI refresh tokens are single-use — a POST rotates the
        // whole family server-side and invalidates the copy the live vendor CLI
        // holds, forcing a `/login`. Switching TO the already-active account
        // (e.g. `usagio switch <active>` to relaunch, or clicking the active
        // row) must therefore ADOPT the vendor slot's current token, exactly
        // like the poll loop's active path — never mint one. See
        // `active_refresh_cas`'s doc.
        if matches!(
            active_refresh_cas(provider, &mut acct),
            ActiveRefreshOutcome::RefreshFailed
        ) {
            logging::log(&format!(
                "switch: active account {email} slot unreadable; keeping stored \
                 token (never POST /token for the active account)"
            ));
        }
    } else {
        // Inactive account: usagio owns its token, so a reactive refresh is
        // safe. On a hard failure (single-use token rotated elsewhere / revoked)
        // bail with a distinctive error so the menu / CLI can prompt a fresh
        // login instead of writing a stale token that `claude` will reject.
        match ensure_fresh_with_fallback(provider, email, &mut acct) {
            Ok(_) => {}
            Err(oauth::RefreshError::InvalidGrant) => {
                flag_needs_relogin(email);
                bail!(
                    "account {email} needs re-login (refresh token rejected); run \
                     `claude /login` for it or click the account row in the menu"
                );
            }
            Err(e) => bail!("token refresh for {email} failed: {e}"),
        }
    }
    let (identity, backfilled) = resolve_identity(provider, &acct)?;
    Ok((acct, identity, backfilled))
}

/// `ensure_fresh` with a disk-fallback safety net for the reactive paths
/// (`cmd_token`, `prepare_switch`). If the initial refresh fails with
/// `InvalidGrant`, the stored refresh token may just be behind a rotation the
/// vendor CLI (or another usagio process) already wrote to disk — or fsnotify
/// may have missed the FSEvent (common after a suspend/resume). Rescan every
/// registered credential path via `credentials::last_chance_fallback`; if a
/// usable blob for this account is sitting on disk, adopt it into state.json
/// and retry the refresh once with the fresh grant. This matches the policy
/// `refresh_inactive_if_stale` already uses on the proactive path so all three
/// entry points agree before we surface "needs re-login" to the user.
fn ensure_fresh_with_fallback(
    provider: &'static dyn Provider,
    email: &str,
    acct: &mut Account,
) -> std::result::Result<bool, oauth::RefreshError> {
    match oauth::ensure_fresh(acct, REFRESH_SKEW_SECS) {
        Ok(b) => Ok(b),
        Err(oauth::RefreshError::InvalidGrant) => {
            let key = crate::providers::trait_def::AccountKey::new(provider.provider_id(), email);
            if credentials::last_chance_fallback(provider, &key) {
                // Reload the account with the tokens the fallback just
                // adopted, then retry the refresh once. If the reload fails
                // or the account vanished, fall through to InvalidGrant.
                if let Ok(st) = State::load() {
                    if let Some(fresh) = st.find(email).cloned() {
                        *acct = fresh;
                        return oauth::ensure_fresh(acct, REFRESH_SKEW_SECS);
                    }
                }
            }
            Err(oauth::RefreshError::InvalidGrant)
        }
        Err(e) => Err(e),
    }
}

/// Best-effort: mark an account `needs_relogin=true` in state.json. If the
/// state lock is held or the save fails, the next `refresh_usage_cache` tick
/// re-observes the invalid_grant and sets it again — callers that need
/// certainty already surface a distinct error to the user.
fn flag_needs_relogin(email: &str) {
    // L7 (round-1 codeaudit): the previous `let _ =` swallowed save errors
    // silently, so a persistent state.json write failure would never be
    // visible in the log. The next `refresh_usage_cache` tick re-sets the
    // flag from the invalid_grant observation, so we still make progress,
    // but the failure should be noisy.
    logging::log(&format!(
        "event=needs_relogin account={email} reason=invalid_grant"
    ));
    if let Err(e) = with_state_lock(|| {
        let mut st = State::load()?;
        if let Some(a) = st.find_mut(email) {
            a.needs_relogin = true;
        }
        st.save()
    }) {
        logging::log(&format!("flag_needs_relogin({email}): save failed: {e:#}"));
    }
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
        // Provider-agnostic: absorb any lagging on-disk rotations for the
        // outgoing account BEFORE we overwrite path[0] with the incoming
        // account's blob. This catches every configured credential file.
        credentials::absorb_before_switch(provider);
        // absorb_before_switch reaches back through the reentrant state lock
        // and rewrites state.json with any freshly-absorbed rotations. Our
        // in-memory `st` snapshot from a few lines above is now stale; reload
        // it here or our final `st.save()` will clobber the absorbed changes
        // (reintroducing the never-re-login regression). See H1 in the
        // round-1 codeaudit findings.
        //
        // NOTE: sync_active_from_keychain MUST run AFTER this reload, not
        // before it — otherwise the reload discards any keychain-derived
        // mutations to the in-memory snapshot, causing apply_account (via
        // the stale `st`) to later re-push pre-rotation tokens on switch-
        // back. See H_R2_1 in the round-2 codeaudit findings.
        st = State::load()?;
        // Preserve any token rotation the currently-active session picked up
        // and wrote into the keychain (Claude legacy path). This mutates `st`
        // in memory; the final `st.save()` below persists the freshest tokens.
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
        let from = st.active.clone();
        // ~/.claude.json first, keychain last (the commit point), rollback on fail.
        apply_account(provider, &acct, identity, from.as_deref(), &label)?;
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
             run `usagio capture` for it while logged in"
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
    from: Option<&str>,
    to: &str,
) -> Result<()> {
    let _ = provider;
    let prior = read_claude_json_raw();
    let identity_result = write_claude_identity(identity, acct.user_id.as_deref());
    if let Err(e) = &identity_result {
        logging::log(&format!(
            "event=switch from={} to={to} identity_written=err:{e:#} keychain_written=skipped",
            from.unwrap_or("<none>"),
        ));
        identity_result?;
    }
    if let Err(e) = keychain_write(&acct.keychain_blob) {
        if let Some((bytes, mode)) = &prior {
            // If the rollback ALSO fails we're half-applied (~/.claude.json points
            // at the new account, keychain still holds the old) — surface that
            // explicitly rather than silently swallowing the rollback error.
            if let Err(re) = restore_claude_json_raw(bytes, *mode) {
                logging::log(&format!(
                    "event=switch from={} to={to} identity_written=ok \
                     keychain_written=err:{e:#} rollback=err:{re:#}",
                    from.unwrap_or("<none>"),
                ));
                return Err(e).context(format!(
                    "writing the account into the keychain, and rolling back \
                     ~/.claude.json failed too ({re:#}); it may now point at the new \
                     account while the keychain holds the old — run `usagio \
                     switch` again to reconcile"
                ));
            }
        }
        logging::log(&format!(
            "event=switch from={} to={to} identity_written=ok keychain_written=err:{e:#}",
            from.unwrap_or("<none>"),
        ));
        return Err(e).context("writing the account into the keychain");
    }
    logging::log(&format!(
        "event=switch from={} to={to} identity_written=ok keychain_written=ok",
        from.unwrap_or("<none>"),
    ));
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

/// Best-effort mirror of a rotated INACTIVE account's new grant back to the
/// vendor CLI's OS-native slot — but ONLY if the vendor's own active identity
/// (`read_active_identity`) currently agrees this account is the one it's
/// using. Claude Code's keychain / credentials-file slot is a single shared
/// resource across every locally known account — there is no per-account
/// file — so blindly mirroring an inactive account's rotation into it would
/// silently clobber whatever account is genuinely logged in right now. This
/// is the write-path counterpart of the identity check `sync_active_from_
/// keychain` already uses before adopting a rotation FROM the slot.
///
/// Returns `None` when mirroring was skipped (no vendor identity, or it
/// doesn't match this account) — the caller logs that as `mirror_back=skip`,
/// distinct from an attempted mirror that failed (`Some(Err(_))`).
fn mirror_inactive_rotation(
    provider: &'static dyn Provider,
    acct: &Account,
) -> Option<std::result::Result<(), String>> {
    // Serialize the identity-check + keychain write against a concurrent switch
    // by holding the cross-process state lock across BOTH, and re-read the
    // vendor identity INSIDE the lock. A switch (`usagio switch` in another
    // process, or a menu switch on the main thread) holds the same lock across
    // its ~/.claude.json + keychain writes, so we observe either the pre-switch
    // identity (still matches → safe to mirror) or the post-switch identity
    // (mismatch → skip) — never a torn in-between where this inactive account's
    // rotated token would be written over the account a switch just committed,
    // clobbering a valid active token. `ACTIVE_SLOT_LOCK` alone can't close this
    // race: it's in-process only, and the racing switch is often a separate
    // process. This lock is held across a `security(1)` write, but the mirror
    // only runs when a rotation was actually detected (rare), and the switch
    // path already holds the state lock across its own keychain write.
    let outcome = with_state_lock(|| {
        let vendor_identity = match provider.read_active_identity() {
            Ok(Some(id)) => id,
            _ => return Ok(None),
        };
        let matches = identity_matches(
            acct.identity_uuid().as_deref(),
            acct.email.as_deref(),
            vendor_identity.uuid.as_deref(),
            vendor_identity
                .email
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref(),
        );
        if !matches {
            return Ok(None);
        }
        Ok(Some(
            provider
                .mirror_rotated_token(&acct.keychain_blob)
                .map_err(|e| e.to_string()),
        ))
    });
    // On the Ok path, the inner Option is the mirror outcome; if we couldn't
    // even acquire the lock / run the closure this cycle, skip (None) — the
    // same "do nothing" treatment as a vendor-identity read miss.
    outcome.unwrap_or_default()
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
        logging::log(&format!(
            "sync: keychain identity does not match the active account; not adopting tokens \
             (event=sync_active_from_keychain account={active} outcome=identity_mismatch)"
        ));
        return;
    }
    logging::log(&format!(
        "event=sync_active_from_keychain account={active} outcome=adopted \
         at_prefix={} rt_prefix={}",
        logging::tok_prefix(&fresh.access_token),
        logging::tok_prefix(&fresh.refresh_token),
    ));
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
            [] => bail!("no accounts; capture one with: usagio capture"),
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
    // Reactive path: try the disk fallback before flagging so a rotation the
    // vendor CLI wrote to ~/.claude/.credentials.json (or that fsnotify missed)
    // is adopted here instead of tripping needs_relogin on a token that's
    // already been superseded on disk.
    let provider = providers::get(CLAUDE_SLUG)
        .ok_or_else(|| anyhow!("internal: provider '{CLAUDE_SLUG}' not registered"))?;
    let refreshed = if state.active.as_deref() == Some(email.as_str()) {
        // GUARANTEE: never POST /token for the ACTIVE account (single-use refresh
        // token would invalidate the live vendor CLI's copy → forced re-login).
        // `usagio token` for the active account ADOPTS the vendor slot's current
        // token instead of minting one; a script gets the same bearer the vendor
        // CLI is using. If the slot is unreadable we return the stored token
        // rather than POST. See `active_refresh_cas`'s doc.
        match active_refresh_cas(provider, &mut acct) {
            ActiveRefreshOutcome::Adopted => true,
            ActiveRefreshOutcome::RefreshFailed => {
                logging::log(&format!(
                    "token: active account {email} slot unreadable; returning stored \
                     token (never POST /token for the active account)"
                ));
                false
            }
        }
    } else {
        match ensure_fresh_with_fallback(provider, &email, &mut acct) {
            Ok(b) => b,
            Err(oauth::RefreshError::InvalidGrant) => {
                flag_needs_relogin(&email);
                bail!(
                    "account {email} needs re-login (refresh token rejected); run \
                     `claude /login` for it or click the account row in the menu"
                );
            }
            Err(e) => bail!("token refresh for {email} failed: {e}"),
        }
    };
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
                 failed; if refreshes start failing, run `usagio capture` for \
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
    let selector = selector.context("usage: usagio rm <email>")?;
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
    /// True when the OAuth token endpoint has permanently rejected this
    /// account's refresh token. Filtered out of the auto-swap picker and
    /// surfaced by the menu as a re-login row.
    needs_relogin: bool,
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
        // `State::accounts` is Claude's dedicated slot (state v2 keeps it
        // that way rather than folding it into `State::providers` — see
        // `store.rs`'s `State` doc), so every row built from it is Claude's.
        provider_id: CLAUDE_SLUG.to_string(),
        needs_relogin: a.needs_relogin,
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

/// Build a display row from a non-Claude provider's captured account
/// (`State::providers[slug]`). Mirrors `row_from_account` exactly, just
/// reading `ProviderAccount`'s generic `cached_usage` instead of `Account`'s.
/// No `opus` window — that's a Claude-specific bucket (see `window_order`
/// filtering it back out for providers that don't declare it).
pub(crate) fn row_from_provider_account(slug: &str, a: &ProviderAccount) -> Row {
    let c = a.cached_usage.as_ref();
    Row {
        provider_id: slug.to_string(),
        needs_relogin: a.needs_relogin,
        email: a.key.clone(),
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
/// ones (maxed out, or no data yet) sinking to the bottom. Used by BOTH the
/// menu bar and `usagio list` so the two surfaces present accounts in the same
/// order.
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
            "no usage data yet — let the menu-bar app or `usagio watch` \
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

/// Run `f` holding an exclusive advisory lock on ~/.config/usagio/lock,
/// serializing state read-modify-write across processes (the daemon poll and
/// concurrent CLI/menu commands). The lock is fd-scoped, so the kernel releases
/// it if the holder dies. Reentrant on the current thread, so provider-
/// implemented callbacks (e.g. `absorb_credential`) can safely take the lock
/// again from inside another `with_state_lock` frame without self-deadlocking
/// (see credentials::with_state_lock for the depth-tracking rationale). Do NOT
/// do network I/O inside `f`.
fn with_state_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    credentials::with_state_lock(f)
}

// ---------------------------------------------------------------------------
// Keychain helpers — delegate to the platform SecretStore
// ---------------------------------------------------------------------------

fn keychain_account() -> String {
    std::env::var("USER").unwrap_or_else(|_| "claude".to_string())
}

// NOTE: on macOS the SecretStore impl uses the `security` CLI rather than the
// Security.framework (`security-framework`) API on purpose. SecItem access
// from this unsigned, brew-installed binary makes macOS prompt on every
// launch (an unsigned binary has no stable identity for "Always Allow" to pin
// to). The CLI path doesn't prompt. The only downside is the write blob
// appears in `security`'s argv, which is LOW risk under this tool's
// single-user threat model. TODO: switch back to security-framework once we
// ship a code-signed (Developer ID) build, where "Always Allow" persists.
fn keychain_read() -> Option<String> {
    platform()
        .secrets()
        .get(KEYCHAIN_SERVICE, &keychain_account())
        .ok()
        .flatten()
}

fn keychain_write(blob: &str) -> Result<()> {
    let result = platform()
        .secrets()
        .set(KEYCHAIN_SERVICE, &keychain_account(), blob);
    // Feed the keychain-write health signal the watchdog watches: a run of
    // consecutive failures is the fingerprint of the fd-exhaustion EMFILE that
    // silently broke account switching (see `src/watchdog.rs`). A success
    // resets the counter.
    watchdog::record_keychain_result(result.is_ok());
    result
}

/// Internal, undocumented CLI hook used ONLY by
/// `tests/integration_linux_secrets.rs` (and any equivalent test on other
/// OSes) to round-trip `Platform::secrets()` against whatever the real
/// backend is on this machine — the real D-Bus Secret Service daemon on
/// Linux CI, Keychain on macOS, Credential Manager on Windows. This crate
/// has no `[lib]` target, so an integration test can't call `LinuxSecrets`
/// directly the way a unit test in `src/platform/linux.rs` can; this
/// subcommand is the black-box seam (same shape as every other
/// `tests/cli.rs` test driving the compiled binary as a subprocess).
///
/// Deliberately not listed in `print_help` — it exists purely as a test
/// fixture, not a user-facing feature. Round-trips a caller-supplied
/// `(service, account, secret)` through delete → get(None) → set →
/// get(Some) → delete → get(None), printing `OK` and exiting 0 on success,
/// or returning an `Err` (non-zero exit, message on stderr) describing
/// exactly which step diverged.
///
/// Usage: `usagio __secrets_selftest <service> <account> <secret>`
fn cmd_secrets_selftest(args: &[String]) -> Result<()> {
    let (service, account, secret) = match args {
        [service, account, secret] => (service.as_str(), account.as_str(), secret.as_str()),
        _ => bail!("usage: usagio __secrets_selftest <service> <account> <secret>"),
    };
    let store = platform().secrets();

    // Start from a clean slate in case a previous run crashed mid-round-trip
    // and left a stale entry behind under this test-scoped service name.
    let _ = store.delete(service, account);
    if store.get(service, account)?.is_some() {
        bail!("secrets_selftest: secret unexpectedly present before set()");
    }

    store.set(service, account, secret)?;
    let got = store.get(service, account)?;
    if got.as_deref() != Some(secret) {
        bail!("secrets_selftest: get() after set() returned {got:?}, expected Some({secret:?})");
    }

    store.delete(service, account)?;
    if store.get(service, account)?.is_some() {
        bail!("secrets_selftest: secret still present after delete()");
    }

    println!("OK");
    Ok(())
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
    let tmp = path.with_extension("json.usagio.tmp");
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
// Active-account refresh — compare-and-swap on the OS keychain
// ---------------------------------------------------------------------------
//
// Claude Code (the vendor CLI) may rotate the same OS-native credential slot
// (macOS keychain / `~/.claude/.credentials.json`) at any moment — its
// refresh_token is single-use, and Anthropic invalidates the whole family the
// instant either side rotates. A naive "usagio also refreshes the active
// account" would race Claude Code's own rotation and strand one side with a
// dead grant (the never-re-login regression this file's history is full of).
// A naive "usagio never refreshes the active account" (the previous
// redesign, commit 735b762) avoids the race but means an active account left
// idle long enough goes stale with nobody to refresh it.
//
// The fix is a compare-and-swap: read the slot before refreshing, refresh
// only if we're provably the sole owner of the current token generation, then
// read the slot again before committing. If Claude Code touched the slot at
// any point in that window, our own (already-stale) grant is discarded and
// Claude Code's rotation is adopted instead — usagio never fights Claude Code
// for the write, but it DOES rotate the token when nobody else is racing it.

/// Outcome of one `active_refresh_adopt` pass.
#[derive(Debug)]
enum ActiveRefreshOutcome {
    /// Read the slot Claude Code owns and synced our in-memory `acct` to
    /// whatever it currently holds (whether it had drifted or was already in
    /// sync). No token was minted.
    Adopted,
    /// The slot was unreadable/unparseable; `acct` is left untouched and the
    /// caller keeps the existing cache for this cycle.
    RefreshFailed,
}

/// Sync the ACTIVE account's tokens FROM the slot Claude Code owns — usagio is
/// a pure follower here and MUST NEVER mint a token for the active account.
///
/// Why no `/token` POST: Anthropic's refresh tokens are **single-use**. Posting
/// `refresh_token` rotates the whole token family *server-side* and permanently
/// invalidates every other copy — including the one Claude Code is holding. The
/// moment usagio refreshes the active account, Claude Code's next refresh comes
/// back `invalid_grant` and it drops the user into a `/login`. This is
/// irreversible: the damage is the POST itself, not any keychain write, so no
/// amount of before/after compare-and-swap can undo it. (The prior "CAS"
/// design POSTed and then tried to discard its own grant on a detected race —
/// that never protected Claude Code, because Anthropic had already burned CC's
/// token by the time we read the slot back.)
///
/// Claude Code refreshes the active account on its own cadence; usagio's only
/// job is to read that slot and adopt whatever it holds, keeping our state.json
/// in sync so a later `switch` writes back the correct (current) blob. The
/// adopted access token is also what the usage fetch uses this cycle. If Claude
/// Code is dormant and its cached access token has expired, the usage fetch
/// simply fails and we keep the previous cache — a cosmetic staleness, never a
/// login break.
///
/// MUST be run inside `with_state_lock` by the caller (see
/// `refresh_usage_cache`). `acct` is the caller's just-reloaded in-memory copy;
/// it is mutated in place. This function does not save state.json — the caller
/// does that once, after this returns.
fn active_refresh_cas(provider: &dyn Provider, acct: &mut Account) -> ActiveRefreshOutcome {
    let email = acct.email.clone().unwrap_or_default();

    let before = match provider.read_active_slot() {
        Ok(Some(b)) => b,
        Ok(None) => {
            logging::log(&format!(
                "event=active_refresh_adopt_failed account={email} reason=slot_empty"
            ));
            return ActiveRefreshOutcome::RefreshFailed;
        }
        Err(e) => {
            logging::log(&format!(
                "event=active_refresh_adopt_failed account={email} reason=read:{e}"
            ));
            return ActiveRefreshOutcome::RefreshFailed;
        }
    };
    let cc_acct = match Account::from_keychain_blob(&before) {
        Ok(a) => a,
        Err(e) => {
            logging::log(&format!(
                "event=active_refresh_adopt_failed account={email} reason=parse:{e:#}"
            ));
            return ActiveRefreshOutcome::RefreshFailed;
        }
    };

    let drifted = cc_acct.access_token != acct.access_token;
    let cc_prefix = logging::tok_prefix(&cc_acct.access_token);
    acct.set_tokens(
        cc_acct.access_token,
        cc_acct.refresh_token,
        cc_acct.expires_at,
    );
    acct.keychain_blob = before;
    // The active slot is, by definition, what Claude Code is actively using —
    // adopting it means we now hold a valid grant, so clear any stale flag.
    acct.needs_relogin = false;
    if drifted {
        logging::log(&format!(
            "event=active_refresh_adopted account={email} adopted_at_prefix={cc_prefix}.. \
             note=cc_rotated_since_last_cycle"
        ));
    } else {
        logging::log(&format!(
            "event=active_refresh_in_sync account={email} at_prefix={cc_prefix}.."
        ));
    }
    ActiveRefreshOutcome::Adopted
}

// ---------------------------------------------------------------------------
// Active-refresh CAS dispatch for non-Claude providers (state v2)
// ---------------------------------------------------------------------------

/// Run the active-account CAS refresh for a non-Claude provider whose active
/// login is tracked in `State::providers[slug]` (currently only `codex`).
/// Gap-4 of the codex-switch-e2e work: `active_refresh_cas` above is
/// irreducibly Claude-shaped (it operates on `Account`/the macOS keychain
/// slot), so rather than force Codex through that function's signature, this
/// is the per-provider dispatch point `main.rs::active_refresh_cas` used to
/// lack — for Codex it drives `providers::codex::oauth::active_refresh_cas`
/// (the file-CAS on `auth.json`, already implemented and unit-tested) instead
/// of any Claude-shaped flow. Gated on `Provider::supports_active_refresh()`
/// exactly like the Claude path above, so a provider that hasn't opted in
/// costs nothing here.
fn refresh_provider_active_account(slug: &str) {
    let Some(provider) = providers::get(slug) else {
        return;
    };
    if !provider.supports_active_refresh() {
        return;
    }
    let result = with_state_lock(|| {
        let mut state = State::load()?;
        let Some(active_key) = state.provider_accounts(slug).and_then(|p| p.active.clone()) else {
            return Ok(()); // nothing captured / nothing active for this provider yet
        };
        let Some(acct) = state.find_provider_account(slug, &active_key).cloned() else {
            return Ok(()); // active key points at an account that's since been removed
        };
        if acct.needs_relogin {
            return Ok(());
        }
        let outcome = provider_active_refresh(slug, &acct);
        match outcome {
            ProviderRefreshOutcome::Nothing => return Ok(()),
            ProviderRefreshOutcome::Refreshed(new_blob, grant) => {
                if let Some(a) = state.find_provider_account_mut(slug, &active_key) {
                    a.secret_blob = new_blob;
                    a.access_token = grant.access.clone();
                    if let Some(r) = grant.refresh.clone() {
                        a.refresh_token = r;
                    }
                    a.expires_at = Utc::now().timestamp() + grant.expires_in_secs;
                    a.needs_relogin = false;
                }
                state.save()?;
            }
            ProviderRefreshOutcome::InvalidGrant => {
                // Vendor rejected our refresh_token — user must re-login. Flip
                // the flag so the menu bar row surfaces "needs re-login" instead
                // of silently retrying the same dead token every poll cycle.
                if let Some(a) = state.find_provider_account_mut(slug, &active_key) {
                    if !a.needs_relogin {
                        logging::log(&format!(
                            "event=needs_relogin_flipped provider={slug} \
                             account={active_key} reason=invalid_grant"
                        ));
                        a.needs_relogin = true;
                    }
                }
                state.save()?;
            }
            ProviderRefreshOutcome::Failed => {
                // Already logged; leave state untouched, retry next cycle.
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        logging::log(&format!(
            "poll: {slug} active-refresh CAS state operation failed: {e:#}"
        ));
    }
}

/// Outcome of a provider-specific active-account refresh cycle, so the caller
/// can distinguish "vendor rejected the grant → flip needs_relogin" from
/// "network hiccup → retry next cycle" from "nothing to do".
enum ProviderRefreshOutcome {
    /// Fresh enough already, or CAS lost / drift adopted (state was already
    /// updated in-place by the CAS primitive); caller does nothing.
    Nothing,
    /// New blob + token grant to persist against the active account.
    Refreshed(String, TokenGrant),
    /// Vendor returned invalid_grant (RFC 6749 400/401). The refresh token
    /// is dead — user must re-login. Caller flips `needs_relogin`.
    InvalidGrant,
    /// Anything else (network transient, 5xx, parse error, etc.) — already
    /// logged. Leave state alone, retry next cycle.
    Failed,
}

/// The provider-specific half of `refresh_provider_active_account`: drive
/// whatever CAS primitive `slug` implements and normalize the outcome. Only
/// Codex is wired today; a future provider adds an arm here rather than a
/// parallel copy of the state-locking logic above.
fn provider_active_refresh(slug: &str, acct: &ProviderAccount) -> ProviderRefreshOutcome {
    match slug {
        #[cfg(feature = "codex")]
        "codex" => codex_active_refresh(acct),
        // Reached only when a provider's `supports_active_refresh()` returned
        // true (the caller `refresh_provider_active_account` gates on it) but
        // no dispatch arm was added here — a wiring mistake that would silently
        // let the active account's token rot. Fail loud in debug/test builds
        // and log in release rather than returning a benign-looking Nothing.
        _ => {
            debug_assert!(
                false,
                "provider '{slug}' opted into supports_active_refresh() but has no \
                 provider_active_refresh arm — its active token will never refresh"
            );
            crate::logging::log(&format!(
                "event=active_refresh_unwired provider={slug} msg=\"supports_active_refresh \
                 but no dispatch arm; active token not refreshed\""
            ));
            ProviderRefreshOutcome::Nothing
        }
    }
}

/// Codex's active-account half: usagio is a pure FOLLOWER of the slot the
/// Codex CLI owns (`~/.codex/auth.json`). It NEVER POSTs `/token` for the
/// active account — Codex's refresh tokens are single-use, so minting one would
/// rotate the token family server-side and invalidate the copy the CLI holds,
/// forcing an interactive `codex login` (see `codex::oauth::active_refresh_cas`,
/// and the Claude analog `main.rs::active_refresh_cas`). It reads the slot and,
/// if the CLI has rotated it since we last saw it, adopts whatever the CLI now
/// holds via `capture_current_login` — reusing the provider's own parsing
/// instead of hand-rolling a blob->TokenGrant conversion — so the result is
/// consistent with a fresh capture. The inactive-account path
/// (`CodexProvider::refresh_token`) still refreshes normally; the CLI isn't
/// using those logins.
#[cfg(feature = "codex")]
fn codex_active_refresh(acct: &ProviderAccount) -> ProviderRefreshOutcome {
    use providers::codex::oauth::{active_refresh_cas, CasOutcome, RefreshError};
    match active_refresh_cas(Some(&acct.secret_blob)) {
        Ok(CasOutcome::Fresh) => ProviderRefreshOutcome::Nothing,
        Ok(CasOutcome::Adopted(_)) => {
            let Some(provider) = providers::get("codex") else {
                return ProviderRefreshOutcome::Nothing;
            };
            match provider.capture_current_login() {
                Ok(Some(captured)) => {
                    ProviderRefreshOutcome::Refreshed(captured.secret_blob, captured.tokens)
                }
                Ok(None) => ProviderRefreshOutcome::Nothing,
                Err(e) => {
                    logging::log(&format!(
                        "event=active_refresh_cas_failed provider=codex \
                         reason=adopt_reread:{e}"
                    ));
                    ProviderRefreshOutcome::Failed
                }
            }
        }
        // `active_refresh_cas` never mints a token for the active account, so it
        // can no longer return `InvalidGrant`; keep the arm so the generic
        // `needs_relogin` machinery stays wired for any future provider whose
        // active-refresh primitive can still reject a grant.
        Err(RefreshError::InvalidGrant) => {
            logging::log("event=active_refresh_cas_failed provider=codex reason=invalid_grant");
            ProviderRefreshOutcome::InvalidGrant
        }
        Err(e) => {
            logging::log(&format!(
                "event=active_refresh_cas_failed provider=codex reason={e}"
            ));
            ProviderRefreshOutcome::Failed
        }
    }
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
    let mut updates: Vec<(
        String,
        Account,
        Option<CachedUsage>,
        Option<notifications::NotifState>,
    )> = Vec::new();
    let emails: Vec<String> = state.accounts.iter().map(|a| a.key().to_string()).collect();
    // Settings ▸ Notifications ▸: per-trigger enable flags, persisted on
    // `State` and toggled from the menu bar (`menubar::toggle_notification_trigger`).
    let notif_cfg = state.notification_config.clone();
    for email in &emails {
        let Some(acct) = state.find(email).cloned() else {
            continue;
        };
        let mut acct = acct;
        // Skip accounts already flagged needs_relogin — no useful token to
        // reason about, and hammering an invalid_grant refresh both wastes
        // requests and can trip rate limits shared across the tenant.
        if acct.needs_relogin {
            continue;
        }
        // Locked accounts don't change until their reset — skip the network
        // refresh entirely, keeping the (accurate) locked cache. The
        // reset-boundary wake (`menubar::next_reset_wake_secs`) brings the
        // poller back at expiry, when `is_locked_until_reset` flips false and
        // the account refreshes to its fresh post-reset reading. This is the
        // primary rate-limit guard: a shelf of 100%-locked accounts polled
        // every cycle exhausts the shared tenant's request budget and
        // 429-starves the accounts that actually need refreshing (what froze
        // an unlocked account at a stale reading). The ACTIVE account is never
        // skipped — its live reading must stay current.
        let is_the_active_account = state.active.as_deref() == Some(email.as_str());
        if !is_the_active_account {
            if let Some(cu) = &acct.cached_usage {
                if countdown::is_locked_until_reset(&cached_to_account_usage(cu), Utc::now()) {
                    logging::log(&format!(
                        "poll: {email} locked until reset; skipping refresh (event=refresh_skip_locked account={email})"
                    ));
                    continue;
                }
            }
        }
        // The ACTIVE account gets the compare-and-swap treatment (see the
        // `active_refresh_cas` doc above): usagio DOES rotate its token, but
        // the read-refresh-read sequence is run entirely under the state
        // lock so it can never race a concurrent switch/capture in this
        // process, and it backs off cleanly the instant it observes Claude
        // Code touched the same slot. Every other account uses the plain
        // network-outside-the-lock refresh below.
        // H3/gap-4 (v0.5.0 codeaudit + codex-switch-e2e): `supports_active_refresh`
        // used to be read nowhere outside its own doc comment — this is the
        // gate that makes it load-bearing. A provider that can't do a
        // programmatic refresh grant (or, before this change, hasn't
        // explicitly opted in) falls through to the plain `ensure_fresh`
        // path below even for its active account, same as an inactive one.
        let is_active =
            state.active.as_deref() == Some(email.as_str()) && provider.supports_active_refresh();
        if is_active {
            let email_owned = email.clone();
            let cas = with_state_lock(|| {
                let mut st = State::load()?;
                let outcome = st
                    .find_mut(&email_owned)
                    .map(|a| active_refresh_cas(provider, a));
                if outcome.is_some() {
                    st.save()?;
                }
                let fresh = st.find(&email_owned).cloned();
                Ok((outcome, fresh))
            });
            match cas {
                Ok((Some(ActiveRefreshOutcome::RefreshFailed), _)) => {
                    // Keep the existing cache this cycle; retry next tick.
                    continue;
                }
                Ok((Some(_), Some(fresh))) => {
                    // `Adopted` leaves `fresh` holding the tokens Claude Code
                    // owns, which is what the usage fetch below uses.
                    acct = fresh;
                }
                Ok((Some(_), None)) | Ok((None, _)) => {
                    // Account vanished (a concurrent `rm`) mid-cycle.
                    continue;
                }
                Err(e) => {
                    logging::log(&format!(
                        "poll: active CAS state operation failed for {email}: {e:#}"
                    ));
                    continue;
                }
            }
        } else {
            match oauth::ensure_fresh(&mut acct, REFRESH_SKEW_SECS) {
                Ok(true) => {
                    let old_prefix = logging::tok_prefix(&acct.access_token);
                    // mirror_back is `skip` (not `ok`/`err`) when the vendor
                    // CLI's own active identity doesn't match this account —
                    // Claude Code's keychain/credentials-file slot is a
                    // single shared resource across every locally known
                    // account (there's no per-account file), so blindly
                    // mirroring an inactive account's rotation into it would
                    // clobber whichever account is genuinely logged in right
                    // now. Only mirror when the vendor agrees this account IS
                    // its current active identity.
                    let mirror_back = mirror_inactive_rotation(provider, &acct);
                    logging::log(&format!(
                        "event=inactive_refresh account={email} at_prefix={old_prefix}.. -> \
                         {}.. mirror_back={}",
                        logging::tok_prefix(&acct.access_token),
                        match &mirror_back {
                            Some(Ok(())) => "ok",
                            Some(Err(_)) => "err",
                            None => "skip",
                        }
                    ));
                }
                Ok(false) => {}
                Err(oauth::RefreshError::InvalidGrant) => {
                    logging::log(&format!(
                        "token refresh permanently rejected for {email} (invalid_grant); \
                         flagging for re-login"
                    ));
                    logging::log(&format!(
                        "event=needs_relogin account={email} reason=invalid_grant"
                    ));
                    acct.needs_relogin = true;
                    // Fall through to the merge step so the flag is persisted;
                    // skip the usage fetch — a rejected refresh means we don't
                    // have a usable access token to try the usage endpoint with.
                    updates.push((email.clone(), acct, None, None));
                    continue;
                }
                Err(e) => {
                    logging::log(&format!(
                        "token refresh failed for {email}: {e} (keeping cache)"
                    ));
                    continue;
                }
            }
        }
        let cu = match usage::fetch(&acct.access_token) {
            Ok(u) => Some(cached_from_usage(&u)),
            Err(usage::FetchError::RateLimited) => {
                rate_limited = true;
                logging::log(&format!("usage 429 for {email}; keeping cache"));
                None
            }
            Err(usage::FetchError::Auth) if is_active => {
                // The active account's token was invalidated between our CAS
                // and this fetch (Claude Code rotated again in that narrow
                // window). Not ours to fix this cycle — skip and let the
                // next cycle's CAS pick up the new rotation.
                logging::log(&format!(
                    "active account token invalidated for {email}; waiting for Claude Code to rotate"
                ));
                continue;
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
            if let Some(prev) = prev_snap {
                let mut ns = acct.notif_state.clone();
                let raw = notifications::evaluate(&prev, &curr_snap, &notif_cfg);
                let mut kept = notifications::dedup_and_apply(&mut ns, &prev, &curr_snap, raw);
                // Pace check (default off) — feeds through the same dedup path.
                // Read pace (historical slope) BEFORE the append below: appending
                // curr_snap advances the current-month file's mtime and would
                // otherwise invalidate the usage-log cache mid-tick, forcing pace
                // (and the next account's last_snapshot) to re-parse the whole
                // month. curr_snap is passed to evaluate_pace explicitly, so the
                // current point is still considered; only the historical read is
                // reordered (v0.5.17 PERF).
                let pace = usage_log::pace(&account_key);
                if let Some(pt) =
                    notifications::evaluate_pace(pace.as_ref(), &curr_snap, &notif_cfg, Utc::now())
                {
                    let extra =
                        notifications::dedup_and_apply(&mut ns, &prev, &curr_snap, vec![pt]);
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
            // Append LAST: all reads for this account (last_snapshot above, pace
            // in the prev block) have completed against the warm cache.
            if let Err(e) = usage_log::append(&curr_snap) {
                logging::log(&format!("usage_log: append failed for {email}: {e:#}"));
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
                // Propagate the flag from the phase-1/2 snapshot to the
                // locked state. `set_tokens_if_newer` only writes on a real
                // refresh, so we mirror this bit explicitly here — but only
                // ever OR it in. A plain overwrite (`a.needs_relogin =
                // acct.needs_relogin`) could silently clobber `true` back to
                // `false` if a concurrent path (switch, capture,
                // `flag_needs_relogin`) set the flag on the locked state
                // between our phase-1 snapshot and this merge (errors-01,
                // v0.5.2 codeaudit) — a re-login that genuinely IS needed
                // would stop being shown.
                a.needs_relogin = a.needs_relogin || acct.needs_relogin;
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
    // Refresh non-Claude provider accounts (state v2 `providers[slug]`) too.
    // Claude lives in `state.accounts` and is handled above; every other
    // captured provider (Codex, …) needs its own usage fetch or it stays
    // "no data yet" forever — captured and shown in the menu/list but inert.
    refresh_provider_usage_caches();
    logging::log("poll: done");
    RefreshOutcome { rate_limited }
}

/// Map a provider [`UsageSnapshot`] into the stored [`CachedUsage`] shape the
/// menu/list render from: the first window (`primary`) is the session window,
/// the second (`secondary`) is the weekly one — matching every provider's
/// `window_order` and how `row_from_provider_account` reads the cache.
fn cached_from_usage_snapshot(snap: &UsageSnapshot) -> CachedUsage {
    let mut cu = CachedUsage {
        session_pct: None,
        weekly_pct: None,
        session_reset: None,
        weekly_reset: None,
        opus_pct: None,
        opus_reset: None,
        fetched_at: snap.fetched_at.timestamp(),
    };
    for w in &snap.windows {
        let reset = w.resets_at.map(|dt| dt.to_rfc3339());
        match w.id.as_str() {
            "primary" | "session" => {
                cu.session_pct = w.utilization;
                cu.session_reset = reset;
            }
            "secondary" | "weekly" => {
                cu.weekly_pct = w.utilization;
                cu.weekly_reset = reset;
            }
            _ => {}
        }
    }
    cu
}

/// Fetch usage for every captured non-Claude provider account and persist it
/// into `state.providers[slug]`. The counterpart to the Claude loop in
/// `refresh_usage_cache` — without it, a captured Codex/etc. account renders
/// "no data yet" forever because `Provider::fetch_usage` is otherwise never
/// called. Best-effort throughout: a token-refresh or usage error keeps the
/// existing cache and moves on (never panics the poll loop). Applies the same
/// locked-until-reset skip so a maxed provider account doesn't burn requests.
fn refresh_provider_usage_caches() {
    let state = match State::load() {
        Ok(s) => s,
        Err(_) => return,
    };
    for provider in providers::all() {
        let slug = provider.provider_id();
        if slug == CLAUDE_SLUG {
            continue;
        }
        let Some(pa) = state.providers.get(slug) else {
            continue;
        };
        let keys: Vec<String> = pa.accounts.iter().map(|a| a.key.clone()).collect();
        for key in keys {
            let Some(acct) = state.find_provider_account(slug, &key).cloned() else {
                continue;
            };
            if acct.needs_relogin {
                continue;
            }
            // Locked accounts don't change until reset — skip (same guard as
            // the Claude path; the reset-boundary wake brings us back).
            if let Some(cu) = &acct.cached_usage {
                if countdown::is_locked_until_reset(&cached_to_account_usage(cu), Utc::now()) {
                    continue;
                }
            }
            // Refresh the token if it's at/near expiry and we have a refresh
            // token; on failure keep the cache and try again next cycle.
            let mut access = acct.access_token.clone();
            let mut refreshed: Option<(String, String, i64)> = None;
            if acct.expires_at <= Utc::now().timestamp() + REFRESH_SKEW_SECS
                && !acct.refresh_token.is_empty()
            {
                match provider.refresh_token(&acct.refresh_token) {
                    Ok(grant) => {
                        access = grant.access.clone();
                        let expires_at = Utc::now().timestamp() + grant.expires_in_secs;
                        let refresh = grant.refresh.unwrap_or_else(|| acct.refresh_token.clone());
                        refreshed = Some((grant.access, refresh, expires_at));
                    }
                    Err(e) => {
                        logging::log(&format!(
                            "provider refresh failed for {slug}/{key}: {e} (keeping cache)"
                        ));
                        continue;
                    }
                }
            }
            let cu = match provider.fetch_usage(&access) {
                Ok(snap) => Some(cached_from_usage_snapshot(&snap)),
                Err(e) => {
                    logging::log(&format!(
                        "provider usage error for {slug}/{key}: {e}; keeping cache"
                    ));
                    None
                }
            };
            // Persist tokens + usage under the lock with a fresh reload so a
            // concurrent capture/switch isn't clobbered.
            let _ = with_state_lock(|| {
                let mut st = State::load()?;
                if let Some(a) = st.find_provider_account_mut(slug, &key) {
                    if let Some((at, rt, exp)) = &refreshed {
                        a.access_token = at.clone();
                        a.refresh_token = rt.clone();
                        a.expires_at = *exp;
                    }
                    if let Some(cu) = &cu {
                        a.cached_usage = Some(cu.clone());
                    }
                }
                st.save()?;
                Ok(())
            });
            if cu.is_some() {
                logging::log(&format!(
                    "event=provider_usage_refreshed provider={slug} account={key}"
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// watch — auto-swap daemon
// ---------------------------------------------------------------------------

/// Fold a `Row`'s cached usage into `countdown::AccountUsage` so
/// `countdown::any_reset_within` can reason about its reset instants — used
/// by `cap_sleep_to_reset_boundary` (v0.5.2 item 4).
fn row_to_account_usage(r: &Row) -> countdown::AccountUsage {
    countdown::AccountUsage {
        session_pct: r.session.pct,
        session_reset: r.session.resets_at,
        weekly_pct: r.weekly.pct,
        weekly_reset: r.weekly.resets_at,
        fetched_at: r.fetched_at.and_then(|t| DateTime::from_timestamp(t, 0)),
    }
}

/// Parse a cached-usage snapshot into the `countdown::AccountUsage` the lock
/// predicates operate on (RFC3339 reset strings → `DateTime<Utc>`, epoch
/// seconds → `DateTime`). Used by `refresh_usage_cache` to decide, per
/// account, whether the account is locked-until-reset and can skip its network
/// refresh this cycle.
fn cached_to_account_usage(cu: &CachedUsage) -> countdown::AccountUsage {
    let parse = |s: &Option<String>| {
        s.as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
    };
    countdown::AccountUsage {
        session_pct: cu.session_pct,
        session_reset: parse(&cu.session_reset),
        weekly_pct: cu.weekly_pct,
        weekly_reset: parse(&cu.weekly_reset),
        fetched_at: DateTime::from_timestamp(cu.fetched_at, 0),
    }
}

/// Horizon (seconds) within which an imminent reset is worth waking early
/// for, and the buffer added past the reset instant so the wake lands just
/// AFTER it (not exactly on it, where clock skew could still read stale).
const RESET_WAKE_HORIZON_SECS: i64 = 30;
const RESET_WAKE_BUFFER_SECS: i64 = 2;

/// v0.5.2 item 4: if any Claude account has a session/weekly reset within
/// `RESET_WAKE_HORIZON_SECS` of `now`, cap `planned` so the loop wakes right
/// after it instead of riding out the whole adaptive-cadence interval — a
/// stale locked/100% reading would otherwise survive up to a full cadence
/// window past its own reset, and the very next loop iteration runs a FULL
/// `watch_cycle` (refresh + swap re-evaluation), not just a display patch.
/// Falls back to `planned` unchanged when nothing is imminent. Pure over
/// already-loaded rows + `now` so it's unit-testable without a live clock.
fn cap_sleep_to_reset_boundary(rows: &[Row], now: DateTime<Utc>, planned: u64) -> u64 {
    let soonest = rows
        .iter()
        .filter_map(|r| {
            countdown::any_reset_within(
                &row_to_account_usage(r),
                now,
                Duration::seconds(RESET_WAKE_HORIZON_SECS),
            )
        })
        .min();
    match soonest {
        Some(reset_at) => {
            let secs = ((reset_at - now).num_seconds() + RESET_WAKE_BUFFER_SECS).max(1) as u64;
            secs.min(planned)
        }
        None => planned,
    }
}

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

    eprintln!("usagio watch: every {interval}s, swap at {trigger:.0}%, target <= {ceiling:.0}%");

    let base = interval;
    let mut current = base;
    let mut guard = SwapGuard::default();
    let mut wd = watchdog::Watchdog::default();
    let mut wd_effects = watchdog::RealEffects;
    loop {
        // Self-healing health check (fd-count + keychain-write). Throttled to
        // ~once a minute internally; a cheap directory read otherwise. Keeps
        // account switching from silently breaking if fds ever leak or the
        // keychain starts failing again (see `src/watchdog.rs`).
        wd.maybe_run(&mut wd_effects);
        match watch_cycle(trigger, ceiling, &mut guard) {
            Ok(outcome) => {
                if let Some((from, to)) = outcome.swapped {
                    eprintln!("[{}] swapped {from} -> {to}", Utc::now().to_rfc3339());
                }
                let prev = current;
                current = next_interval(
                    current,
                    base,
                    outcome.rate_limited,
                    outcome.max_pct,
                    trigger,
                    outcome.actionable,
                );
                if outcome.rate_limited {
                    logging::log(&format!("rate limited; backing off to {current}s"));
                } else if current != prev && current < base {
                    // Log cadence tightening so a user chasing a missed swap can
                    // see the daemon was polling faster on approach.
                    let max_pct = outcome.max_pct.unwrap_or(0.0);
                    logging::log(&format!(
                        "cadence: {prev}s → {current}s (max {max_pct:.1}%, \
                         trigger {trigger:.0}%) event=cadence prev={prev}s new={current}s \
                         max_pct={max_pct:.1} trigger={trigger:.0}"
                    ));
                }
            }
            Err(e) => eprintln!("watch cycle error: {e:#}"),
        }
        // v0.5.2 item 4: if a Claude account's session/weekly reset falls
        // within the next `current` seconds, wake right after it (+2s
        // buffer) instead of riding out the whole cadence window — the next
        // loop iteration's `watch_cycle` does a full refresh + swap
        // re-evaluation, not just a display patch, so a stale locked/100%
        // reading can't survive past its own reset for up to a full cadence
        // interval. Best-effort: falls back to `current` unchanged on any
        // load failure or when nothing is imminent.
        let wake_rows: Vec<Row> = State::load()
            .map(|s| s.accounts.iter().map(row_from_account).collect())
            .unwrap_or_default();
        let sleep_secs = cap_sleep_to_reset_boundary(&wake_rows, Utc::now(), current);
        std::thread::sleep(std::time::Duration::from_secs(sleep_secs));
    }
}

/// The ACTIVE account's `Row::max_pct()` (session OR weekly, whichever is
/// tighter), or `None` if there's no active account or it has no data yet —
/// feeds `next_interval`'s adaptive cadence.
///
/// v0.5.13: scoped from "peak across ALL accounts" (`peak_max_pct`) to the
/// active account alone. The cadence backstop exists to catch the moment an
/// account crosses the trigger so the auto-swap fires promptly — but the only
/// account you ever swap AWAY from is the active one, and it's the only one
/// that climbs on its own (inactive accounts aren't being spent). An inactive
/// account already pinned at 100% (e.g. a weekly limit that won't reset for
/// days) is not actionable: you never swap TO an account that's out of room.
/// Folding it into the peak pinned the poll cadence at the 30s BACKSTOP for
/// days at a stretch, which hammered the usage endpoint into HTTP 429s for no
/// benefit. Scoping to the active account preserves the original
/// user-reported miss-fix (active at 94% must tighten so it doesn't lock
/// before the next poll — see `next_interval`) while dropping the waste.
///
/// Still weekly-aware: `Row::max_pct()` folds `max(session%, weekly%)`, so an
/// active account at session=0%/weekly=99% correctly yields `Some(99.0)` and
/// thus BACKSTOP cadence.
fn cadence_max_pct(rows: &[Row], active: Option<&str>) -> Option<f64> {
    let active = active?;
    rows.iter()
        .find(|r| r.email == active && r.has_data())
        .map(Row::max_pct)
}

/// Threshold band widths for `next_interval`'s adaptive cadence.
///
/// Rationale for these values, from a real user-reported prod miss on
/// v0.4.3: fixed 150s cadence caught an account at 94%, waited the full
/// 150s to the next poll, and the account was at 99-100% by then — lock,
/// swap missed. Below the warning band we stay at the full base cadence
/// (cheap, ordinary case); inside the warning band we tighten to WARNING
/// so we can't miss more than ~30s of runway; above the trigger threshold
/// (the auto-swap should already have fired, but if it hasn't for any
/// reason — network flap, keychain unlocked mid-cycle — the backstop
/// makes sure the next attempt is 30s away, not 150s).
///
/// 30s is the floor for BOTH tiers, deliberately: the usage endpoint is
/// polled once per account per cycle, so with several near-maxed accounts a
/// tighter floor multiplies into a request rate that Anthropic rate-limits
/// (429). 30s catches a reset/swap-window within half a minute while keeping
/// the aggregate request rate well under the limit even with many accounts.
const WATCH_WARNING_BAND: f64 = 15.0;
const WATCH_WARNING_INTERVAL_SECS: u64 = 30;
const WATCH_BACKSTOP_INTERVAL_SECS: u64 = 30;

/// Compute the next poll interval. Priority order:
///   1. Rate-limited from Anthropic → exponential backoff (doubling,
///      capped at WATCH_MAX_INTERVAL_SECS). Overrides everything below.
///   2. Active account at or above the trigger threshold AND a swap is
///      actionable (an eligible target exists, even if currently
///      cooldown-blocked) → BACKSTOP (10s). The auto-swap should already
///      have fired; this makes sure a transient failure or a soon-clearing
///      cooldown doesn't leave us blind for a full base cycle.
///   3. Active account at/above trigger but NOT actionable (no eligible
///      target at all — a single-account user, or every other account is
///      full / needs_relogin / env-overridden) → BASE. There is nothing to
///      catch, so polling every 10s for up to a week only risks HTTP 429;
///      the reset-boundary cap still wakes us near the reset.
///   4. Active account inside the warning band (trigger - 15% ≤ pct <
///      trigger) → WARNING (30s), regardless of `actionable` — we tighten to
///      catch the *crossing*, at which point a target may become relevant.
///   5. Comfortably below → BASE (default WATCH_INTERVAL_SECS = 150s).
///
/// `max_pct` is the ACTIVE account's `Row::max_pct()` — i.e.
/// `max(session%, weekly%)` for the account currently logged in (v0.5.13,
/// scoped down from the cross-account peak; see `cadence_max_pct`). It's
/// weekly-aware: a weekly window pinned at 99% with a healthy session is just
/// as "about to lock" as the reverse. `actionable` (v0.5.17) gates the
/// backstop so a maxed account with nowhere to swap doesn't over-poll.
fn next_interval(
    current: u64,
    base: u64,
    rate_limited: bool,
    max_pct: Option<f64>,
    trigger: f64,
    actionable: bool,
) -> u64 {
    if rate_limited {
        return (current.max(base) * 2).min(WATCH_MAX_INTERVAL_SECS);
    }
    let Some(pct) = max_pct else {
        return base;
    };
    if pct >= trigger {
        if actionable {
            WATCH_BACKSTOP_INTERVAL_SECS
        } else {
            base
        }
    } else if pct >= trigger - WATCH_WARNING_BAND {
        WATCH_WARNING_INTERVAL_SECS
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

/// Result of one poll: the swap it made (if any), the rate-limited flag, and
/// the ACTIVE account's `Row::max_pct()` (session OR weekly, whichever is
/// tighter) this cycle (used by `next_interval` to tighten the poll cadence
/// on approach to the trigger threshold — see the user-reported miss where
/// 94% + 150s wait produced a lock before the next poll). v0.5.13: scoped to
/// the active account (see `cadence_max_pct`) so an inactive, already-maxed
/// account can't pin the cadence at the 30s backstop indefinitely.
pub(crate) struct CycleOutcome {
    pub swapped: Option<(String, String)>,
    pub rate_limited: bool,
    pub max_pct: Option<f64>,
    /// Whether a swap is actionable this cycle: an eligible target exists for
    /// the active account, even if a cooldown currently blocks it. Feeds
    /// `next_interval` so the 30s backstop only engages when there is actually
    /// something to catch — a maxed active account with no eligible target
    /// (single-account user, or all others full/needs_relogin) falls back to
    /// BASE instead of hammering the usage endpoint into HTTP 429 (v0.5.17).
    pub actionable: bool,
}

/// True if `cand` is a strictly better place to be than the healthy `act`:
/// a sooner weekly reset (use-it-or-lose-it), or — on an equal reset — a
/// meaningful headroom lead. Used only on the proactive (active-not-in-trouble)
/// path; the reactive path moves regardless of how the active account ranks.
fn worth_returning_to(cand: &Row, act: &Row) -> bool {
    // If the ACTIVE account's weekly reset is unknown, there is no
    // use-it-or-lose-it basis to leave a healthy active account. Require a real
    // headroom lead instead of the old behavior, which defaulted the unknown
    // reset to MAX_UTC and so ranked ANY candidate with a known reset as
    // "sooner" — forcing a needless proactive swap off a working account
    // (v0.5.17). Only the active-unknown case changes; every act-known path
    // (including a candidate whose own reset is unknown → treated as furthest)
    // keeps its prior behavior.
    let Some(ka) = act.weekly.resets_at else {
        return cand.headroom() - act.headroom() >= PROACTIVE_HEADROOM_MARGIN;
    };
    let kc = cand.weekly.resets_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
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
///
/// `watch_cycle` itself calls `evaluate_swap` directly (v0.5.2 item 6 needs
/// the extra cooldown/no-eligible-target detail for its structured decision
/// logging); this thin `.target`-only wrapper survives purely because the
/// existing test suite calls it by this name — `#[cfg(test)]` rather than
/// deleting it keeps that coverage without carrying dead code into release
/// builds.
#[cfg(test)]
fn choose_swap_target(
    rows: &[Row],
    active: &str,
    trigger: f64,
    ceiling: f64,
    guard: &SwapGuard,
) -> Option<String> {
    evaluate_swap(rows, active, trigger, ceiling, guard).target
}

/// Extended swap-target inspection shared by `choose_swap_target` (the actual
/// decision consulted for swapping) and `watch_cycle`'s structured
/// `event=swap_decision` logging (v0.5.2 item 6) — the two must agree on what
/// "would swap if not for cooldown" means, so a "blocked" log line names the
/// SAME target `choose_swap_target` would have picked once the cooldown
/// clears.
struct SwapEval {
    /// The account to switch to right now, if any (cooldown/no-return/
    /// eligibility have all already been consulted).
    target: Option<String>,
    /// True if an active cooldown window is the ONLY reason `target` is
    /// `None` — i.e. ignoring cooldown, there IS an eligible + genuinely
    /// better candidate (`target_ignoring_cooldown`).
    blocked_by_cooldown: bool,
    /// The best eligible candidate ignoring the cooldown gate, whether or not
    /// cooldown ends up blocking it. `None` when there's no eligible
    /// candidate at all (or the active account isn't in trouble and no
    /// candidate is `worth_returning_to` it).
    target_ignoring_cooldown: Option<String>,
}

fn evaluate_swap(
    rows: &[Row],
    active: &str,
    trigger: f64,
    ceiling: f64,
    guard: &SwapGuard,
) -> SwapEval {
    let none = SwapEval {
        target: None,
        blocked_by_cooldown: false,
        target_ignoring_cooldown: None,
    };
    let Some(act) = rows.iter().find(|r| r.email == active) else {
        return none;
    };
    if !act.has_data() {
        return none;
    }
    // If the active account's provider has its env-override active, the CLI
    // ignores whatever token we install into the keychain — swapping is a
    // no-op. Bail before we churn the state file.
    if env_override_active(&act.provider_id) {
        return none;
    }
    let mut candidates: Vec<&Row> = rows
        .iter()
        // A row is only a valid swap candidate if its owning provider has both
        // a usage signal (so we can compare its cached utilization to the
        // ceiling) and a way to actually switch to it. This is the sole
        // capability gate for auto-swap: reporting-only or stub providers
        // never surface here, even if their rows carry a stale utilization.
        .filter(|r| provider_supports_swap(&r.provider_id))
        // Skip candidates the token endpoint has permanently rejected —
        // swapping to a needs-relogin account would just re-write the same
        // dead credentials into the keychain.
        .filter(|r| !r.needs_relogin)
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
        return none;
    }
    candidates.sort_by(|a, b| candidate_order(a, b));
    let best = candidates[0];
    // Active in trouble → move to the best candidate. Active still healthy →
    // only move if the best candidate is genuinely a better place to be.
    if !(act.max_pct() >= trigger || worth_returning_to(best, act)) {
        return none;
    }
    let cooldown_active = guard
        .last_swap
        .map(|t| t.elapsed().as_secs() < SWAP_COOLDOWN_SECS)
        .unwrap_or(false);
    if cooldown_active {
        SwapEval {
            target: None,
            blocked_by_cooldown: true,
            target_ignoring_cooldown: Some(best.email.clone()),
        }
    } else {
        SwapEval {
            target: Some(best.email.clone()),
            blocked_by_cooldown: false,
            target_ignoring_cooldown: Some(best.email.clone()),
        }
    }
}

/// Poll usage for every account (the only network path), record history, and
/// auto-swap away from the active account if it has reached `trigger` and a
/// healthy target exists. Shared by `usagio watch` and the menu-bar poller.
fn watch_cycle(trigger: f64, ceiling: f64, guard: &mut SwapGuard) -> Result<CycleOutcome> {
    // Pre-cycle: absorb any on-disk credential rotations the vendor CLI made
    // behind our back (fsnotify may not fire on remote/network volumes, and
    // we want the invariant to hold even on the polling path). Then refresh
    // INACTIVE Claude accounts eagerly so a user who switches to one later
    // doesn't have to wait for the reactive path to notice.
    for p in providers::all() {
        let _ = credentials::absorb_all_lagging(p.as_ref());
    }
    credentials::refresh_inactive_if_stale(State::load().ok().and_then(|s| s.active).as_deref());

    let refresh = refresh_usage_cache();
    // Gap-4 (codex-switch-e2e): drive the active-account CAS refresh for
    // every non-Claude provider that has one wired (currently just codex).
    // Best-effort and self-contained — logs and moves on rather than
    // affecting `refresh`'s rate-limit signal, which only tracks the usage
    // endpoint above.
    for p in providers::all() {
        if p.provider_id() != CLAUDE_SLUG {
            refresh_provider_active_account(p.provider_id());
        }
    }
    let state = State::load()?;
    if state.accounts.is_empty() {
        return Ok(CycleOutcome {
            swapped: None,
            rate_limited: refresh.rate_limited,
            max_pct: None,
            actionable: false,
        });
    }
    let rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
    let max_pct = cadence_max_pct(&rows, state.active.as_deref());
    append_history(&rows, state.active.as_deref());

    let active = state.active.clone();
    let mut swapped = None;
    // Whether a swap is actionable this cycle (an eligible target exists,
    // ignoring cooldown). Captured out here because the final CycleOutcome is
    // built after the `if let Some(active_email)` block closes and `eval` is
    // out of scope. Drives the cadence backstop gate — see next_interval.
    let mut actionable = false;

    if let Some(active_email) = active.clone() {
        let eval = evaluate_swap(&rows, &active_email, trigger, ceiling, guard);
        // A target that's merely cooldown-blocked still counts as actionable:
        // we want the fast backstop so we swap the instant the cooldown clears.
        actionable = eval.target_ignoring_cooldown.is_some();
        // v0.5.2 item 6: structured decision logging (+ a "stayed" notification)
        // fires every cycle the active account is at/above trigger, regardless
        // of what happens next — a swap, a cooldown-blocked would-be swap, or
        // genuinely nothing eligible.
        let act_row = rows.iter().find(|r| r.email == active_email);
        let act_over = act_row
            .map(|r| r.has_data() && r.max_pct() >= trigger)
            .unwrap_or(false);
        let active_pct = act_row.map(|r| r.max_pct()).unwrap_or(0.0);

        match eval.target {
            Some(target) => {
                let target_row = rows
                    .iter()
                    .find(|r| r.email == target)
                    .expect("evaluate_swap returned an email absent from `rows`");
                let (pick_s, pick_w) = (
                    target_row.session.pct.unwrap_or(0.0),
                    target_row.weekly.pct.unwrap_or(0.0),
                );
                // Proactive flip-back if the account we're leaving wasn't itself
                // over the trigger — a better account simply freed up.
                let proactive = !act_over;
                if act_over {
                    logging::log(&format!(
                        "event=swap_decision active={active_email} active_pct={active_pct:.0}% \
                         target={target} target_pct={:.0}% action=switching",
                        target_row.max_pct()
                    ));
                }
                // Resolve the target row's provider. `evaluate_swap` already
                // filtered on `provider_supports_swap`, so this must succeed
                // for any row it returned; a mismatch is a bug.
                let target_provider = match provider_by_slug(&target_row.provider_id) {
                    Ok(p) => p,
                    Err(e) => {
                        logging::log(&format!(
                            "swap to {target} skipped: provider unavailable: {e:#}"
                        ));
                        return Ok(CycleOutcome {
                            swapped: None,
                            rate_limited: refresh.rate_limited,
                            max_pct,
                            // A target existed (we're in the eval.target Some
                            // arm); the swap failed transiently — keep the fast
                            // backstop so the retry is prompt.
                            actionable: true,
                        });
                    }
                };
                // Compare-and-set on the active account: if a manual switch landed
                // since evaluate_swap read the snapshot, skip this swap.
                let Some(label) =
                    switch_to_if_still_active(target_provider, &target, &active_email)?
                else {
                    return Ok(CycleOutcome {
                        swapped: None,
                        rate_limited: refresh.rate_limited,
                        max_pct,
                        // A target existed but a concurrent switch won the CAS;
                        // still actionable — retry promptly.
                        actionable: true,
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
                if act_over {
                    if eval.blocked_by_cooldown {
                        if let Some(target) = &eval.target_ignoring_cooldown {
                            logging::log(&format!(
                                "event=swap_decision active={active_email} \
                                 active_pct={active_pct:.0}% target={target} action=blocked \
                                 reason=cooldown"
                            ));
                        }
                    } else {
                        logging::log(&format!(
                            "event=swap_decision active={active_email} action=stayed \
                             reason=no_eligible_target"
                        ));
                        if !guard.stuck_notified {
                            // Pick the soonest reset by TIMESTAMP, then humanize —
                            // taking min() of humanized strings sorts
                            // lexicographically ("3d 12h" < "3d 9h"), which is not
                            // chronological.
                            let soonest = rows
                                .iter()
                                .filter(|r| r.has_data())
                                .filter_map(|r| r.weekly.resets_at)
                                .min()
                                .map(humanize_until)
                                .unwrap_or_else(|| "unknown".to_string());
                            notify(&format!(
                                "staying on {active_email} — no better target available \
                                 (soonest reset in {soonest})"
                            ));
                            guard.stuck_notified = true;
                        }
                    }
                } else {
                    guard.stuck_notified = false;
                }
            }
        }
    }

    Ok(CycleOutcome {
        swapped,
        rate_limited: refresh.rate_limited,
        max_pct,
        actionable,
    })
}

/// Fire a native "usagio: {msg}" notification (best effort, cross-platform).
/// Delegates to `notifications::fire_plain`, which routes through notify-rust
/// so macOS/Linux/Windows all get a real notification — the previous osascript
/// path silently no-op'd on Linux and Windows. Best-effort: failures are
/// logged (R2-EH-04) so H6's user-facing notification guarantee has
/// diagnostic backing when the channel is unavailable (headless SSH, no
/// notification daemon, denied permission, etc.).
fn notify(msg: &str) {
    if let Err(e) = notifications::fire_plain(msg) {
        logging::log(&format!("notify: fire_plain failed: {e}"));
    }
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

// ---------------------------------------------------------------------------
// context — Context Ledger
// ---------------------------------------------------------------------------

/// `usagio context [--provider SLUG] [--project PATH]` — audit what a
/// CLI auto-injects into the model context per turn, with token cost per item.
/// Delegates parsing (positional-agnostic) to a pure helper so the arg parsing
/// stays unit-testable without a subprocess.
fn cmd_context(args: &[String]) -> Result<()> {
    let (provider, project) = parse_context_args(args)?;
    context_ledger::cli::run(provider, project)
}

/// Parse the `context` subcommand's two supported flags. Unknown flags are
/// rejected so a typo doesn't silently degrade to "audit every provider".
fn parse_context_args(args: &[String]) -> Result<(Option<String>, Option<std::path::PathBuf>)> {
    let mut provider: Option<String> = None;
    let mut project: Option<std::path::PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--provider" => {
                provider = Some(it.next().cloned().context("--provider requires a value")?);
            }
            "--project" => {
                project = Some(std::path::PathBuf::from(
                    it.next().context("--project requires a value")?,
                ));
            }
            other => bail!("unknown context option: {other}"),
        }
    }
    Ok((provider, project))
}

fn cmd_report(args: &[String]) -> Result<()> {
    // Analytics subflavours share the same command surface so `report` stays
    // the one-stop CLI for usage insight. `--pace` prints per-account burn-
    // rate forecasts; `--pricing` dumps the model→price lookup table;
    // `--verdict` classifies each captured account as cancel/downgrade/keep/
    // upgrade based on the last few weekly cycles. With no flag we fall
    // through to the classic weekday/hour histogram.
    if args.iter().any(|a| a == "--pace") {
        return cmd_report_pace();
    }
    if args.iter().any(|a| a == "--pricing") {
        return cmd_report_pricing();
    }
    if args.iter().any(|a| a == "--verdict") {
        return cmd_report_verdict();
    }

    use chrono::{Datelike, Local, TimeZone, Timelike};

    let path = history_path()?;
    let data = std::fs::read_to_string(&path)
        .context("no history yet — run `usagio watch` (or `install`) to collect it")?;
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

// --- report --pace / --pricing / --verdict --------------------------------
//
// The three flag paths reuse the burn_rate / pricing / cost_tracking modules,
// so the CLI stays a thin renderer over the same data the menu bar consumes.
// Every flag prints the "estimate, not billing" disclaimer once so users read
// the output with appropriate skepticism.

fn cmd_report_pace() -> Result<()> {
    use crate::providers::trait_def::Window;
    use crate::usage_log::AccountKey;
    let state = State::load()?;
    if state.accounts.is_empty() {
        println!("No accounts captured yet — run `usagio capture` first.");
        return Ok(());
    }
    println!("\nBurn-rate forecast");
    println!("  {}\n", crate::cost_tracking::DISCLAIMER);
    let now = Utc::now();
    for acct in &state.accounts {
        let Some(email) = acct.email.clone() else {
            continue;
        };
        let key = AccountKey::new(CLAUDE_SLUG, &email);
        println!("  {email}");
        for window in [Window::Session, Window::Weekly] {
            if let Some(est) = crate::burn_rate::estimate(&key, window, now) {
                println!("    {}", crate::burn_rate::format_menu_row(&est));
            }
        }
    }
    println!();
    Ok(())
}

fn cmd_report_pricing() -> Result<()> {
    // Static table dump — useful for verifying which model → dollar rate the
    // cost estimator will use before it renders in the menu.
    println!("\nModel pricing (USD per 1M tokens)");
    println!("  Source: vendor pricing pages, compiled 2026-09-06.\n");
    // We enumerate provider-slug guesses; unknown lookups are silently skipped.
    let probes: &[(&str, &[&str])] = &[
        (
            "claude",
            &[
                "claude-fable-5-1",
                "claude-opus-5",
                "claude-opus-4-1",
                "claude-sonnet-5",
                "claude-sonnet-4-5",
                "claude-haiku-4-5",
            ],
        ),
        (
            "codex",
            &[
                "gpt-6-astra",
                "gpt-5-6-sol",
                "gpt-5-6-luna",
                "gpt-5",
                "gpt-4-1",
                "o3",
            ],
        ),
        (
            "gemini-cli",
            &[
                "gemini-2-5-pro",
                "gemini-2-5-flash",
                "gemini-2-5-flash-lite",
            ],
        ),
        (
            "deepseek",
            &["deepseek-v4", "deepseek-chat", "deepseek-reasoner"],
        ),
        (
            "qwen-code",
            &["qwen-max", "qwen-plus", "qwen-turbo", "qwen-coder-3"],
        ),
        ("zai", &["glm-4-5", "glm-4-air", "glm-4-plus"]),
        (
            "fireworks",
            &["llama-3-3-70b", "llama-4-scout", "llama-4-maverick"],
        ),
    ];
    for (provider, models) in probes {
        println!("  {provider}");
        for m in *models {
            if let Some(p) = crate::pricing::lookup(provider, m) {
                println!(
                    "    {m:<24} in ${:>5.2} / out ${:>6.2}",
                    p.input_per_million, p.output_per_million
                );
            }
        }
        println!();
    }
    println!("  Passthrough providers (billed per-request against the vendor's API):");
    for p in ["openrouter", "synthetic"] {
        if crate::pricing::is_passthrough(p) {
            println!("    {p} — cost pulled live from the underlying model");
        } else {
            // synthetic is not tagged as passthrough today; still enumerate so
            // the user sees why it has no local rate.
            println!("    {p} — no local rate table");
        }
    }
    println!();
    Ok(())
}

fn cmd_report_verdict() -> Result<()> {
    use crate::cost_tracking::{subscription_verdict, Verdict};
    use crate::usage_log::AccountKey;
    let state = State::load()?;
    if state.accounts.is_empty() {
        println!("No accounts captured yet — run `usagio capture` first.");
        return Ok(());
    }
    println!("\nSubscription verdict");
    println!("  {}\n", crate::cost_tracking::DISCLAIMER);
    // Four weekly cycles ≈ one month, the window we quote as "recoverable per
    // month" downstream.
    const CYCLES: usize = 4;
    for acct in &state.accounts {
        let Some(email) = acct.email.clone() else {
            continue;
        };
        let key = AccountKey::new(CLAUDE_SLUG, &email);
        match subscription_verdict(&key, CYCLES) {
            None => println!("  {email}: not enough history yet"),
            Some(sv) => {
                let label = match sv.verdict {
                    Verdict::Cancel => "cancel",
                    Verdict::Downgrade => "downgrade",
                    Verdict::Keep => "keep",
                    Verdict::Upgrade => "upgrade",
                };
                println!(
                    "  {email}: {label}  (avg {:.0}%, peak {:.0}%, ~${:.2} recoverable/mo)",
                    sv.avg_utilization_pct, sv.peak_utilization_pct, sv.recoverable_dollars,
                );
            }
        }
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
// Autostart install / uninstall — delegate to the platform Autostart backend
// ---------------------------------------------------------------------------

/// Path to invoke for the login item and for a post-upgrade relaunch, chosen to
/// survive `brew upgrade`. `current_exe()` resolves symlinks to the versioned
/// Homebrew Cellar path, which an upgrade deletes; map that back to the stable
/// `<prefix>/bin/usagio` symlink brew keeps repointing. For a from-source
/// install the resolved path is already stable.
pub(crate) fn stable_exe_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let s = exe.to_string_lossy();
    if let Some(idx) = s.find("/Cellar/usagio/") {
        let stable = std::path::PathBuf::from(format!("{}/bin/usagio", &s[..idx]));
        if stable.exists() {
            return stable;
        }
    }
    exe
}

/// Path to hand `launchctl` for the LaunchAgent's `ProgramArguments`, chosen
/// so Login Items shows the usagio app icon (see `packaging/macos/`) instead
/// of the generic "exec" glyph a bare binary gets. Resolution order:
///
/// 1. If we're already executing from inside a bundle — `current_exe()`
///    contains `.app/Contents/MacOS/` — use that path as-is. This is the
///    case once `usagio install` itself is invoked via the bundled
///    executable (e.g. a future launcher), and it's already the
///    Cellar-versioned bundle path brew keeps stable per version.
/// 2. Otherwise, probe for a sibling `usagio.app` next to the resolved
///    stable binary: a Homebrew install lays out
///    `<Cellar>/usagio/<version>/bin/usagio` alongside
///    `<Cellar>/usagio/<version>/usagio.app/Contents/MacOS/usagio`, so from
///    the binary's `bin/` directory, `usagio.app` is one level up.
/// 3. Fall back to the bare [`stable_exe_path`] if no bundle is found (a
///    from-source install, or a brew formula version predating the bundle).
pub(crate) fn launch_agent_exe_path() -> std::path::PathBuf {
    let current = std::env::current_exe().unwrap_or_default();
    if current.to_string_lossy().contains(".app/Contents/MacOS/") {
        return current;
    }
    let stable = stable_exe_path();
    sibling_app_bundle_exe(&stable).unwrap_or(stable)
}

/// Given a path to `.../bin/usagio` — typically the stable Homebrew symlink
/// `<prefix>/bin/usagio` — resolve a **version-stable** path to the sibling
/// `usagio.app/Contents/MacOS/usagio` that survives `brew upgrade`.
///
/// Homebrew keeps two stable roots per formula: `<prefix>/bin/<formula>`
/// (symlink to the current Cellar bin), and `<prefix>/opt/<formula>/`
/// (symlink to the current Cellar root — the whole version dir). Both point
/// at the just-installed version and are updated atomically on `brew upgrade`.
/// We MUST use one of those, not the canonicalized `<prefix>/Cellar/usagio/<version>/...`
/// path, because Homebrew deletes the old version's Cellar directory on
/// upgrade — pinning the LaunchAgent to the versioned path would silently
/// break autostart on every user's next `brew upgrade`.
///
/// Resolution order:
///   1. Given `<prefix>/bin/usagio`, derive `<prefix>` (bin's parent) and
///      probe `<prefix>/opt/usagio/usagio.app/Contents/MacOS/usagio`. Homebrew
///      creates that symlink whenever a formula ships a bundle. This is the
///      preferred path — brew keeps it valid across upgrades.
///   2. Given a non-brew layout (from-source install, custom deployment),
///      look for `usagio.app` as a sibling of the binary's version dir
///      (`../usagio.app/Contents/MacOS/usagio` from `bin/`). Not upgrade-
///      stable for brew but doesn't apply to brew installs.
///   3. Otherwise, `None` — callers fall back to the bare `stable_exe_path()`.
fn sibling_app_bundle_exe(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    // (1) Homebrew stable opt-path — the correct answer for a brew install.
    if let Some(bin_dir) = exe.parent() {
        if let Some(prefix) = bin_dir.parent() {
            let opt_bundle = prefix
                .join("opt")
                .join("usagio")
                .join("usagio.app")
                .join("Contents")
                .join("MacOS")
                .join("usagio");
            if opt_bundle.exists() {
                return Some(opt_bundle);
            }
        }
    }
    // (2) Non-brew: sibling in the version dir. Match on canonicalized form
    // too so a symlinked bin/usagio still finds a real sibling app.
    let mut candidates = vec![exe.to_path_buf()];
    if let Ok(resolved) = std::fs::canonicalize(exe) {
        candidates.push(resolved);
    }
    for path in candidates {
        let bin_dir = path.parent()?; // .../<version>/bin
        let version_dir = bin_dir.parent()?; // .../<version>
        let bundle_exe = version_dir
            .join("usagio.app")
            .join("Contents")
            .join("MacOS")
            .join("usagio");
        if bundle_exe.exists() {
            return Some(bundle_exe);
        }
    }
    None
}

fn cmd_install() -> Result<()> {
    // One-shot launchd migration: unload + remove the pre-v0.4.0 plist so
    // an upgrading user doesn't end up with two agents fighting for the
    // menu bar. Best-effort — a failed unload (e.g. label not loaded)
    // is fine; only the file-remove failure is worth surfacing.
    migrate_launchd_if_needed();
    // Also purge any legacy System Events login item (contract-01, v0.5.2
    // codeaudit). `MacOsAutostart::uninstall` already does this, but a
    // `brew upgrade` user who installed under a pre-v0.4.3 build and never
    // ran `usagio uninstall` keeps that stale entry forever — `install`
    // itself must clean it up too, since it's the path every upgrade
    // actually runs through.
    purge_legacy_login_items();

    let exe = launch_agent_exe_path();
    platform()
        .autostart()
        .install(AUTOSTART_LABEL, &exe, &["menubar"])?;
    println!("Installed and started the usagio menu bar app — it now runs at every login.");
    println!(
        "Logs: {}",
        store::config_dir()?.join("usagio.log").display()
    );
    Ok(())
}

fn cmd_uninstall() -> Result<()> {
    platform().autostart().uninstall(AUTOSTART_LABEL)?;
    // Also clean up the pre-v0.4.0 plist if the user last installed under
    // the `claude-usage` name.
    migrate_launchd_if_needed();
    println!("Uninstalled — the menu-bar app will no longer start at login.");
    Ok(())
}

/// Bump the soft `RLIMIT_NOFILE` up to the hard cap (or 65536, whichever is
/// smaller). macOS ships every process with a soft cap of 256 files, which
/// is fine for a CLI one-shot but crippling for a menubar that keeps
/// fsnotify watchers open, opens state.json on every poll, does DNS, and
/// talks to the keychain. libc getrlimit/setrlimit are always available on
/// Unix; failure is logged and swallowed (we still run, just with the
/// stock cap).
///
/// The ceiling was 4096, but a since-fixed fsnotify fd leak (the `macos_kqueue`
/// backend — see `credentials.rs`) blew past 4096 and started failing keychain
/// calls with EMFILE. The real fix removed the leak; this higher ceiling is
/// defense-in-depth so any future fd growth degrades gracefully rather than
/// silently breaking account switching.
#[cfg(unix)]
fn raise_nofile_limit() {
    // SAFETY: getrlimit/setrlimit take a valid resource id and a writable
    // rlimit struct; both preconditions are trivially met here.
    unsafe {
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) != 0 {
            crate::logging::log("credentials: getrlimit(NOFILE) failed; leaving stock cap");
            return;
        }
        let target = rl.rlim_max.min(65536);
        if rl.rlim_cur >= target {
            return;
        }
        let new = libc::rlimit {
            rlim_cur: target,
            rlim_max: rl.rlim_max,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &new) != 0 {
            crate::logging::log(&format!(
                "credentials: setrlimit(NOFILE, {target}) failed; \
                 leaving soft cap at {}",
                rl.rlim_cur
            ));
        }
    }
}

/// One-shot launchd label migration. If a pre-v0.4.0 plist named
/// `com.claude-usage.menubar.plist` still exists in `~/Library/LaunchAgents/`,
/// unload it (best-effort) and remove the file so it doesn't run in parallel
/// with the new `com.mattjackson.usagio.menubar` agent.
fn migrate_launchd_if_needed() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let old_plist = std::path::PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LEGACY_AUTOSTART_LABEL}.plist"));
    if !old_plist.exists() {
        return;
    }
    // Best-effort: the plist may already be unloaded (typical after a reboot
    // or `brew uninstall`). launchctl prints "Unload failed: 5: Input/output
    // error" to stderr in that case, which surfaces as a scary line during
    // `usagio install`. Swallow stdout+stderr — we only care whether the
    // file removal below succeeds.
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &old_plist.to_string_lossy()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match std::fs::remove_file(&old_plist) {
        Ok(()) => {
            eprintln!(
                "usagio: removed legacy launchd plist {}",
                old_plist.display()
            );
        }
        Err(e) => {
            eprintln!(
                "usagio: could not remove legacy launchd plist {}: {}",
                old_plist.display(),
                e
            );
        }
    }
}

/// Purge any legacy System Events login item a pre-v0.4.3 usagio (or its
/// `claude-usage` predecessor) registered via the now-removed "Launch at
/// login" menu toggle (contract-01, v0.5.2 codeaudit). Left in place, macOS
/// re-launches the stale binary at every login even after the launchd plist
/// is gone/never installed. `MacOsAutostart::uninstall`
/// (`src/platform/macos.rs`) already runs this exact purge on `usagio
/// uninstall`; this is a `main.rs`-local copy so `cmd_install` can run it too
/// without editing `src/platform/**` (out of scope for this pass — a later
/// pass should hoist both call sites onto one shared helper there instead of
/// keeping this duplicate in sync by hand).
///
/// Best-effort everywhere it's called from (`cmd_install`, which today only
/// runs meaningfully on macOS — `platform().autostart()` is the thing that's
/// actually OS-gated, via the `Platform` trait in `src/platform/`). No
/// `#[cfg(target_os)]` here on purpose: `tests/strict_cfg.rs` restricts that
/// attribute to `src/platform/*`, and spawning a nonexistent `timeout`/
/// `osascript` binary on a non-macOS host is already a silent, swallowed
/// `Err` — `Command::spawn`'s ordinary "not found" failure mode — so the
/// unconditional call is a no-op there without needing an explicit gate.
/// Also a no-op if the login item is already absent on macOS — `osascript`
/// exiting non-zero in that case is the expected, swallowed outcome.
fn purge_legacy_login_items() {
    for name in ["usagio", "claude-usage"] {
        let _ = std::process::Command::new("timeout")
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
                "{marker}  {:<28} no data yet (run the menu-bar app or `usagio watch`)",
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
        "Usage updates on a schedule (menu-bar app / `usagio watch`). \
         Run `usagio list --refresh` to fetch now.\n"
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
    // Anything under a minute (incl. already-past) reads "<1m", never "0m" /
    // "now" — a sub-minute countdown flooring to "0m" looked like it had
    // already reset when it hadn't (and matches `countdown::format_countdown`).
    if secs < 60 {
        return "<1m".to_string();
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
