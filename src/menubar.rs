//! macOS menu-bar app: shows the active account's usage in the status bar and
//! lets you switch / capture / remove accounts and set the auto-swap threshold
//! from a dropdown. The same `watch_cycle` that powers `claude-usage watch` runs
//! on a background thread here, so the daemon behaviour is identical.
//!
//! Usage numbers come from the cache (written by the scheduler poll); the UI
//! never fetches on its own, so menu interactions can't trigger HTTP 429s.
//!
//! Menu wiring iterates `providers::all()` and emits one `ProviderSection` per
//! provider — but only if that provider has at least one captured account in
//! state (see `build_snapshot`). The "Capture current login ▸" submenu always
//! shows, with one row per REGISTERED provider (installed or not).

use anyhow::Result;
use std::cell::RefCell;
use std::time::Duration;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::NSTimer;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::TrayIconBuilder;

use crate::providers::{self, SeverityBands};
use crate::store::State;
use crate::{
    age_str, capture_current, env_override_active, menu_order, next_interval, notify,
    optimize_now, remove_account, row_from_account, switch_to, watch_cycle, with_state_lock,
    Row, SwapGuard, CLAUDE_SLUG, TARGET_CEILING_PCT, TRIGGER_PCT, WATCH_INTERVAL_SECS,
};

/// Name shown for our Login Item in System Events.
const LOGIN_ITEM_NAME: &str = "Claude Usage";

/// Exact title of the disabled section row inserted when a provider's env
/// override is active. A named constant so `build_menu` and the tests
/// asserting on the visible menu content can't drift out of sync.
pub(crate) const ENV_OVERRIDE_ROW_TITLE: &str = "env override active — swap disabled";

/// One rate-limit / quota window as displayed under an account's submenu. `id`
/// is the provider's own window slug ("session"/"weekly"/"opus" for Claude);
/// `label` is the short human string ("5h"/"7d"/"Opus 7d").
#[derive(Clone)]
struct WindowView {
    id: String,
    label: String,
    pct: Option<f64>,
    /// Human "resets in X" text, or empty when unknown.
    reset: String,
}

/// One account as shown in the menu. Windows are ordered by
/// `provider.window_order()`; `has_data` gates the stat-row block.
struct AcctView {
    provider_id: &'static str,
    /// Stable key used inside click ids (`switch:claude:<key>` etc). In v1
    /// this is the account's email; later phases key on the provider's
    /// `account_identifier`.
    key: String,
    /// Human-facing label rendered in the submenu title.
    display: String,
    windows: Vec<WindowView>,
    updated: String,
    active: bool,
    has_data: bool,
}

/// One provider's block in the menu. Rendered only if `accounts` is non-empty
/// (per the "section renders only when the provider has at least one captured
/// account" rule).
pub(crate) struct ProviderSection {
    provider_id: &'static str,
    display_name: &'static str,
    supports_switching: bool,
    supports_usage: bool,
    severity_bands: SeverityBands,
    /// True when the provider's OAuth env-override is set on this process's
    /// environment. Surfaced as a disabled row inside the section and used by
    /// `watch_cycle` to skip that provider entirely.
    env_override_active: bool,
    accounts: Vec<AcctView>,
}

/// Every registered provider's identity, for the always-visible "Capture
/// current login ▸" submenu. `installed` is a best-effort probe used to grey
/// out rows whose credential store isn't present on this host; the row stays
/// clickable so errors surface honestly.
struct RegisteredProvider {
    provider_id: &'static str,
    display_name: &'static str,
    installed: bool,
}

/// Everything the UI needs to render, produced by the poller thread.
#[derive(Default)]
struct Snapshot {
    sections: Vec<ProviderSection>,
    registered: Vec<RegisteredProvider>,
    autoswap: bool,
    threshold: f64,
    start_at_login: bool,
}

/// How near a limit a percentage is, for at-a-glance coloring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Severity {
    /// >= amber band: approaching the wall.
    Amber,
    /// >= red band: about to hit it.
    Red,
}

/// Map a utilization percentage to a color band given the provider's bands.
fn severity_with(p: Option<f64>, bands: SeverityBands) -> Option<Severity> {
    match p {
        Some(v) if v >= bands.red => Some(Severity::Red),
        Some(v) if v >= bands.amber => Some(Severity::Amber),
        _ => None,
    }
}

/// A styling directive for one menu row, matched to its native `NSMenuItem` by
/// the plain title string. We build the muda menu with plain titles (so clicks
/// and structure work exactly as before) then walk the native `NSMenu` and set
/// `attributedTitle` on the rows named here. Offsets are **UTF-16 code units**
/// (what `NSRange` uses); all our runs are ASCII so char == utf16 in practice,
/// but the helpers stay correct if an email ever isn't.
struct RowStyle {
    /// The exact plain title set on the item; used to find it in the menu.
    plain: String,
    /// Bold the whole row (marks the active account instead of a checkmark).
    bold: bool,
    /// Whether the row is a section header (bold, disabled, no tab-stop).
    section_header: bool,
    /// Colored spans: (utf16 offset, utf16 length, band).
    colors: Vec<(usize, usize, Severity)>,
    /// If set, right-align everything after the first `\t` at this x (points),
    /// battery-menu style. Requires the plain title to contain a `\t`.
    tab_x: Option<f64>,
}

/// Fixed x (points) for the right-aligned trailing `S% / W%`. The menu font is
/// proportional, so this must clear the widest email; the menu auto-widens to
/// fit, so over-provisioning only adds a little slack on the right.
const TAB_X: f64 = 260.0;

/// Length of a string in UTF-16 code units (the unit `NSRange` counts in).
fn u16len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// Human-facing label for one window row inside an account submenu. Preserves
/// the v1 "Session"/"Weekly"/"Opus" copy for the Claude windows so a menu that
/// used to read `Session  85%  · resets in 3h` still does after the refactor.
/// Unknown window ids fall through to the provider's own short `label` so a
/// future provider still surfaces something readable.
fn stat_display_label(w: &WindowView) -> &str {
    match w.id.as_str() {
        "session" => "Session",
        "weekly" => "Weekly",
        "opus" => "Opus",
        _ => w.label.as_str(),
    }
}

/// The first two windows of an account, used to build the two-percentage
/// summary text in the header rows. `(pct_a, pct_b)` — either may be `None`.
fn summary_pcts(a: &AcctView) -> (Option<f64>, Option<f64>) {
    let a0 = a.windows.first().and_then(|w| w.pct);
    let a1 = a.windows.get(1).and_then(|w| w.pct);
    (a0, a1)
}

/// The active-account submenu header: `display \t A% / B%`, bold if active,
/// with each high percentage colored per the provider's severity bands.
fn header_row(a: &AcctView, bands: SeverityBands) -> RowStyle {
    let (pa, pb) = summary_pcts(a);
    let sa = pct(pa);
    let sb = pct(pb);
    let plain = format!("{}\t{sa} / {sb}", a.display);
    let mut colors = Vec::new();
    let s_off = u16len(&a.display) + 1; // + '\t'
    if let Some(sev) = severity_with(pa, bands) {
        colors.push((s_off, u16len(&sa), sev));
    }
    let w_off = s_off + u16len(&sa) + u16len(" / ");
    if let Some(sev) = severity_with(pb, bands) {
        colors.push((w_off, u16len(&sb), sev));
    }
    RowStyle {
        plain,
        bold: a.active,
        section_header: false,
        colors,
        tab_x: Some(TAB_X),
    }
}

/// The top info line for the active account: `display  ·  A% / B%`,
/// percentages colored (no bold, no tab — it's a disabled header, not a row).
fn top_header_row(a: &AcctView, bands: SeverityBands) -> RowStyle {
    let (pa, pb) = summary_pcts(a);
    let sa = pct(pa);
    let sb = pct(pb);
    let plain = header_line(a);
    let mut colors = Vec::new();
    let s_off = u16len(&a.display) + u16len("  ·  ");
    if let Some(sev) = severity_with(pa, bands) {
        colors.push((s_off, u16len(&sa), sev));
    }
    let w_off = s_off + u16len(&sa) + u16len(" / ");
    if let Some(sev) = severity_with(pb, bands) {
        colors.push((w_off, u16len(&sb), sev));
    }
    RowStyle {
        plain,
        bold: false,
        section_header: false,
        colors,
        tab_x: None,
    }
}

/// All styling directives for the current menu, derived from the same snapshot
/// `build_menu` renders. The native walk applies each by matching `.plain`.
fn menu_styles(snap: &Snapshot) -> Vec<RowStyle> {
    let mut styles = Vec::new();
    // Top header: pick the active account across all sections. Its own
    // section's severity bands drive coloring so a provider with different
    // thresholds gets its own numbers colored right.
    if let Some((sec, a)) = active_account(snap) {
        styles.push(top_header_row(a, sec.severity_bands));
    }
    for sec in &snap.sections {
        // The section header row itself gets styled (bold, disabled). Plain
        // title matches the muda MenuItem title so the walker finds it.
        styles.push(RowStyle {
            plain: sec.display_name.to_string(),
            bold: false,
            section_header: true,
            colors: Vec::new(),
            tab_x: None,
        });
        for a in &sec.accounts {
            styles.push(header_row(a, sec.severity_bands));
            if a.has_data {
                for w in &a.windows {
                    if let Some(sev) = severity_with(w.pct, sec.severity_bands) {
                        let (plain, span) = stat_row(stat_display_label(w), w.pct, &w.reset);
                        if let Some((off, len)) = span {
                            styles.push(RowStyle {
                                plain,
                                bold: false,
                                section_header: false,
                                colors: vec![(off, len, sev)],
                                tab_x: None,
                            });
                        }
                    }
                }
            }
        }
    }
    styles
}

/// Locate the active section + account (if any) in the snapshot.
fn active_account(snap: &Snapshot) -> Option<(&ProviderSection, &AcctView)> {
    for sec in &snap.sections {
        if let Some(a) = sec.accounts.iter().find(|a| a.active) {
            return Some((sec, a));
        }
    }
    None
}

pub fn run() -> Result<()> {
    // Register providers on the main thread before anything else — the poll
    // thread and menu build both dispatch through `providers::get`.
    providers::init();

    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("the menu bar must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    // Background (menu-bar-only) app: no Dock icon, even as the bare binary.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // Poll + auto-swap on a background thread. It writes cached usage to
    // state.json; the main-thread timer reads it back to render.
    std::thread::spawn(poll_loop);

    // Remember the binary we launched from so the timer can notice a `brew
    // upgrade` replacing it and relaunch into the new version.
    let start_exe = std::fs::canonicalize(crate::stable_exe_path()).ok();

    // Build the tray on the main thread and keep it alive for the app's lifetime.
    let initial = build_snapshot();
    let tray = TrayIconBuilder::new()
        .with_title(title_for(&initial))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to create tray icon: {e}"))?;
    install_menu(&tray, &initial);
    let _ = tray.set_tooltip(Some(tooltip_for(&initial)));

    // All UI updates happen in this timer, scheduled in the DEFAULT run-loop mode.
    // A status menu opens its own nested tracking loop (NSEventTrackingRunLoopMode);
    // a default-mode timer never fires there, so an open menu is never dismissed.
    // (This is exactly the tao bug we sidestepped by dropping tao: it added its
    // run-loop observer to kCFRunLoopCommonModes, which *does* fire during
    // tracking and collapsed the menu.) Each tick rebuilds the display from cached
    // state — so "updated Xm ago" ticks and a CLI switch shows up within ~1s — and
    // only re-installs the menu/title when the rendered content actually changes.
    let menu_rx = MenuEvent::receiver().clone();
    let last_sig = RefCell::new(menu_signature(&initial));
    let last_title = RefCell::new(title_for(&initial));
    let tick = RcBlock::new(move |_t: core::ptr::NonNull<NSTimer>| {
        // If `brew upgrade` replaced our binary, relaunch into the new version.
        if let Some(start) = &start_exe {
            maybe_relaunch_after_upgrade(start);
        }
        // A menu closes before its click is delivered, so handling it here won't
        // fight menu tracking.
        while let Ok(ev) = menu_rx.try_recv() {
            handle_click(&ev.id.0);
        }
        let snap = build_snapshot();
        let sig = menu_signature(&snap);
        if *last_sig.borrow() != sig {
            install_menu(&tray, &snap);
            let _ = tray.set_tooltip(Some(tooltip_for(&snap)));
            *last_sig.borrow_mut() = sig;
        }
        let title = title_for(&snap);
        if *last_title.borrow() != title {
            tray.set_title(Some(title.clone()));
            *last_title.borrow_mut() = title;
        }
    });
    // The run loop retains the timer; scheduled timers fire in the default mode.
    let _timer =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.75, true, &tick) };

    app.run();
    Ok(())
}

// ---------------------------------------------------------------------------
// Poller thread
// ---------------------------------------------------------------------------

fn poll_loop() {
    let mut guard = SwapGuard::default();
    let base = WATCH_INTERVAL_SECS;
    let mut current = base;
    loop {
        // Fetch usage + auto-swap; this writes cached usage to state.json, which
        // the main-thread timer reads back to render. This is the ONLY thing that
        // hits the network, so ordinary use can never rate-limit.
        let rate_limited = run_cycle(&mut guard);
        current = next_interval(current, base, rate_limited);
        if rate_limited {
            crate::logging::log(&format!("rate limited; backing off to {current}s"));
        }
        std::thread::sleep(Duration::from_secs(current));
    }
}

/// Relaunch into the on-disk binary if it changed since we started (i.e. a
/// `brew upgrade` repointed the stable symlink), so the menu bar hot-swaps to
/// the new version without waiting for the next login.
///
/// When we're the launchd-managed agent we must NOT just spawn a child and
/// exit: the child is in the job's process group, and launchd SIGKILLs that
/// whole group when the main process exits (AbandonProcessGroup defaults to
/// false), so the replacement dies with us and — with KeepAlive=false — is never
/// restarted. Instead we ask launchd to restart the job (`launchctl kickstart
/// -k`), which relaunches it in a fresh job context. A bare/from-source run has
/// no such job, so there the orphaned self-spawn survives our exit as usual.
fn maybe_relaunch_after_upgrade(start: &std::path::Path) {
    let stable = crate::stable_exe_path();
    let Ok(now) = std::fs::canonicalize(&stable) else {
        return;
    };
    if now == start {
        return;
    }
    crate::logging::log("binary changed on disk (brew upgrade); relaunching");
    match relaunch_via_launchd() {
        LaunchdRestart::Issued => {
            // kickstart -k will SIGKILL and restart this job; wait to be replaced.
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        }
        LaunchdRestart::Failed => {
            // Under launchd but the kickstart didn't take. Staying on the current
            // (old) binary — alive — is far better than exiting into a dead,
            // never-restarted state (KeepAlive=false won't bring us back).
            crate::logging::log("launchctl kickstart failed; staying on current version");
        }
        LaunchdRestart::NotManaged => {
            // Bare run (no launchd job): an orphaned child re-parents to
            // launchd/init and outlives our exit — but only exit if the spawn
            // actually succeeded, else we'd vanish with no replacement.
            match std::process::Command::new(&stable).arg("menubar").spawn() {
                Ok(_) => std::process::exit(0),
                Err(e) => crate::logging::log(&format!(
                    "relaunch spawn failed: {e}; staying on current version"
                )),
            }
        }
    }
}

/// Outcome of attempting a launchd-driven restart.
enum LaunchdRestart {
    /// kickstart succeeded — the caller should wait to be replaced.
    Issued,
    /// We're launchd-managed but kickstart failed — caller must NOT exit.
    Failed,
    /// Not launchd-managed — caller should self-spawn a replacement.
    NotManaged,
}

/// If we're running as the launchd agent, ask launchd to kill+restart the job
/// (`launchctl kickstart -k`). We wait on the command's exit status — reporting
/// success only when the kickstart actually took, so a failed request can't lead
/// the caller to exit into a dead, unrestarted state.
fn relaunch_via_launchd() -> LaunchdRestart {
    if !is_launchd_managed() {
        return LaunchdRestart::NotManaged;
    }
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/{}", crate::LAUNCHD_LABEL);
    crate::logging::log(&format!("relaunching via launchctl kickstart -k {target}"));
    match std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .status()
    {
        Ok(s) if s.success() => LaunchdRestart::Issued,
        Ok(s) => {
            crate::logging::log(&format!("launchctl kickstart exited with {s}"));
            LaunchdRestart::Failed
        }
        Err(e) => {
            crate::logging::log(&format!("launchctl kickstart could not run: {e}"));
            LaunchdRestart::Failed
        }
    }
}

/// Whether we're the launchd-managed agent. `XPC_SERVICE_NAME` is set by launchd
/// to the job label for a LaunchAgent, so a manual `claude-usage menubar` run
/// (which self-spawns fine) isn't misrouted to the kickstart path.
fn is_launchd_managed() -> bool {
    launchd_managed_from_env(std::env::var("XPC_SERVICE_NAME").ok().as_deref())
}

/// Pure predicate behind `is_launchd_managed`, split out for testing.
fn launchd_managed_from_env(xpc_service_name: Option<&str>) -> bool {
    xpc_service_name == Some(crate::LAUNCHD_LABEL)
}

/// Run one poll+auto-swap cycle; returns whether it was rate limited.
fn run_cycle(guard: &mut SwapGuard) -> bool {
    let st = State::load().unwrap_or_default();
    let autoswap = !st.autoswap_disabled;
    let threshold = st.trigger_pct.unwrap_or(TRIGGER_PCT);
    // With auto-swap off, use an unreachable trigger so we only observe.
    let trigger = if autoswap { threshold } else { 101.0 };
    match watch_cycle(trigger, TARGET_CEILING_PCT, guard) {
        Ok(o) => o.rate_limited,
        Err(e) => {
            crate::logging::log(&format!("menubar poll failed: {e}"));
            false
        }
    }
}

/// Whether the provider currently under an env-override that would defeat any
/// switch we make. Delegates to the shared `crate::env_override_active` so
/// the menu display and the `choose_swap_target` auto-swap filter can never
/// disagree, and so tests can toggle overrides through one hook.
fn env_override_for(provider_id: &str) -> bool {
    env_override_active(provider_id)
}

/// Build one account's rendered view from a v1 `Row`. v1 state only has
/// Claude accounts, so the window set is fixed (session / weekly / opus); the
/// window IDs come from `provider.window_order()` so this is trivially
/// generalizable when state v2 lands.
fn acctview_from_row(
    r: &Row,
    active: &Option<String>,
    provider_id: &'static str,
    window_order: &'static [&'static str],
) -> AcctView {
    // Build the fixed pool of windows we know about for this v1 row.
    let mut pool: Vec<WindowView> = Vec::new();
    pool.push(WindowView {
        id: "session".into(),
        label: "5h".into(),
        pct: r.session.pct,
        reset: r.session.resets_in(),
    });
    pool.push(WindowView {
        id: "weekly".into(),
        label: "7d".into(),
        pct: r.weekly.pct,
        reset: r.weekly.resets_in(),
    });
    if let Some(c) = &r.opus {
        pool.push(WindowView {
            id: "opus".into(),
            label: "Opus 7d".into(),
            pct: c.pct,
            reset: c.resets_in(),
        });
    }
    // Present them in the provider's declared order; unknown IDs (a
    // provider added in later phases) fall through to their pool order.
    let mut windows: Vec<WindowView> = Vec::with_capacity(pool.len());
    for id in window_order {
        if let Some(pos) = pool.iter().position(|w| w.id == *id) {
            windows.push(pool.remove(pos));
        }
    }
    windows.extend(pool);

    AcctView {
        provider_id,
        key: r.email.clone(),
        display: r.email.clone(),
        windows,
        updated: age_str(r.fetched_at),
        active: active.as_deref() == Some(r.email.as_str()),
        has_data: r.has_data(),
    }
}

/// Build the UI snapshot from local State only (no network), reading each
/// account's cached usage. Runs every 0.75s UI tick, so it avoids re-reading
/// state.json unless its mtime changed and probes the Login Item at most once a
/// minute (both were per-tick subprocess/disk costs before).
///
/// A provider's section is only emitted if at least one captured account exists
/// for it in state — that's the "no header, no rows" rule from the design.
/// The "Capture current login" submenu is separately fed from
/// `providers::all()` so every REGISTERED provider stays listable there.
fn build_snapshot() -> Snapshot {
    let st = cached_state();
    let autoswap = !st.autoswap_disabled;
    let threshold = st.trigger_pct.unwrap_or(TRIGGER_PCT);
    let active = st.active.clone();

    // v1 state only has Claude accounts. Group by provider slug so once state
    // v2 lands (each account tagged with its provider), this loop generalises
    // with a one-line change (filter by account's slug instead of hardcoded
    // CLAUDE_SLUG).
    let mut rows: Vec<Row> = st.accounts.iter().map(row_from_account).collect();
    rows.sort_by(menu_order);

    let mut sections: Vec<ProviderSection> = Vec::new();
    for provider in providers::all() {
        let slug = provider.provider_id();
        // In v1 every stored row is a Claude account. Once state carries a
        // per-account slug this becomes `rows.iter().filter(|r| r.provider_id == slug)`.
        let provider_rows: Vec<&Row> =
            rows.iter().filter(|r| r.provider_id == slug).collect();
        if provider_rows.is_empty() {
            continue; // no captured accounts → no section (no header, no rows).
        }
        let accounts: Vec<AcctView> = provider_rows
            .into_iter()
            .map(|r| acctview_from_row(r, &active, slug, provider.window_order()))
            .collect();
        let caps = provider.capabilities();
        sections.push(ProviderSection {
            provider_id: slug,
            display_name: provider.display_name(),
            supports_switching: caps.supports_switching,
            supports_usage: caps.supports_usage,
            severity_bands: provider.severity_bands(),
            env_override_active: env_override_for(slug),
            accounts,
        });
    }

    // The capture submenu always lists every registered provider — including
    // ones with zero captured accounts — so onboarding for a new provider is
    // discoverable from the menu itself.
    let registered: Vec<RegisteredProvider> = providers::all()
        .iter()
        .map(|p| RegisteredProvider {
            provider_id: p.provider_id(),
            display_name: p.display_name(),
            // v1 only knows how to probe the Claude keychain. Everything else
            // is assumed installed; wrong guesses just surface a real error
            // on the capture attempt instead of pre-emptively greying out.
            installed: probe_installed(p.provider_id()),
        })
        .collect();

    Snapshot {
        sections,
        registered,
        autoswap,
        threshold,
        start_at_login: cached_login_item_enabled(),
    }
}

/// Best-effort per-provider "is the credential store present on this host?"
/// probe used to grey out capture rows. Cheap enough for the 0.75s tick — for
/// Claude it does an mtime-cached keychain lookup only when state.json changes
/// (via `cached_state`). Non-Claude providers default to `true` so a stub
/// provider's row stays enabled and its click reports a real error.
fn probe_installed(_slug: &str) -> bool {
    // v1: don't do anything expensive per tick. A future phase can front this
    // with a TTL cache once real stub providers land.
    true
}

/// Return the parsed state, re-reading state.json only when its mtime changed.
/// The 0.75s UI tick would otherwise read+parse the file every tick forever.
/// Only the main thread calls this, but a Mutex keeps it trivially sound.
fn cached_state() -> State {
    use std::sync::{Mutex, OnceLock};
    use std::time::SystemTime;
    struct C {
        mtime: Option<SystemTime>,
        state: State,
        loaded: bool,
    }
    static CELL: OnceLock<Mutex<C>> = OnceLock::new();
    let cell = CELL.get_or_init(|| {
        Mutex::new(C {
            mtime: None,
            state: State::default(),
            loaded: false,
        })
    });
    let path = crate::store::config_dir()
        .map(|d| d.join("state.json"))
        .unwrap_or_default();
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut g = cell.lock().unwrap_or_else(|e| e.into_inner());
    if !g.loaded || g.mtime != mtime {
        g.state = State::load().unwrap_or_default();
        g.mtime = mtime;
        g.loaded = true;
    }
    g.state.clone()
}

/// How often to re-probe the Login Item state (an osascript subprocess).
const LOGIN_PROBE_TTL: Duration = Duration::from_secs(60);

/// Login-item enabled state, probed at most once per `LOGIN_PROBE_TTL` (the
/// probe forks an `osascript`; doing it every 0.75s tick was the top perf cost).
/// `set_login_item_cache` refreshes it immediately when we toggle it ourselves.
fn cached_login_item_enabled() -> bool {
    let mut g = login_cache().lock().unwrap_or_else(|e| e.into_inner());
    let fresh = g
        .map(|(_, t)| t.elapsed() < LOGIN_PROBE_TTL)
        .unwrap_or(false);
    if !fresh {
        let v = login_item_enabled();
        *g = Some((v, std::time::Instant::now()));
    }
    g.map(|(v, _)| v).unwrap_or(false)
}

/// Record a known Login Item state (after we toggle it) so the menu reflects it
/// at once instead of waiting for the next probe.
fn set_login_item_cache(enabled: bool) {
    *login_cache().lock().unwrap_or_else(|e| e.into_inner()) =
        Some((enabled, std::time::Instant::now()));
}

fn login_cache() -> &'static std::sync::Mutex<Option<(bool, std::time::Instant)>> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<Option<(bool, std::time::Instant)>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(None))
}

// ---------------------------------------------------------------------------
// Menu building
// ---------------------------------------------------------------------------

/// Disabled/header-only rows a section prepends before its account submenus.
/// Currently: the "env override active — swap disabled" row when the section's
/// provider is env-overridden. Pure, so tests can assert on it without
/// instantiating any native menu (muda requires the main thread on macOS).
pub(crate) fn section_headline_rows(sec: &ProviderSection) -> Vec<&'static str> {
    let mut rows = Vec::new();
    if sec.env_override_active {
        rows.push(ENV_OVERRIDE_ROW_TITLE);
    }
    rows
}

fn build_menu(snap: &Snapshot) -> Menu {
    let menu = Menu::new();

    // Top header — active account across every section, or a placeholder.
    match active_account(snap) {
        Some((_sec, a)) => {
            add(&menu, MenuItem::with_id("hdr", header_line(a), false, None));
            // "weekly resets in X" — use the second window's reset text if
            // populated, mirroring the v1 Claude two-window layout.
            // Second-line header preserved verbatim from v1 ("weekly resets in
            // X"). Uses the second window's reset text — for Claude that's the
            // 7-day window, matching the v1 layout. Providers whose "weekly-
            // analog" window isn't 7d still get accurate copy, since the
            // countdown text itself comes from `resets_in()`.
            let weekly_reset = a.windows.get(1).map(|w| w.reset.as_str()).unwrap_or("");
            if !weekly_reset.is_empty() {
                let line = format!("weekly resets in {weekly_reset}");
                add(&menu, MenuItem::with_id("hdr2", line, false, None));
            }
        }
        None => add(
            &menu,
            MenuItem::with_id("hdr", "No active account", false, None),
        ),
    }
    let _ = menu.append(&PredefinedMenuItem::separator());

    if snap.sections.is_empty() {
        add(
            &menu,
            MenuItem::with_id("none", "Capture a login below to begin", false, None),
        );
    }

    // Per-provider sections. Only emit a header + submenus if the provider
    // has at least one captured account — the "no header, no rows" rule.
    for sec in &snap.sections {
        // Section header: bold, disabled (styled later by `apply_menu_styles`
        // via the RowStyle matching on the plain title).
        add(
            &menu,
            MenuItem::with_id("noop", sec.display_name, false, None),
        );
        for title in section_headline_rows(sec) {
            add(
                &menu,
                MenuItem::with_id(
                    format!("envoverride:{}", sec.provider_id),
                    title,
                    false,
                    None,
                ),
            );
        }
        for a in &sec.accounts {
            build_account_submenu(&menu, sec, a);
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
    }

    // Auto-swap: one submenu, Off / 90 / 95 / 98. Stays global for v1 (the
    // click grammar reserves `autoswap:<slug>:<n>` for per-provider later).
    let cur = if snap.autoswap {
        snap.threshold.round() as i32
    } else {
        0
    };
    let swap = Submenu::with_id("autoswap", "Auto-swap at high usage", true);
    let _ = swap.append(&CheckMenuItem::with_id(
        "autoswap:off",
        "Off",
        true,
        cur == 0,
        None,
    ));
    for t in [90i32, 95, 98] {
        let _ = swap.append(&CheckMenuItem::with_id(
            format!("autoswap:{t}"),
            format!("{t}%"),
            true,
            cur == t,
            None,
        ));
    }
    let _ = swap.append(&PredefinedMenuItem::separator());
    let _ = swap.append(&MenuItem::with_id(
        "autoswap:now",
        "Switch to best account now",
        true,
        None,
    ));
    let _ = menu.append(&swap);
    let _ = menu.append(&PredefinedMenuItem::separator());

    // Capture current login: ALWAYS shows, one row per REGISTERED provider —
    // even providers with zero captured accounts so onboarding is discoverable.
    let capture = Submenu::with_id("capture", "Capture current login", true);
    if snap.registered.is_empty() {
        let _ = capture.append(&MenuItem::with_id(
            "noop",
            "(no providers registered)",
            false,
            None,
        ));
    } else {
        for reg in &snap.registered {
            let title = if reg.installed {
                reg.display_name.to_string()
            } else {
                format!("{} (not installed)", reg.display_name)
            };
            // Row stays clickable even when we think it's not installed —
            // the actual `capture_current_login` call surfaces the real error.
            let _ = capture.append(&MenuItem::with_id(
                format!("capture:{}", reg.provider_id),
                title,
                true,
                None,
            ));
        }
    }
    let _ = menu.append(&capture);

    add_check(
        &menu,
        CheckMenuItem::with_id(
            "startlogin",
            "Launch at login",
            true,
            snap.start_at_login,
            None,
        ),
    );
    let _ = menu.append(&PredefinedMenuItem::separator());
    add(
        &menu,
        MenuItem::with_id(
            "version",
            format!("claude-usage v{}", env!("CARGO_PKG_VERSION")),
            false,
            None,
        ),
    );
    add(&menu, MenuItem::with_id("quit", "Quit", true, None));

    menu
}

/// Build one account's submenu inside a provider section. Splits Switch /
/// stats / Launch / Remove; the pieces vary by capability so a reporting-only
/// provider drops the Switch item and a no-usage provider swaps the stat block
/// for a `(no usage endpoint — headers only)` disabled row.
fn build_account_submenu(menu: &Menu, sec: &ProviderSection, a: &AcctView) {
    let head = header_row(a, sec.severity_bands).plain;
    let sub = Submenu::with_id(
        format!("sub:{}:{}", sec.provider_id, a.key),
        head,
        true,
    );
    if sec.supports_switching {
        if a.active {
            let _ = sub.append(&MenuItem::with_id("noop", "✓ Active", false, None));
        } else {
            let _ = sub.append(&MenuItem::with_id(
                format!("switch:{}:{}", sec.provider_id, a.key),
                "Switch to this account",
                true,
                None,
            ));
        }
    }
    let _ = sub.append(&PredefinedMenuItem::separator());
    if sec.supports_usage {
        if a.has_data && !a.windows.is_empty() {
            for w in &a.windows {
                let _ = sub.append(&stat_item(stat_display_label(w), w.pct, &w.reset));
            }
            let _ = sub.append(&MenuItem::with_id(
                "noop",
                format!("updated {}", a.updated),
                false,
                None,
            ));
        } else {
            let _ = sub.append(&MenuItem::with_id("noop", "no data yet", false, None));
        }
    } else {
        let _ = sub.append(&MenuItem::with_id(
            "noop",
            "(no usage endpoint — headers only)",
            false,
            None,
        ));
    }
    let _ = sub.append(&PredefinedMenuItem::separator());
    // "Launch" only exposed for providers that both switch and know how to
    // spawn their client. The trait's default `launch_client` returns
    // `Unsupported`, so a click on this for a stub provider surfaces a
    // real error rather than doing nothing.
    if sec.supports_switching {
        let _ = sub.append(&MenuItem::with_id(
            format!("launch:{}:{}", sec.provider_id, a.key),
            "Launch client",
            true,
            None,
        ));
    }
    let _ = sub.append(&MenuItem::with_id(
        format!("remove:{}:{}", sec.provider_id, a.key),
        "Remove…",
        true,
        None,
    ));
    let _ = menu.append(&sub);
}

/// Build the menu for `snap`, install it on the tray, then style the native
/// rows (bold active account, right-aligned trailing `S% / W%`, high
/// percentages colored) via `attributedTitle`. We take the `NSMenu` pointer
/// before moving the menu into `set_menu`: the menu is reference-counted and the
/// tray retains it, so the pointer stays valid for the walk. The attributed
/// titles persist until the next rebuild (muda only overwrites a title if we
/// call `set_text`, which we never do on these items).
fn install_menu(tray: &tray_icon::TrayIcon, snap: &Snapshot) {
    let menu = build_menu(snap);
    #[cfg(target_os = "macos")]
    let ns_menu = {
        use tray_icon::menu::ContextMenu;
        menu.ns_menu()
    };
    tray.set_menu(Some(Box::new(menu)));
    #[cfg(target_os = "macos")]
    apply_menu_styles(ns_menu, &menu_styles(snap));
    #[cfg(not(target_os = "macos"))]
    let _ = snap;
}

/// Walk the native `NSMenu` (and its submenus) and set `attributedTitle` on any
/// item whose plain title matches a `RowStyle` — the mechanism muda's plain
/// string API can't reach (right-aligned tab stops and arbitrary colors).
#[cfg(target_os = "macos")]
fn apply_menu_styles(ns_menu: *mut core::ffi::c_void, styles: &[RowStyle]) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::AllocAnyThread;
    use objc2_app_kit::{
        NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMenu,
        NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSTextAlignment, NSTextTab,
        NSTextTabOptionKey,
    };
    use objc2_foundation::{
        NSArray, NSAttributedString, NSDictionary, NSMutableAttributedString, NSRange, NSString,
    };

    if ns_menu.is_null() {
        return;
    }

    fn color_for(sev: Severity) -> Retained<NSColor> {
        match sev {
            Severity::Amber => NSColor::systemOrangeColor(),
            Severity::Red => NSColor::systemRedColor(),
        }
    }

    /// Build the attributed title for one row from its `RowStyle`.
    fn attributed(style: &RowStyle) -> Retained<NSAttributedString> {
        let ns_text = NSString::from_str(&style.plain);
        // NSRange is UTF-16 code units — use NSString::length, not byte length.
        let full_len = ns_text.length();
        let attr =
            NSMutableAttributedString::initWithString(NSMutableAttributedString::alloc(), &ns_text);

        // Right-aligned trailing run at a fixed tab stop (battery-menu style).
        if let Some(x) = style.tab_x {
            let para = NSMutableParagraphStyle::new();
            let opts: Retained<NSDictionary<NSTextTabOptionKey, AnyObject>> = NSDictionary::new();
            // SAFETY: the options generic is the correct (empty) dictionary type.
            let tab = unsafe {
                NSTextTab::initWithTextAlignment_location_options(
                    NSTextTab::alloc(),
                    NSTextAlignment::Right,
                    x,
                    &opts,
                )
            };
            let tabs = NSArray::from_retained_slice(&[tab]);
            para.setTabStops(Some(&tabs));
            // SAFETY: value type matches the paragraph-style attribute key.
            unsafe {
                attr.addAttribute_value_range(
                    NSParagraphStyleAttributeName,
                    &para,
                    NSRange::new(0, full_len),
                );
            }
        }

        // Bold marks either the active account (via header_row) or a section
        // header (via `section_header`). Both use the same appearance.
        if style.bold || style.section_header {
            // 0.0 => default menu font size.
            let font = NSFont::boldSystemFontOfSize(0.0);
            // SAFETY: value type matches the font attribute key.
            unsafe {
                attr.addAttribute_value_range(
                    NSFontAttributeName,
                    &font,
                    NSRange::new(0, full_len),
                );
            }
        }

        // Tint high percentages (amber approaching, red near the wall).
        for &(off, len, sev) in &style.colors {
            if len == 0 || off >= full_len {
                continue;
            }
            let end = (off + len).min(full_len);
            let color = color_for(sev);
            // SAFETY: value type matches the foreground-color attribute key.
            unsafe {
                attr.addAttribute_value_range(
                    NSForegroundColorAttributeName,
                    &color,
                    NSRange::new(off, end - off),
                );
            }
        }

        Retained::into_super(attr)
    }

    /// Style every item whose plain title matches, descending into submenus.
    /// `top_level` is true on the outermost NSMenu only; section-header styles
    /// are suppressed inside submenus so a section header whose plain title
    /// happens to match a submenu row (e.g. the "Claude" row inside the
    /// "Capture current login" submenu) doesn't inherit the bold header style.
    fn walk(menu: &NSMenu, styles: &[RowStyle], top_level: bool) {
        for item in menu.itemArray().iter() {
            let title = item.title().to_string();
            if let Some(style) = styles
                .iter()
                .find(|s| s.plain == title && (top_level || !s.section_header))
            {
                item.setAttributedTitle(Some(&attributed(style)));
            }
            if let Some(sub) = item.submenu() {
                walk(&sub, styles, false);
            }
        }
    }

    // SAFETY: called only on the main thread (the run-loop timer), with a live
    // NSMenu pointer from muda's ns_menu() that the tray keeps retained.
    let menu: &NSMenu = unsafe { &*(ns_menu as *const NSMenu) };
    walk(menu, styles, true);
}

/// A submenu stat line and the span of its percentage (for coloring). Returns
/// `(plain_title, Some((utf16_offset, utf16_len)))`; the offset locates the
/// `NN%` so the native walk can tint just the number.
fn stat_row(label: &str, pct_val: Option<f64>, reset: &str) -> (String, Option<(usize, usize)>) {
    let p = pct(pct_val);
    let plain = if reset.is_empty() {
        format!("{label}  {p}")
    } else {
        format!("{label}  {p}  · resets in {reset}")
    };
    let off = u16len(label) + u16len("  ");
    (plain, Some((off, u16len(&p))))
}

fn stat_item(label: &str, pct_val: Option<f64>, reset: &str) -> MenuItem {
    MenuItem::with_id("noop", stat_row(label, pct_val, reset).0, false, None)
}

/// A stable fingerprint of everything the menu renders. When it's unchanged we
/// skip `set_menu`, so an open menu is never dismissed by a no-op poll.
///
/// Provider id is folded into every account fingerprint so a Codex-side change
/// doesn't collide with a stale Claude signature (or vice versa) once state
/// carries multiple providers.
fn menu_signature(snap: &Snapshot) -> String {
    let mut s = String::new();
    for sec in &snap.sections {
        s.push_str(&format!(
            "SEC[{}={}|sw={}|us={}|env={}|",
            sec.provider_id,
            sec.display_name,
            sec.supports_switching,
            sec.supports_usage,
            sec.env_override_active,
        ));
        for a in &sec.accounts {
            // `a.provider_id` mirrors `sec.provider_id` in v1; folding both
            // keeps the signature honest once state v2 tags each account
            // with its own slug and a mis-bucketed account could exist.
            s.push_str(&format!(
                "{}@{}/{}|{}|{}|",
                a.provider_id, sec.provider_id, a.key, a.active, a.has_data,
            ));
            for w in &a.windows {
                s.push_str(&format!(
                    "{}={}:r={}|",
                    w.id,
                    w.pct.map(|v| v.round() as i64).unwrap_or(-1),
                    w.reset,
                ));
            }
            s.push_str(&format!("u={};", a.updated));
        }
        s.push_str("] ");
    }
    // Capture-submenu contents also affect the redraw: adding a new registered
    // provider (or a change in its installed-probe answer) must show up.
    for reg in &snap.registered {
        s.push_str(&format!("REG[{}|{}|{}] ", reg.provider_id, reg.display_name, reg.installed));
    }
    s.push_str(&format!(
        "as={} th={:.0} li={}",
        snap.autoswap, snap.threshold, snap.start_at_login
    ));
    s
}

fn header_line(a: &AcctView) -> String {
    let (pa, pb) = summary_pcts(a);
    format!("{}  ·  {} / {}", a.display, pct(pa), pct(pb))
}

fn add(menu: &Menu, item: MenuItem) {
    let _ = menu.append(&item);
}

fn add_check(menu: &Menu, item: CheckMenuItem) {
    let _ = menu.append(&item);
}

fn title_for(snap: &Snapshot) -> String {
    match active_account(snap) {
        // Session (5h) matters most day to day; fall back to weekly. Preserves
        // the v1 tray-title semantics — a full weekly can't silently replace
        // the low session number in the menu bar. Multi-window providers still
        // yield a single honest number by leaning on the provider's window
        // ordering (first = session-analog, second = weekly-analog).
        Some((_sec, a)) => {
            let s = a.windows.first().and_then(|w| w.pct);
            let w = a.windows.get(1).and_then(|w| w.pct);
            match s.or(w) {
                Some(p) => format!("{p:.0}%"),
                None => "—".to_string(),
            }
        }
        None => "—".to_string(),
    }
}

fn tooltip_for(snap: &Snapshot) -> String {
    match active_account(snap) {
        // Preserve the v1 tooltip format verbatim: `email — session X, weekly Y`.
        // Multi-window providers still project onto the first two windows
        // (session-analog / weekly-analog) so the tooltip stays a stable
        // one-liner regardless of how many windows the provider carries.
        Some((_sec, a)) => {
            let s = a.windows.first().and_then(|w| w.pct);
            let w = a.windows.get(1).and_then(|w| w.pct);
            format!(
                "{} — session {}, weekly {}",
                a.display,
                pct(s),
                pct(w)
            )
        }
        None => "claude-usage: no active account".to_string(),
    }
}

fn pct(p: Option<f64>) -> String {
    p.map(|v| format!("{v:.0}%"))
        .unwrap_or_else(|| "—".to_string())
}

// ---------------------------------------------------------------------------
// Click handling
// ---------------------------------------------------------------------------

/// Parsed click id: `action[:slug[:key]]`. `slug`/`key` are `None` for global
/// actions (`quit`, `autoswap:off`, `autoswap:95`, `autoswap:now`, `noop`,
/// `startlogin`, `capture` — the plain-capture id used only by the submenu
/// title itself, never a click).
struct ClickId<'a> {
    action: &'a str,
    slug: Option<&'a str>,
    key: Option<&'a str>,
}

fn parse_click_id(id: &str) -> ClickId<'_> {
    let mut it = id.splitn(3, ':');
    let action = it.next().unwrap_or("");
    let slug = it.next();
    let key = it.next();
    ClickId { action, slug, key }
}

fn handle_click(id: &str) {
    let c = parse_click_id(id);
    // Actions only mutate state.json; the main-thread timer re-renders from it
    // within ~1s without any network call.
    match (c.action, c.slug, c.key) {
        ("quit", _, _) => std::process::exit(0),
        ("noop", _, _) => {}
        ("startlogin", _, _) => toggle_login_item(),
        ("autoswap", Some("off"), None) => set_autoswap(false),
        ("autoswap", Some("now"), None) => match optimize_now() {
            Ok(Some(email)) => notify(&format!("Switched to {email}")),
            Ok(None) => notify("Already on the best account"),
            Err(e) => notify(&format!("Optimize failed: {e}")),
        },
        ("autoswap", Some(t), None) => {
            if let Ok(v) = t.parse::<f64>() {
                set_autoswap_threshold(v);
            }
        }
        ("capture", Some(slug), None) => handle_capture(slug),
        ("switch", Some(slug), Some(key)) => handle_switch(slug, key),
        ("remove", Some(slug), Some(key)) => handle_remove(slug, key),
        ("launch", Some(slug), Some(key)) => handle_launch(slug, key),
        // Ignore unrecognized ids (e.g. the top-level "capture" submenu title
        // or a future action added by a later phase we don't yet handle).
        _ => {}
    }
}

/// Capture the current login for `slug`. For Claude, use the full v1 flow
/// (persists into state.json + preserves cached usage). For every other
/// provider, dispatch through the trait — the result is only surfaced via
/// notification since v1 state has no bucket to persist non-Claude accounts.
fn handle_capture(slug: &str) {
    if slug == CLAUDE_SLUG {
        match capture_current() {
            Ok((email, existed)) => notify(&format!(
                "{} {email}",
                if existed { "Refreshed" } else { "Captured" }
            )),
            Err(e) => notify(&format!("Capture failed: {e}")),
        }
        return;
    }
    let Some(provider) = providers::get(slug) else {
        notify(&format!("Capture failed: provider '{slug}' is not registered"));
        return;
    };
    match provider.capture_current_login() {
        Ok(Some(_)) => notify(&format!(
            "Captured {} account (persistence lands in a later phase)",
            provider.display_name()
        )),
        Ok(None) => notify(&format!("{} — nothing to capture", provider.display_name())),
        Err(e) => notify(&format!("Capture failed: {e}")),
    }
}

fn handle_switch(slug: &str, key: &str) {
    // v1 state only knows Claude accounts; a switch on any other slug can't
    // be persisted yet, so gate on Claude and route to the shared free
    // function that already knows the v1 identity/keychain dance.
    if slug != CLAUDE_SLUG {
        notify(&format!("Switching is not yet supported for {slug}"));
        return;
    }
    match switch_to(key) {
        Ok(label) => notify(&format!("Switched to {label}")),
        Err(e) => notify(&format!("Switch failed: {e}")),
    }
}

fn handle_remove(slug: &str, key: &str) {
    if slug != CLAUDE_SLUG {
        notify(&format!("Remove is not yet supported for {slug}"));
        return;
    }
    if !confirm(&format!("Remove account {key}? This cannot be undone.")) {
        return;
    }
    match remove_account(key) {
        Ok(_) => notify(&format!("Removed {key}")),
        Err(e) => notify(&format!("Remove failed: {e}")),
    }
}

/// Launch the vendor CLI for `slug`. Providers whose `launch_client` returns
/// `Unsupported` (the trait default) surface that as a notification rather
/// than silently doing nothing.
///
/// The launch is dispatched onto a background thread — the Claude
/// implementation calls `Command::status()` (synchronous wait on the child)
/// and every click is drained inside the main-thread NSTimer tick, so calling
/// it inline would freeze the entire menu-bar UI (no ticks, no redraws, no
/// clicks) until the launched `claude` exits, which under a menu-bar app with
/// no controlling TTY is effectively indefinite.
fn handle_launch(slug: &str, _key: &str) {
    let Some(provider) = providers::get(slug) else {
        notify(&format!("Launch failed: provider '{slug}' is not registered"));
        return;
    };
    // `providers::get` returns `&'static dyn Provider`; the trait is `Send +
    // Sync + 'static`, so the reference is trivially safe to move.
    std::thread::spawn(move || {
        if let Err(e) = provider.launch_client(crate::providers::LaunchMode::Continue) {
            notify(&format!("Launch failed: {e}"));
        }
    });
}

/// Enable or disable auto-swap. Surfaces a save failure so the menu checkmark
/// and on-disk state can't silently disagree.
fn set_autoswap(enabled: bool) {
    let r = with_state_lock(|| {
        let mut st = State::load()?;
        st.autoswap_disabled = !enabled;
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save auto-swap setting: {e}"));
    }
}

/// Set the swap threshold AND enable auto-swap.
fn set_autoswap_threshold(v: f64) {
    let r = with_state_lock(|| {
        let mut st = State::load()?;
        st.trigger_pct = Some(v);
        st.autoswap_disabled = false;
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save threshold: {e}"));
    }
}

/// A native confirm dialog; true only if the user clicks the destructive button.
fn confirm(question: &str) -> bool {
    let script = format!(
        "display dialog {question:?} buttons {{\"Cancel\", \"Remove\"}} \
         default button \"Cancel\" with title \"claude-usage\""
    );
    match std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).contains("Remove"),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Start-at-login (System Events Login Item)
// ---------------------------------------------------------------------------

/// Path to point the Login Item at: the .app bundle if we're inside one,
/// otherwise the bare binary.
fn app_path() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return String::new();
    };
    let s = exe.to_string_lossy().to_string();
    match s.find(".app/Contents/MacOS/") {
        Some(idx) => s[..idx + 4].to_string(), // keep through ".app"
        None => s,
    }
}

fn login_item_enabled() -> bool {
    let out = std::process::Command::new("osascript")
        .arg("-e")
        .arg("tell application \"System Events\" to get the name of every login item")
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).contains(LOGIN_ITEM_NAME),
        _ => false,
    }
}

fn toggle_login_item() {
    // Probe fresh here (a click is rare) so we don't act on a stale cached state.
    let currently_on = login_item_enabled();
    let (script, desired) = if currently_on {
        (
            format!("tell application \"System Events\" to delete login item {LOGIN_ITEM_NAME:?}"),
            false,
        )
    } else {
        let path = app_path();
        if path.is_empty() {
            notify("Could not determine the app path for launch-at-login");
            return;
        }
        // {name:?}/{path:?} use Rust's Debug quoting so a name or path containing
        // a quote/backslash can't break out of the AppleScript string literal.
        (
            format!(
                "tell application \"System Events\" to make login item at end with properties \
                 {{name:{LOGIN_ITEM_NAME:?}, path:{path:?}, hidden:true}}"
            ),
            true,
        )
    };
    match std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()
    {
        Ok(s) if s.success() => set_login_item_cache(desired),
        _ => notify(
            "Could not change launch-at-login (grant Automation access to System Events in \
             System Settings › Privacy & Security)",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bands() -> SeverityBands {
        SeverityBands {
            amber: 80.0,
            red: 95.0,
        }
    }

    fn acct(email: &str, session: Option<f64>, weekly: Option<f64>, active: bool) -> AcctView {
        AcctView {
            provider_id: CLAUDE_SLUG,
            key: email.to_string(),
            display: email.to_string(),
            windows: vec![
                WindowView {
                    id: "session".into(),
                    label: "5h".into(),
                    pct: session,
                    reset: "3h".into(),
                },
                WindowView {
                    id: "weekly".into(),
                    label: "7d".into(),
                    pct: weekly,
                    reset: "2d".into(),
                },
            ],
            updated: "1m ago".into(),
            active,
            has_data: true,
        }
    }

    fn one_section_snap(a: AcctView) -> Snapshot {
        Snapshot {
            sections: vec![ProviderSection {
                provider_id: CLAUDE_SLUG,
                display_name: "Claude",
                supports_switching: true,
                supports_usage: true,
                severity_bands: bands(),
                env_override_active: false,
                accounts: vec![a],
            }],
            registered: vec![RegisteredProvider {
                provider_id: CLAUDE_SLUG,
                display_name: "Claude",
                installed: true,
            }],
            autoswap: false,
            threshold: 95.0,
            start_at_login: false,
        }
    }

    #[test]
    fn severity_bands_defaults() {
        let b = bands();
        assert!(severity_with(None, b).is_none());
        assert!(severity_with(Some(0.0), b).is_none());
        assert!(severity_with(Some(79.9), b).is_none());
        assert_eq!(severity_with(Some(80.0), b), Some(Severity::Amber));
        assert_eq!(severity_with(Some(94.9), b), Some(Severity::Amber));
        assert_eq!(severity_with(Some(95.0), b), Some(Severity::Red));
        assert_eq!(severity_with(Some(100.0), b), Some(Severity::Red));
    }

    #[test]
    fn severity_bands_are_provider_driven() {
        // A provider with different thresholds (50/75) colors the same pct
        // differently — proving the hardcoded 80/95 are gone.
        let b = SeverityBands {
            amber: 50.0,
            red: 75.0,
        };
        assert_eq!(severity_with(Some(60.0), b), Some(Severity::Amber));
        assert_eq!(severity_with(Some(80.0), b), Some(Severity::Red));
    }

    // For ASCII rows, UTF-16 offsets equal byte offsets, so we can slice the
    // plain title to prove each colored span lands exactly on its `NN%`.
    fn span_text(plain: &str, off: usize, len: usize) -> String {
        plain.chars().skip(off).take(len).collect()
    }

    #[test]
    fn header_row_colors_land_on_percentages() {
        let a = acct("you@work.com", Some(82.0), Some(96.0), true);
        let r = header_row(&a, bands());
        assert_eq!(r.plain, "you@work.com\t82% / 96%");
        assert!(r.bold, "active account is bold");
        assert_eq!(r.tab_x, Some(TAB_X), "trailing run is right-aligned");
        assert_eq!(r.colors.len(), 2);
        let (so, sl, ss) = r.colors[0];
        assert_eq!(span_text(&r.plain, so, sl), "82%");
        assert_eq!(ss, Severity::Amber);
        let (wo, wl, ws) = r.colors[1];
        assert_eq!(span_text(&r.plain, wo, wl), "96%");
        assert_eq!(ws, Severity::Red);
    }

    #[test]
    fn header_row_low_usage_has_no_colors_and_no_bold_when_inactive() {
        let a = acct("dev@side.com", Some(3.0), Some(9.0), false);
        let r = header_row(&a, bands());
        assert!(!r.bold);
        assert!(r.colors.is_empty());
    }

    #[test]
    fn header_row_offsets_hold_for_unicode_email() {
        let a = acct("café@x.com", Some(99.0), None, false);
        let r = header_row(&a, bands());
        let (off, len, _) = r.colors[0];
        let utf16: Vec<u16> = r.plain.encode_utf16().collect();
        let picked = String::from_utf16(&utf16[off..off + len]).unwrap();
        assert_eq!(picked, "99%");
    }

    #[test]
    fn top_header_row_colors_land_on_percentages() {
        let a = acct("you@work.com", Some(50.0), Some(88.0), true);
        let r = top_header_row(&a, bands());
        assert_eq!(r.plain, "you@work.com  ·  50% / 88%");
        assert!(!r.bold, "top info line is not bold");
        assert_eq!(r.tab_x, None);
        assert_eq!(r.colors.len(), 1);
        let (o, l, s) = r.colors[0];
        assert_eq!(span_text(&r.plain, o, l), "88%");
        assert_eq!(s, Severity::Amber);
    }

    #[test]
    fn stat_row_span_lands_on_percentage() {
        let (plain, span) = stat_row("Session", Some(97.0), "3h");
        assert_eq!(plain, "Session  97%  · resets in 3h");
        let (off, len) = span.unwrap();
        assert_eq!(span_text(&plain, off, len), "97%");
    }

    #[test]
    fn menu_styles_emits_section_header_and_skips_low_stat_rows() {
        let snap = one_section_snap(acct("a@x.com", Some(10.0), Some(20.0), true));
        let styles = menu_styles(&snap);
        // Expect: top header + section header + account header. No stat rows
        // (all under amber band).
        let plains: Vec<&str> = styles.iter().map(|s| s.plain.as_str()).collect();
        assert!(plains.contains(&"Claude"), "section header row present");
        assert!(styles.iter().any(|s| s.section_header && s.plain == "Claude"));
        assert!(
            styles.iter().all(|s| s.colors.is_empty()),
            "all pcts are low → no colored spans"
        );
    }

    #[test]
    fn u16len_counts_surrogate_pairs() {
        assert_eq!(u16len("abc"), 3);
        assert_eq!(u16len("café"), 4); // é is BMP → 1 code unit
        assert_eq!(u16len("a😀b"), 4); // 😀 is astral → 2 code units
    }

    #[test]
    fn header_row_offsets_hold_for_astral_email() {
        let a = acct("😀@x.com", Some(99.0), None, false);
        let r = header_row(&a, bands());
        let (off, len, _) = r.colors[0];
        let utf16: Vec<u16> = r.plain.encode_utf16().collect();
        let picked = String::from_utf16(&utf16[off..off + len]).unwrap();
        assert_eq!(picked, "99%");
    }

    #[test]
    fn launchd_managed_detects_matching_service_name() {
        assert!(launchd_managed_from_env(Some(crate::LAUNCHD_LABEL)));
        assert!(!launchd_managed_from_env(Some("com.other.service")));
        assert!(!launchd_managed_from_env(None));
    }

    #[test]
    fn menu_signature_changes_when_a_window_reset_changes() {
        // A window's reset countdown is folded into the signature so a
        // changing reset triggers a redraw (would otherwise leave stale text).
        let mut a = acct("you@work.com", Some(50.0), Some(60.0), true);
        a.windows.push(WindowView {
            id: "opus".into(),
            label: "Opus 7d".into(),
            pct: Some(30.0),
            reset: "3h".into(),
        });
        let base = one_section_snap(a);
        let sig1 = menu_signature(&base);
        let mut changed = base;
        changed.sections[0].accounts[0].windows[2].reset = "2h".into();
        assert_ne!(sig1, menu_signature(&changed));
    }

    #[test]
    fn menu_signature_folds_provider_id() {
        // Same account, same numbers, different provider slug → different
        // signature. Guards against a cross-provider menu change being
        // swallowed by a stale Claude-shaped fingerprint.
        let mut base = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
        let sig1 = menu_signature(&base);
        // Simulate the same-shaped row landing under a different provider.
        base.sections[0].provider_id = "codex";
        base.sections[0].accounts[0].provider_id = "codex";
        assert_ne!(sig1, menu_signature(&base));
    }

    #[test]
    fn parse_click_id_splits_action_slug_key() {
        let c = parse_click_id("switch:claude:matt@example.com");
        assert_eq!(c.action, "switch");
        assert_eq!(c.slug, Some("claude"));
        assert_eq!(c.key, Some("matt@example.com"));

        let c = parse_click_id("capture:codex");
        assert_eq!(c.action, "capture");
        assert_eq!(c.slug, Some("codex"));
        assert!(c.key.is_none());

        let c = parse_click_id("autoswap:off");
        assert_eq!(c.action, "autoswap");
        assert_eq!(c.slug, Some("off"));

        let c = parse_click_id("quit");
        assert_eq!(c.action, "quit");
        assert!(c.slug.is_none());
    }

    // --- env-override guard end-to-end -----------------------------------

    #[test]
    fn env_override_for_reads_shared_hook_and_row_flows_into_menu_signature() {
        // (a) With no override, the Claude section's `env_override_active`
        // stays false and the menu signature reflects "env=false".
        assert!(!env_override_for(CLAUDE_SLUG));

        let mut snap = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
        assert!(!snap.sections[0].env_override_active);
        let sig_off = menu_signature(&snap);

        // (b) Toggle the env override on for the Claude slug via the shared
        // hook. `env_override_for` — the same function `build_snapshot`
        // consults to populate each section's flag — must now report true,
        // and a section rebuilt with that flag must produce a different
        // `menu_signature` (so the tray redraws and shows the disabled row).
        crate::with_env_override_hook(&[CLAUDE_SLUG], || {
            assert!(env_override_for(CLAUDE_SLUG));
            assert!(!env_override_for("codex"));
            snap.sections[0].env_override_active = env_override_for(CLAUDE_SLUG);
        });

        assert!(snap.sections[0].env_override_active);
        assert_ne!(
            sig_off,
            menu_signature(&snap),
            "env-override flag must fold into the signature so the menu redraws with the disabled row",
        );
        // The redraw signal includes the exact override state so a change
        // from active→inactive is not swallowed either.
        assert!(menu_signature(&snap).contains("env=true"));
    }

    #[test]
    fn section_renders_env_override_row_when_flagged() {
        // A section without the override contributes no extra header rows.
        let base = one_section_snap(acct("a@x.com", Some(10.0), Some(20.0), true));
        assert!(section_headline_rows(&base.sections[0]).is_empty());

        // With the override on, the section prepends the disabled row that
        // `build_menu` adds verbatim (`ENV_OVERRIDE_ROW_TITLE`). muda's Menu
        // requires the main thread on macOS, so we assert on the pure
        // helper `build_menu` shares with us instead of building the menu.
        let mut flagged = one_section_snap(acct("a@x.com", Some(10.0), Some(20.0), true));
        flagged.sections[0].env_override_active = true;
        let rows = section_headline_rows(&flagged.sections[0]);
        assert_eq!(rows, vec![ENV_OVERRIDE_ROW_TITLE]);
    }

    #[test]
    fn active_account_looks_across_sections() {
        // Two sections, active row lives in the second — active_account must
        // still find it (it's the anchor for the top header + title).
        let mut a = acct("dev@x.com", Some(10.0), Some(20.0), false);
        a.active = false;
        let mut b = acct("work@x.com", Some(50.0), Some(60.0), true);
        b.provider_id = "codex";
        let snap = Snapshot {
            sections: vec![
                ProviderSection {
                    provider_id: CLAUDE_SLUG,
                    display_name: "Claude",
                    supports_switching: true,
                    supports_usage: true,
                    severity_bands: bands(),
                    env_override_active: false,
                    accounts: vec![a],
                },
                ProviderSection {
                    provider_id: "codex",
                    display_name: "Codex",
                    supports_switching: true,
                    supports_usage: true,
                    severity_bands: bands(),
                    env_override_active: false,
                    accounts: vec![b],
                },
            ],
            registered: Vec::new(),
            autoswap: false,
            threshold: 95.0,
            start_at_login: false,
        };
        let (sec, acc) = active_account(&snap).expect("active row found");
        assert_eq!(sec.provider_id, "codex");
        assert_eq!(acc.key, "work@x.com");
    }
}
