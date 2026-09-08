//! macOS menu-bar app: shows the active account's usage in the status bar and
//! lets you switch / capture / remove accounts and set the auto-swap threshold
//! from a dropdown. The same `watch_cycle` that powers `usagio watch` runs
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
use chrono::{DateTime, Utc};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::NSTimer;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::TrayIconBuilder;

use crate::countdown::{self, AccountUsage, BlockingWindow, DisplayState};
use crate::providers::{self, CaptureMode, Provider, SeverityBands};
use crate::store::State;
use crate::{
    age_str, capture_current, env_override_active, menu_order, next_interval, notify, optimize_now,
    remove_account, row_from_account, switch_to, watch_cycle, with_state_lock, Row, SwapGuard,
    CLAUDE_SLUG, TARGET_CEILING_PCT, TRIGGER_PCT, WATCH_INTERVAL_SECS,
};

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
    /// Absolute reset instants for the first two windows, threaded through so
    /// `countdown::compute_display` can decide whether the row is "locked"
    /// (session/weekly ≥99.5% and reset still in the future). The strings in
    /// `WindowView.reset` are for display only — the raw `DateTime` is needed
    /// to reason about the future.
    session_reset_at: Option<DateTime<Utc>>,
    weekly_reset_at: Option<DateTime<Utc>>,
}

/// One provider's block in the menu. Rendered only if `accounts` is non-empty
/// (per the "section renders only when the provider has at least one captured
/// account" rule).
pub(crate) struct ProviderSection {
    provider_id: &'static str,
    display_name: &'static str,
    supports_switching: bool,
    /// Gates the "Launch client" row (H3, v0.5.0 codeaudit). Independent of
    /// `supports_switching` — a provider can have one wired without the
    /// other; the row must not appear (and error on click) for a provider
    /// whose `launch_client` is still the `Unsupported` trait default.
    supports_launch: bool,
    /// Gates the "Remove…" row (H3, v0.5.0 codeaudit). Defaults `true` for
    /// every current provider — removing a captured account is a generic
    /// state.json operation — but stays explicit so a provider needing extra
    /// cleanup can opt out until that's wired.
    supports_remove: bool,
    supports_usage: bool,
    severity_bands: SeverityBands,
    /// True when the provider's OAuth env-override is set on this process's
    /// environment. Surfaced as a disabled row inside the section and used by
    /// `watch_cycle` to skip that provider entirely.
    env_override_active: bool,
    accounts: Vec<AcctView>,
}

/// One row in the "Capture current login ▸" submenu (or its "Paste API key ▸"
/// sub-submenu). `installed` is a best-effort probe used to grey out rows
/// whose credential store isn't present on this host; the row stays clickable
/// so errors surface honestly. `capture_mode` decides which bucket the row
/// belongs to — filtered upstream by `capture_menu_providers`.
struct RegisteredProvider {
    provider_id: &'static str,
    display_name: &'static str,
    installed: bool,
    /// Kept for click-handling code that will need to route API-key rows to
    /// the paste-a-key prompt instead of `Provider::capture_current_login`.
    /// Also lets the redraw signature distinguish a provider whose capture
    /// mode changed even if its slug and display name didn't.
    #[allow(dead_code)]
    capture_mode: CaptureMode,
}

/// Everything the UI needs to render, produced by the poller thread.
#[derive(Default)]
struct Snapshot {
    sections: Vec<ProviderSection>,
    /// Providers whose `capture_mode == CredsOnDisk` and `supports_usage == true`:
    /// rendered directly under "Capture current login ▸".
    capture_creds: Vec<RegisteredProvider>,
    /// Providers whose `capture_mode == ApiKey` and `supports_usage == true`:
    /// rendered under the "Paste API key ▸" sub-submenu of "Capture current login ▸".
    capture_api_key: Vec<RegisteredProvider>,
    autoswap: bool,
    threshold: f64,
    /// Settings ▸ Notifications ▸ per-trigger enable checkboxes.
    notification_config: crate::notifications::NotificationConfig,
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
    /// If set, attach the 16px provider icon (looked up by slug in
    /// `crate::icons::png16_for`) to the native menu item via `setImage:`. Only
    /// set on section-header rows so per-provider iconography appears once at
    /// the top of each section. A missing slug (no bundled PNG) is a no-op.
    icon_slug: Option<&'static str>,
    /// If true, mark the native menu item as `NSControlStateValueOn` so a
    /// leading checkmark glyph appears — the "✓ Active" affordance for the
    /// active account row without changing the plain title text.
    checkmark: bool,
    /// If set, paint the run from this UTF-16 offset to the end of the row in
    /// `NSColor::secondaryLabelColor` (macOS "grey secondary" — the same tint
    /// disabled menu items use). Used to render "usagio vX.Y.Z" as a subdued
    /// trailing label on the enabled Quit row: right-aligned via `tab_x`,
    /// grey via this field, without disabling the row's click.
    grey_tail_from: Option<usize>,
    /// v0.5.0: paint the WHOLE row in `NSColor::labelColor()` (the normal
    /// enabled-text color — white in dark mode, black in light) even though
    /// the underlying muda item is `enabled: true`. Used for the reset-window
    /// / model-breakdown informational rows inside an account submenu: they
    /// must read as normal text (not the greyed "disabled" look `enabled:
    /// false` produces) while still routing clicks to a no-op (see the
    /// `("noop", _, _)` arm in `handle_click`) since there's nothing to do
    /// when you click "Session resets in 3h".
    disabled_but_white: bool,
}

impl RowStyle {
    /// A row with no special styling beyond its plain title — construction
    /// sites override just the fields they need via struct-update syntax so
    /// adding a new `RowStyle` field never requires touching every call site.
    fn plain_row(plain: String) -> RowStyle {
        RowStyle {
            plain,
            bold: false,
            section_header: false,
            colors: Vec::new(),
            tab_x: None,
            icon_slug: None,
            checkmark: false,
            grey_tail_from: None,
            disabled_but_white: false,
        }
    }
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

/// Fold an `AcctView` into the shape `countdown::compute_display` expects —
/// pcts from the first two windows plus the raw reset instants we captured off
/// the row. Used by both `main_row` and `switch_target_lock_countdown` so a
/// row that switches to "locked · Xd Yh" agrees with the switch-refusal path.
fn account_usage_for(a: &AcctView) -> AccountUsage {
    let (sp, wp) = summary_pcts(a);
    AccountUsage {
        session_pct: sp,
        session_reset: a.session_reset_at,
        weekly_pct: wp,
        weekly_reset: a.weekly_reset_at,
    }
}

/// If the account is "locked" (session or weekly at ≥99.5% with a
/// still-future reset), return the human countdown string — the piece that
/// swaps in for `S% / W%` in the row title. `None` otherwise.
fn locked_countdown_for(a: &AcctView, now: DateTime<Utc>) -> Option<(String, BlockingWindow)> {
    match countdown::compute_display(&account_usage_for(a), now) {
        DisplayState::Locked { until, window } => {
            Some((countdown::format_countdown(until - now), window))
        }
        DisplayState::Usage { .. } => None,
    }
}

/// Wall-clock "now" used by the render helpers. Overridable in tests so a
/// pinned locked-row can be asserted without waiting real hours.
#[cfg(not(test))]
fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
fn now_utc() -> DateTime<Utc> {
    tests::test_now()
}

/// Left column width (chars) the provider name is padded to in the flat main
/// list row, so every account's email starts at the same x regardless of
/// provider-name length ("Claude" vs "Codex").
const PROVIDER_COL: usize = 10;

/// v0.5.0 flat main-list row: `{provider}    {email}\tS {n}%  W {n}%`, bold +
/// checkmarked if active, high percentages colored per the provider's
/// severity bands. When the account is fully consumed
/// (`countdown::compute_display` → Locked), the `S n%  W n%` run is swapped
/// for `locked · <countdown>` and colored red — the user is being told *when*
/// the account is next usable, not *how used* it is. This same string is both
/// the row's `RowStyle` (for `apply_menu_styles`) and the plain title of the
/// top-level `Submenu` `build_account_submenu` builds for the account (the
/// row IS the submenu trigger — there's no separate provider header row).
fn main_row(provider_display: &str, a: &AcctView, bands: SeverityBands) -> RowStyle {
    let label = format!("{provider_display:<PROVIDER_COL$}{}", a.display);
    let base = u16len(&label) + 1; // + '\t'
    if let Some((cd, _win)) = locked_countdown_for(a, now_utc()) {
        // UX (v0.5.0): drop the "locked · " prefix — the trailing run is
        // rendered in red (visually implying locked) and the payload is a
        // time-until-reset instead of a percentage (structurally implying
        // locked, since a healthy row shows "S n%  W n%"). The old
        // "locked · Xh Ym" wording repeated the same fact three ways.
        let trailing = cd.clone();
        let plain = format!("{label}\t{trailing}");
        // A "locked" account is by definition red — no need to consult bands.
        let colors = vec![(base, u16len(&trailing), Severity::Red)];
        return RowStyle {
            bold: a.active,
            colors,
            tab_x: Some(TAB_X),
            checkmark: a.active,
            ..RowStyle::plain_row(plain)
        };
    }
    let (pa, pb) = summary_pcts(a);
    let sa = pct(pa);
    let sb = pct(pb);
    let trailing = format!("S {sa}  W {sb}");
    let plain = format!("{label}\t{trailing}");
    let mut colors = Vec::new();
    let s_off = base + u16len("S ");
    if let Some(sev) = severity_with(pa, bands) {
        colors.push((s_off, u16len(&sa), sev));
    }
    let w_off = s_off + u16len(&sa) + u16len("  W ");
    if let Some(sev) = severity_with(pb, bands) {
        colors.push((w_off, u16len(&sb), sev));
    }
    RowStyle {
        bold: a.active,
        colors,
        tab_x: Some(TAB_X),
        checkmark: a.active,
        ..RowStyle::plain_row(plain)
    }
}

/// All styling directives for the current menu, derived from the same snapshot
/// `build_menu` renders. The native walk applies each by matching `.plain`.
/// Plain title used both when we build the Quit row and when the style
/// walker matches it. Keep the two in lockstep — a mismatch (e.g. a
/// forgotten `\t`) means the walker finds no match and the item renders
/// as plain "Quit<tab>usagio vX.Y.Z" without alignment or grey.
fn quit_row_plain() -> String {
    format!("Quit\tusagio v{}", env!("CARGO_PKG_VERSION"))
}

fn menu_styles(snap: &Snapshot) -> Vec<RowStyle> {
    let mut styles = Vec::new();
    // v0.5.0: the top "resets in X" header rows are gone (item 1 of the menu
    // redesign) and there's no separate disabled provider-header row either —
    // the provider name now lives inline in each account's own row (see
    // `main_row`). Every row carries its provider's 16px icon (looked up by
    // slug in `crate::icons::png16_for`; a missing PNG is a no-op).
    for sec in &snap.sections {
        for a in &sec.accounts {
            let mut style = main_row(sec.display_name, a, sec.severity_bands);
            style.icon_slug = Some(sec.provider_id);
            styles.push(style);
            // Submenu info rows (reset windows, burn-rate/cost, "updated …")
            // render enabled but must read as normal (not disabled-grey)
            // text — see `disabled_but_white` and `submenu_info_rows`.
            for row in submenu_info_rows(sec, a) {
                styles.push(RowStyle {
                    disabled_but_white: true,
                    ..RowStyle::plain_row(row)
                });
            }
        }
    }
    // Quit row: "Quit\tusagio vX.Y.Z" — right-align the trailing run at
    // TAB_X and paint everything from the tab onward in secondaryLabelColor
    // (macOS's disabled-text grey) while the row itself stays clickable.
    let quit_plain = quit_row_plain();
    let grey_from = u16len("Quit") + 1; // +1 for the '\t'
    styles.push(RowStyle {
        tab_x: Some(TAB_X),
        grey_tail_from: Some(grey_from),
        ..RowStyle::plain_row(quit_plain)
    });
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
        let (rate_limited, max_session_pct, trigger) = run_cycle(&mut guard);
        let prev = current;
        current = next_interval(current, base, rate_limited, max_session_pct, trigger);
        if rate_limited {
            crate::logging::log(&format!("rate limited; backing off to {current}s"));
        } else if current != prev && current < base {
            crate::logging::log(&format!(
                "cadence: {prev}s → {current}s (max session {:.1}%, trigger {:.0}%)",
                max_session_pct.unwrap_or(0.0),
                trigger
            ));
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
    let target = format!("gui/{uid}/{}", crate::AUTOSTART_LABEL);
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
/// to the job label for a LaunchAgent, so a manual `usagio menubar` run
/// (which self-spawns fine) isn't misrouted to the kickstart path.
fn is_launchd_managed() -> bool {
    launchd_managed_from_env(std::env::var("XPC_SERVICE_NAME").ok().as_deref())
}

/// Pure predicate behind `is_launchd_managed`, split out for testing.
fn launchd_managed_from_env(xpc_service_name: Option<&str>) -> bool {
    xpc_service_name == Some(crate::AUTOSTART_LABEL)
}

/// Run one poll+auto-swap cycle; returns whether it was rate limited.
/// Runs one poll cycle and returns `(rate_limited, max_session_pct, trigger)`
/// so the caller's adaptive-cadence math has everything it needs. The
/// menubar poller uses the trigger the user actually configured (via
/// `Settings ▸ Auto-swap threshold`), matching what `watch_cycle` itself
/// dispatched on.
fn run_cycle(guard: &mut SwapGuard) -> (bool, Option<f64>, f64) {
    let st = State::load().unwrap_or_default();
    let autoswap = !st.autoswap_disabled;
    let threshold = st.trigger_pct.unwrap_or(TRIGGER_PCT);
    // With auto-swap off, use an unreachable trigger so we only observe.
    let trigger = if autoswap { threshold } else { 101.0 };
    match watch_cycle(trigger, TARGET_CEILING_PCT, guard) {
        Ok(o) => (o.rate_limited, o.max_session_pct, trigger),
        Err(e) => {
            crate::logging::log(&format!("menubar poll failed: {e}"));
            (false, None, trigger)
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

/// Compare two `AcctView` values by their weekly-reset instant, treating a
/// `None` reset as "furthest in the future" so accounts without data yet sort
/// last. Used inside `build_snapshot` and asserted directly in the
/// `sort_by_expiration_orders_accounts_soonest_first` test.
fn sort_by_expiration(a: &AcctView, b: &AcctView) -> std::cmp::Ordering {
    let ka = a.weekly_reset_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
    let kb = b.weekly_reset_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
    ka.cmp(&kb)
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
        session_reset_at: r.session.resets_at,
        weekly_reset_at: r.weekly.resets_at,
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
        let provider_rows: Vec<&Row> = rows.iter().filter(|r| r.provider_id == slug).collect();
        if provider_rows.is_empty() {
            continue; // no captured accounts → no section (no header, no rows).
        }
        let mut accounts: Vec<AcctView> = provider_rows
            .into_iter()
            .map(|r| acctview_from_row(r, &active, slug, provider.window_order()))
            .collect();
        // Primary sort: soonest-to-expire first, using the weekly-reset instant
        // as the "expiration" signal (accounts with no data yet sort last).
        // Without this, a newly-captured account lands at the tail of the vec
        // (State::upsert appends) and stays there in the menu — the "sort-by-
        // expiration on add" bug the user reported.
        accounts.sort_by(sort_by_expiration);
        // Flat-list rule: within a provider section the active account renders
        // first, then everyone else in the order picked above. Stable sort so
        // the expiration ordering is preserved among the inactives.
        accounts.sort_by_key(|a| std::cmp::Reverse(a.active));
        let caps = provider.capabilities();
        sections.push(ProviderSection {
            provider_id: slug,
            display_name: provider.display_name(),
            supports_switching: caps.supports_switching,
            supports_launch: caps.supports_launch,
            supports_remove: caps.supports_remove,
            supports_usage: caps.supports_usage,
            severity_bands: provider.severity_bands(),
            env_override_active: env_override_for(slug),
            accounts,
        });
    }

    // The capture submenu lists every registered provider that CAN capture a
    // login on this host — filtered by `capture_menu_providers` so stub
    // providers (`supports_usage == false`) don't clutter the onboarding UX.
    // Providers with `capture_mode == ApiKey` go into the "Paste API key ▸"
    // sub-submenu instead of the main list — UNLESS they already have a
    // captured account (i.e. a section above), in which case that account now
    // renders in the main list and "Paste API key ▸" stops offering it (item
    // 9 of the redesign: capture is for NEW accounts only once one exists).
    let captured_provider_ids: Vec<&str> = sections.iter().map(|s| s.provider_id).collect();
    let (creds_providers, api_key_providers) = capture_menu_providers(&captured_provider_ids);
    let capture_creds: Vec<RegisteredProvider> =
        creds_providers.into_iter().map(register_provider).collect();
    let capture_api_key: Vec<RegisteredProvider> = api_key_providers
        .into_iter()
        .map(register_provider)
        .collect();

    Snapshot {
        sections,
        capture_creds,
        capture_api_key,
        autoswap,
        threshold,
        notification_config: st.notification_config.clone(),
    }
}

/// Build one `RegisteredProvider` row from a live provider reference.
/// Extracted so both buckets in `build_snapshot` populate identically.
fn register_provider(p: &'static dyn Provider) -> RegisteredProvider {
    RegisteredProvider {
        provider_id: p.provider_id(),
        display_name: p.display_name(),
        // v1 only knows how to probe the Claude keychain. Everything else
        // is assumed installed; wrong guesses just surface a real error
        // on the capture attempt instead of pre-emptively greying out.
        installed: probe_installed(p.provider_id()),
        capture_mode: p.capabilities().capture_mode,
    }
}

/// Pick the providers the "Capture current login ▸" menu should offer.
/// Providers with `supports_usage == false` are stubs that have no capture
/// path wired yet (their `capture_current_login` returns `Ok(None)` with a
/// `TODO`) — hiding them keeps the onboarding UX free of dead rows.
///
/// Returns `(creds_on_disk_providers, api_key_providers)`:
/// - the first bucket renders directly under "Capture current login ▸";
/// - the second renders under a "Paste API key ▸" sub-submenu.
///
/// `captured_provider_ids` excludes an API-key provider from the "Paste API
/// key ▸" bucket once it already has a captured account (item 9): that
/// account now renders in the main list instead, and the capture row would
/// just be a confusing duplicate. Creds-on-disk providers (Claude, Codex) are
/// NOT filtered this way — those support multiple captured accounts per
/// provider, so "Capture current login ▸ Claude" must keep working even with
/// an existing Claude account.
///
/// Pure function given its inputs: no I/O, no direct state reads — the caller
/// (`build_snapshot`) is the one that turns `state.json` into
/// `captured_provider_ids`.
pub(crate) fn capture_menu_providers(
    captured_provider_ids: &[&str],
) -> (Vec<&'static dyn Provider>, Vec<&'static dyn Provider>) {
    partition_capture_providers(providers::all(), captured_provider_ids)
}

/// Inner form of `capture_menu_providers` that operates on an arbitrary
/// provider slice so tests can pass their own fixtures without touching the
/// process-wide registry.
fn partition_capture_providers<'a>(
    provs: &'a [Box<dyn Provider>],
    captured_provider_ids: &[&str],
) -> (Vec<&'a dyn Provider>, Vec<&'a dyn Provider>) {
    let mut creds: Vec<&'a dyn Provider> = Vec::new();
    let mut api_key: Vec<&'a dyn Provider> = Vec::new();
    for p in provs {
        let caps = p.capabilities();
        if !caps.supports_usage {
            // Stub provider — no capture path wired yet; hide from the menu.
            continue;
        }
        match caps.capture_mode {
            CaptureMode::CredsOnDisk => creds.push(&**p),
            CaptureMode::ApiKey => {
                if !captured_provider_ids.contains(&p.provider_id()) {
                    api_key.push(&**p);
                }
            }
        }
    }
    (creds, api_key)
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

// The old "Backup Config ▸ Restore from backup ▸ <list>" submenu (and its
// `cached_backups()` mtime-cached directory listing) was replaced in v0.5.0
// by native Save…/Restore… file panels under Settings ▸ Advanced ▸ Backups ▸
// (see `handle_backup_save` / `handle_backup_restore_dialog`). The automatic
// rolling backups `store::list_backups()` enumerates still exist on disk —
// `handle_backup_restore_dialog` just defaults the OPEN panel's directory to
// them instead of rendering a menu row per file.

// (Login-item probe cache removed in 0.4.3 alongside the osascript path —
// see the comment on the removed "Launch at login" menu row for rationale.
// `usagio install` / `usagio uninstall` are the sole autostart entry
// points now, backed by a launchd LaunchAgent plist.)

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

    // v0.5.0 menu redesign: the two "resets in X" status header rows are gone
    // (item 1) — the same numbers now live inline in each account's own row.
    if snap.sections.is_empty() {
        add(
            &menu,
            MenuItem::with_id("none", "Capture a login below to begin", false, None),
        );
    }

    // Flat main list: one top-level Submenu row per (provider, account), no
    // separate provider-header row and no separator between providers — the
    // provider name is folded into each row by `main_row` (item 2). Providers
    // still only contribute rows when they have at least one captured
    // account (the "no header, no rows" rule survives the redesign).
    for sec in &snap.sections {
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
    }
    let _ = menu.append(&PredefinedMenuItem::separator());

    // Auto-swap: a single top-level checkbox now (item 4 of the target tree).
    // The Off/90/95/98/"switch now" picker moves under Settings ▸ Auto-swap
    // threshold ▸ below.
    let _ = menu.append(&CheckMenuItem::with_id(
        "autoswap:toggle",
        "Auto-swap enabled",
        true,
        snap.autoswap,
        None,
    ));
    let _ = menu.append(&PredefinedMenuItem::separator());

    // Capture current login ▸ FIRST, then Settings ▸ — the redesign reorders
    // these (item 4). Creds-on-disk providers render directly; API-key
    // providers roll up under a "Paste API key ▸" sub-submenu, but ONLY while
    // they have no captured account yet (item 9) — once captured, the
    // account itself renders in the main list above instead.
    let capture = Submenu::with_id("capture", "Capture current login", true);
    if snap.capture_creds.is_empty() && snap.capture_api_key.is_empty() {
        let _ = capture.append(&MenuItem::with_id(
            "noop",
            "(no providers registered)",
            false,
            None,
        ));
    } else {
        for reg in &snap.capture_creds {
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
        if !snap.capture_api_key.is_empty() {
            if !snap.capture_creds.is_empty() {
                let _ = capture.append(&PredefinedMenuItem::separator());
            }
            let paste = Submenu::with_id("capture:apikey", "Paste API key", true);
            for reg in &snap.capture_api_key {
                let title = if reg.installed {
                    reg.display_name.to_string()
                } else {
                    format!("{} (not installed)", reg.display_name)
                };
                let _ = paste.append(&MenuItem::with_id(
                    format!("apikey:{}", reg.provider_id),
                    title,
                    true,
                    None,
                ));
            }
            let _ = capture.append(&paste);
        }
    }
    let _ = menu.append(&capture);

    // Settings ▸ Notifications ▸ / Auto-swap threshold ▸ / Advanced ▸.
    let settings = Submenu::with_id("settings", "Settings", true);

    let notifications = Submenu::with_id("settings:notifications", "Notifications", true);
    let _ = notifications.append(&CheckMenuItem::with_id(
        "notifications:threshold",
        "Threshold alerts (70% / 90%)",
        true,
        snap.notification_config.threshold_enabled,
        None,
    ));
    let _ = notifications.append(&CheckMenuItem::with_id(
        "notifications:resetback",
        "Window reset alerts",
        true,
        snap.notification_config.reset_back_enabled,
        None,
    ));
    let _ = notifications.append(&CheckMenuItem::with_id(
        "notifications:pace",
        "Weekly pace projection (experimental)",
        true,
        snap.notification_config.pace_enabled,
        None,
    ));
    let _ = settings.append(&notifications);

    // Auto-swap threshold ▸ — moved wholesale from the old top-level submenu.
    let cur = if snap.autoswap {
        snap.threshold.round() as i32
    } else {
        0
    };
    let threshold = Submenu::with_id("settings:autoswap-threshold", "Auto-swap threshold", true);
    let _ = threshold.append(&CheckMenuItem::with_id(
        "autoswap:off",
        "Off",
        true,
        cur == 0,
        None,
    ));
    for t in [90i32, 95, 98] {
        let _ = threshold.append(&CheckMenuItem::with_id(
            format!("autoswap:{t}"),
            format!("{t}%"),
            true,
            cur == t,
            None,
        ));
    }
    let _ = threshold.append(&PredefinedMenuItem::separator());
    let _ = threshold.append(&MenuItem::with_id(
        "autoswap:now",
        "Switch to best account now",
        true,
        None,
    ));
    let _ = settings.append(&threshold);

    // Advanced ▸ Backups ▸ (Save… / Restore… via native file panels) and
    // Refresh usage now (items 5, 6, 7).
    let advanced = Submenu::with_id("settings:advanced", "Advanced", true);
    let backups = Submenu::with_id("settings:advanced:backups", "Backups", true);
    let _ = backups.append(&MenuItem::with_id("backup:save", "Save…", true, None));
    let _ = backups.append(&MenuItem::with_id("backup:restore", "Restore…", true, None));
    let _ = advanced.append(&backups);
    let _ = advanced.append(&MenuItem::with_id(
        "refresh:now",
        "Refresh usage now",
        true,
        None,
    ));
    let _ = settings.append(&advanced);

    let _ = menu.append(&settings);

    // Context Ledger is intentionally CLI-only (`usagio context`). It was
    // previously a submenu here that shelled out via osascript to a new
    // Terminal window — but that path needed macOS Automation permission
    // that most users hadn't granted, so clicks were doing nothing. The
    // menu's job is account usage, not diagnostics; the CLI stays available
    // for the rare "wait, what's Claude injecting?" moment.

    // The "Launch at login" checkbox used to live here, backed by
    // `osascript -e 'tell application "System Events" to make login item …'`.
    // On every brew upgrade the binary hash changes → macOS treats it as a
    // new app → it re-prompts "usagio wants access to control System
    // Events" on the next poll. That path was redundant with `usagio
    // install` (which registers a proper launchd LaunchAgent), so we removed
    // the toggle in 0.4.3 and rely on the CLI for autostart. To clean out
    // the stale System Events entry a prior version installed, remove
    // "usagio" from System Settings → General → Login Items → Open at Login.

    let _ = menu.append(&PredefinedMenuItem::separator());
    // Quit on the left, version grey + right-aligned on the same row using
    // the same attributedTitle + right tab-stop machinery the account rows
    // already use for `S% / W%`. The style walker matches on the plain
    // title, so QUIT_ROW_PLAIN below MUST equal what we build here.
    add(
        &menu,
        MenuItem::with_id(
            "quit",
            format!("Quit\tusagio v{}", env!("CARGO_PKG_VERSION")),
            true,
            None,
        ),
    );

    menu
}

/// Human-facing "{Label} resets in X" copy for a window row inside an account
/// submenu. Percentages are deliberately NOT shown here (item 3 of the
/// redesign moved them to the main-list row via `main_row`) — this row is
/// purely about *when* the window refreshes.
fn window_reset_row(w: &WindowView) -> String {
    let label = stat_display_label(w);
    if w.reset.is_empty() {
        format!("{label}: no reset info yet")
    } else {
        format!("{label} resets in {}", w.reset)
    }
}

/// Non-clickable informational rows shown inside an account submenu — reset
/// windows, burn-rate / cost estimates, and the "updated Xm ago" footer (or
/// the has_data / no-usage-endpoint fallbacks). Shared by
/// `build_account_submenu` (which renders each as an `enabled: true` `noop`
/// item so it reads as normal text rather than muda's greyed-out disabled
/// look) and `menu_styles` (which marks each `disabled_but_white` so the
/// native walk explicitly paints `NSColor::labelColor()`) — pulling this out
/// keeps the two lists from drifting apart, the same trick `quit_row_plain`
/// uses for the Quit row.
///
/// Deliberately excludes the burn-rate / cost-estimate / "updated" footer
/// rows (`build_account_submenu` appends those separately): those touch the
/// on-disk usage log via `crate::usage_log`, and `menu_styles` — the other
/// caller of this function — must stay pure (no disk I/O) so it's safe to
/// call from a unit test without a `ScopedConfigDir`.
fn submenu_info_rows(sec: &ProviderSection, a: &AcctView) -> Vec<String> {
    let mut rows = Vec::new();
    if sec.supports_usage {
        if a.has_data && !a.windows.is_empty() {
            for w in &a.windows {
                rows.push(window_reset_row(w));
            }
        } else {
            rows.push("no data yet".to_string());
        }
    } else {
        rows.push("(no usage endpoint — headers only)".to_string());
    }
    rows
}

/// Which optional rows `build_account_submenu` should append for `sec`'s
/// account `a`. Pure over `ProviderSection`/`AcctView` fields, with no
/// `muda`/`tray_icon` types involved, so this decision is unit-testable
/// without a live main-thread `NSApplication` (a bare `muda::Menu` can only
/// be constructed on the main thread, which `cargo test`'s worker threads
/// aren't). H3, v0.5.0 codeaudit: `supports_switching` used to be the only
/// gate consulted, for BOTH the Switch and Launch rows, and Remove had no
/// gate at all.
#[derive(Debug, PartialEq, Eq)]
struct AccountSubmenuRows {
    /// `Some(true)` → render "✓ Active" (this account is the active one).
    /// `Some(false)` → render a clickable "Switch to this account" row.
    /// `None` → the provider doesn't support switching; render neither.
    switch_row: Option<bool>,
    launch_row: bool,
    remove_row: bool,
}

fn account_submenu_rows(sec: &ProviderSection, a: &AcctView) -> AccountSubmenuRows {
    AccountSubmenuRows {
        switch_row: sec.supports_switching.then_some(a.active),
        launch_row: sec.supports_launch,
        remove_row: sec.supports_remove,
    }
}

/// Build one account's submenu inside a provider section. Splits Switch /
/// reset-info / Launch / Remove; the pieces vary by capability so a
/// reporting-only provider drops the Switch item and a no-usage provider
/// swaps the info block for a `(no usage endpoint — headers only)` row.
fn build_account_submenu(menu: &Menu, sec: &ProviderSection, a: &AcctView) {
    let head = main_row(sec.display_name, a, sec.severity_bands).plain;
    let sub = Submenu::with_id(format!("sub:{}:{}", sec.provider_id, a.key), head, true);
    let rows = account_submenu_rows(sec, a);
    match rows.switch_row {
        Some(true) => {
            let _ = sub.append(&MenuItem::with_id("noop", "✓ Active", false, None));
        }
        Some(false) => {
            let _ = sub.append(&MenuItem::with_id(
                format!("switch:{}:{}", sec.provider_id, a.key),
                "Switch to this account",
                true,
                None,
            ));
        }
        None => {}
    }
    let _ = sub.append(&PredefinedMenuItem::separator());
    // v0.5.0 (item 3): these rows are informational, not disabled — enabled:
    // true so `apply_menu_styles`'s `disabled_but_white` can paint them in
    // the normal (white/black) text color instead of muda's greyed
    // "disabled" look, while a click still routes to the `noop` no-op.
    for row in submenu_info_rows(sec, a) {
        let _ = sub.append(&MenuItem::with_id("noop", row, true, None));
    }
    // Burn-rate + cost estimator rows sit under the raw window stats, above
    // the "updated" footer. Cheap best-effort reads against the usage log —
    // if we don't have enough samples yet the rows are simply skipped. Not
    // part of `submenu_info_rows` (which `menu_styles` also calls, and must
    // stay disk-I/O-free) since these touch `crate::usage_log` on disk.
    if sec.supports_usage && a.has_data && !a.windows.is_empty() {
        let account_key =
            crate::usage_log::AccountKey::new(sec.provider_id.to_string(), a.key.clone());
        if let Some(est) = crate::burn_rate::estimate(
            &account_key,
            crate::providers::trait_def::Window::Weekly,
            Utc::now(),
        ) {
            if est.confidence >= crate::burn_rate::CONFIDENCE_FLOOR {
                let _ = sub.append(&MenuItem::with_id(
                    "noop",
                    crate::burn_rate::format_menu_row(&est),
                    true,
                    None,
                ));
            }
        }
        if let Some(cost) = crate::cost_tracking::estimate_cycle_cost(
            &account_key,
            crate::cost_tracking::CLAUDE_MAX_100_WEEKLY_TOKENS,
        ) {
            let _ = sub.append(&MenuItem::with_id(
                "noop",
                format!("~${:.2} this cycle (est)", cost.estimated_usd),
                true,
                None,
            ));
        }
        let _ = sub.append(&MenuItem::with_id(
            "noop",
            format!("updated {}", a.updated),
            true,
            None,
        ));
    }
    let _ = sub.append(&PredefinedMenuItem::separator());
    // "Launch" is gated on `supports_launch`, NOT `supports_switching` (H3,
    // v0.5.0 codeaudit): the two are independent — a provider can have
    // `write_active_account` wired (switching) without `launch_client` wired
    // (spawning the vendor CLI), and vice versa. Building this row off
    // `supports_switching` used to expose a "Launch client" row for any
    // switching-capable provider even when its `launch_client` was still the
    // `Unsupported` trait default, so every click errored.
    if rows.launch_row {
        let _ = sub.append(&MenuItem::with_id(
            format!("launch:{}:{}", sec.provider_id, a.key),
            "Launch client",
            true,
            None,
        ));
    }
    // "Remove…" is gated on `supports_remove` (H3, v0.5.0 codeaudit): the
    // row used to be built unconditionally regardless of what the provider
    // actually supports. Every current provider sets `supports_remove:
    // true`, so this is not a behavior change today, but it stops a future
    // provider that needs `supports_remove: false` from getting a row that
    // silently does nothing useful.
    if rows.remove_row {
        let _ = sub.append(&MenuItem::with_id(
            format!("remove:{}:{}", sec.provider_id, a.key),
            "Remove…",
            true,
            None,
        ));
    }
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
    let ns_menu = {
        use tray_icon::menu::ContextMenu;
        menu.ns_menu()
    };
    tray.set_menu(Some(Box::new(menu)));
    apply_menu_styles(ns_menu, &menu_styles(snap));
}

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AllocAnyThread;
use objc2_app_kit::{
    NSColor, NSControlStateValueOn, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
    NSImage, NSMenu, NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSTextAlignment,
    NSTextTab, NSTextTabOptionKey,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSData, NSDictionary, NSMutableAttributedString, NSRange, NSString,
};

fn color_for(sev: Severity) -> Retained<NSColor> {
    match sev {
        Severity::Amber => NSColor::systemOrangeColor(),
        Severity::Red => NSColor::systemRedColor(),
    }
}

/// Build the attributed title for one row from its `RowStyle`. Module-level
/// (not nested in `apply_menu_styles`) so `#[cfg(test)]` can exercise it
/// directly — e.g. asserting `disabled_but_white` produces a `labelColor`
/// foreground-color attribute over the full row.
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

    // Bold marks either the active account (via `main_row`) or a section
    // header (via `section_header`). Both use the same appearance.
    if style.bold || style.section_header {
        // 0.0 => default menu font size.
        let font = NSFont::boldSystemFontOfSize(0.0);
        // SAFETY: value type matches the font attribute key.
        unsafe {
            attr.addAttribute_value_range(NSFontAttributeName, &font, NSRange::new(0, full_len));
        }
    }

    // Optional grey trailing run (Quit row: version rendered in
    // NSColor::secondaryLabelColor — the same tint disabled items use,
    // without actually disabling the row).
    if let Some(from) = style.grey_tail_from {
        if from < full_len {
            let grey = NSColor::secondaryLabelColor();
            // SAFETY: value type matches the foreground-color attribute key.
            unsafe {
                attr.addAttribute_value_range(
                    NSForegroundColorAttributeName,
                    &grey,
                    NSRange::new(from, full_len - from),
                );
            }
        }
    }

    // v0.5.0: force the normal (non-disabled) text color over the WHOLE row
    // for informational submenu rows that are `enabled: true` for
    // click-routing purposes but must not look like muda's greyed-out
    // disabled rows (item 3 of the redesign — "white but not clickable").
    if style.disabled_but_white {
        let normal = NSColor::labelColor();
        // SAFETY: value type matches the foreground-color attribute key.
        unsafe {
            attr.addAttribute_value_range(
                NSForegroundColorAttributeName,
                &normal,
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

/// Walk the native `NSMenu` (and its submenus) and set `attributedTitle` on any
/// item whose plain title matches a `RowStyle` — the mechanism muda's plain
/// string API can't reach (right-aligned tab stops and arbitrary colors).
fn apply_menu_styles(ns_menu: *mut core::ffi::c_void, styles: &[RowStyle]) {
    if ns_menu.is_null() {
        return;
    }

    /// Turn a bundled PNG's bytes into a 16×16 `NSImage`. Nil-safe: a corrupt
    /// or unsupported blob returns `None` (the caller just skips setImage:).
    fn image_from_bytes(bytes: &[u8]) -> Option<Retained<NSImage>> {
        let data = NSData::with_bytes(bytes);
        let img = NSImage::initWithData(NSImage::alloc(), &data)?;
        // Force the drawn size to menu-item height (16pt); PNGs are already
        // 16×16 but `NSImage`'s reported size is 72dpi-scaled, which reads too
        // big at Retina. `usesSize` is not required here — NSMenuItem uses the
        // image's `size` directly.
        use objc2_foundation::NSSize;
        img.setSize(NSSize {
            width: 16.0,
            height: 16.0,
        });
        Some(img)
    }

    /// Style every item whose plain title matches, descending into submenus.
    /// `top_level` is true on the outermost NSMenu only; section-header styles
    /// (unused in v0.5.0's flat main list, but the flag stays load-bearing for
    /// any future disabled/bold top-level row) are suppressed inside submenus
    /// so a plain-title collision with a submenu row doesn't inherit them.
    fn walk(menu: &NSMenu, styles: &[RowStyle], top_level: bool) {
        for item in menu.itemArray().iter() {
            let title = item.title().to_string();
            if let Some(style) = styles
                .iter()
                .find(|s| s.plain == title && (top_level || !s.section_header))
            {
                item.setAttributedTitle(Some(&attributed(style)));
                // Per-provider 16px icon on section header rows. Look up by
                // slug; a missing PNG (or an unknown slug like `vertex-ai`) is
                // a no-op so a future provider without a bundled icon still
                // renders — just text-only.
                if let Some(slug) = style.icon_slug {
                    if let Some(bytes) = crate::icons::png16_for(slug) {
                        if let Some(img) = image_from_bytes(bytes) {
                            item.setImage(Some(&img));
                        }
                    }
                }
                // Leading checkmark glyph for the active-account row (the
                // "✓ trailing glyph" spec). AppKit renders `state == On` as a
                // checkmark in the item's `stateColumn`.
                if style.checkmark {
                    item.setState(NSControlStateValueOn);
                }
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
            // The locked-state trailing text (`locked · Xh Ym`) doesn't come
            // from any window's `w.pct/w.reset` string — it's computed from
            // the reset instants + a wall-clock `now`. Fold the raw instants
            // plus the current `DisplayState` variant into the signature so a
            // usage→locked transition, or a change to *which* window is
            // blocking, triggers a redraw. Instants use RFC3339 for stability.
            let sr = a
                .session_reset_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_default();
            let wr = a
                .weekly_reset_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_default();
            // The `lock` marker alone is not enough: while an account stays
            // Locked the (fixed) reset instant + (fixed) variant produce a
            // static signature for hours, so `install_menu` never re-runs and
            // the header's `locked · Xh Ym` text freezes at whatever value it
            // had when the transition happened. Fold the CURRENT `format_countdown`
            // output into the signature so each minute (or hour, depending on
            // remaining time) the fingerprint changes and the tray redraws with
            // the fresh remaining time. Uses `now_utc()` — the same clock
            // `main_row` renders from.
            let now = now_utc();
            let lock = match countdown::compute_display(&account_usage_for(a), now) {
                DisplayState::Locked {
                    window: BlockingWindow::Session,
                    until,
                } => {
                    format!("L=S|cd={}", countdown::format_countdown(until - now))
                }
                DisplayState::Locked {
                    window: BlockingWindow::Weekly,
                    until,
                } => {
                    format!("L=W|cd={}", countdown::format_countdown(until - now))
                }
                DisplayState::Usage { .. } => "L=0".to_string(),
            };
            s.push_str(&format!(
                "{}@{}/{}|{}|{}|sr={}|wr={}|{}|",
                a.provider_id, sec.provider_id, a.key, a.active, a.has_data, sr, wr, lock,
            ));
            for w in &a.windows {
                s.push_str(&format!(
                    "{}={}:r={}|",
                    w.id,
                    w.pct.map(|v| v.round() as i64).unwrap_or(-1),
                    w.reset,
                ));
            }
            // Deliberately EXCLUDE `a.updated` from the signature. It's the
            // human "Xs ago" / "Xm ago" string that changes every second for
            // the first minute after each poll — folding it in forced a full
            // `install_menu` on every 0.75s tick (defeating the whole cache).
            // Real underlying freshness is already captured by `a.has_data`
            // and each window's pct+reset above. See H8 in the round-1
            // codeaudit findings.
        }
        s.push_str("] ");
    }
    // Capture-submenu contents also affect the redraw: adding a new registered
    // provider (or a change in its installed-probe answer) must show up. Both
    // buckets fold into the signature so moving a provider between them (e.g.
    // a capture-mode change) also triggers a redraw.
    for reg in &snap.capture_creds {
        s.push_str(&format!(
            "REG[{}|{}|{}|creds] ",
            reg.provider_id, reg.display_name, reg.installed
        ));
    }
    for reg in &snap.capture_api_key {
        s.push_str(&format!(
            "REG[{}|{}|{}|apikey] ",
            reg.provider_id, reg.display_name, reg.installed
        ));
    }
    s.push_str(&format!("as={} th={:.0}", snap.autoswap, snap.threshold));
    s
}

fn add(menu: &Menu, item: MenuItem) {
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
            format!("{} — session {}, weekly {}", a.display, pct(s), pct(w))
        }
        None => "usagio: no active account".to_string(),
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
/// actions (`quit`, `autoswap:toggle`, `autoswap:off`, `autoswap:95`,
/// `autoswap:now`, `notifications:threshold` / `:resetback` / `:pace`,
/// `backup:save`, `backup:restore`, `refresh:now`, `noop`, `capture` — the
/// plain-capture id used only by the submenu title itself, never a click).
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
        ("autoswap", Some("toggle"), None) => toggle_autoswap(),
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
        ("notifications", Some(trigger @ ("threshold" | "resetback" | "pace")), None) => {
            toggle_notification_trigger(trigger)
        }
        ("capture", Some(slug), None) => handle_capture(slug),
        ("apikey", Some(slug), None) => handle_apikey_capture(slug),
        ("switch", Some(slug), Some(key)) => handle_switch(slug, key),
        ("remove", Some(slug), Some(key)) => handle_remove(slug, key),
        ("launch", Some(slug), Some(key)) => handle_launch(slug, key),
        ("backup", Some("save"), None) => handle_backup_save(),
        ("backup", Some("restore"), None) => handle_backup_restore_dialog(),
        ("refresh", Some("now"), None) => handle_refresh_now(),
        // Ignore unrecognized ids (e.g. the top-level "capture"/"capture:apikey"
        // submenu titles or a future action added by a later phase we don't
        // yet handle).
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Settings ▸ Advanced ▸ Backups ▸ handlers (native Save…/Restore… panels)
// ---------------------------------------------------------------------------

/// "Save…" click: opens a native SAVE panel (via
/// `Platform::file_dialog().save_file(...)`) defaulting to
/// `~/Downloads/usagio-state-{timestamp}.json`, then writes a
/// REDACTED (token-free) dump of the current state to the chosen path with
/// mode 0600. This is a portable diagnostics/config snapshot — the same
/// shape `redact_state_for_dump` already produces for the "REFUSED save"
/// diagnostic dump — NOT a working credential backup. The full-fidelity
/// automatic rolling backups under `config_dir()/backups/` (written on every
/// real `state.json` save; see `write_rolling_backup`) are what "Restore…"
/// below defaults its file panel to.
fn handle_backup_save() {
    handle_backup_save_with(crate::platform().file_dialog());
}

/// Testable core of the Save… click: takes the `FileDialog` as a parameter
/// so `#[cfg(test)]` can pass a `platform::MockFileDialog` instead of
/// popping a real native panel. See `platform::Platform::file_dialog`.
fn handle_backup_save_with(dialog: &dyn crate::platform::FileDialog) {
    let default_name = format!(
        "usagio-state-{}.json",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    let default_dir = dirs::download_dir();
    let Some(path) = dialog.save_file(&default_name, default_dir.as_deref()) else {
        return; // user cancelled
    };
    let st = match State::load() {
        Ok(s) => s,
        Err(e) => {
            notify(&format!("Save failed: {e}"));
            return;
        }
    };
    let redacted = crate::store::redact_state_for_dump(&st);
    let bytes = match serde_json::to_vec_pretty(&redacted) {
        Ok(b) => b,
        Err(e) => {
            notify(&format!("Save failed: {e}"));
            return;
        }
    };
    if let Err(e) = crate::store::write_private(&path, &bytes) {
        notify(&format!("Save failed: {e}"));
        return;
    }
    notify(&format!("Saved to {}", path.display()));
}

/// "Restore…" click: opens a native OPEN panel (via
/// `Platform::file_dialog().pick_file(...)`) defaulting to the automatic
/// rolling-backups directory, validates the chosen file is state-shaped,
/// warns — by NAME, not just count — before a restore would drop accounts,
/// then replaces `state.json` through the same `save_state_safe`
/// drop-protection every other write goes through. The rolling backups
/// already snapshot the pre-restore state on every ordinary save, but we
/// ALSO stash the live file to `<config_dir>/backups/` right before
/// overwriting it, so the restore itself is reversible even if the user
/// picked a very old backup.
///
/// H2 (v0.5.0 codeaudit): this used to (1) stash the live state — full OAuth
/// tokens, non-redacted — to a `/tmp` sidecar, a world-writable shared
/// directory, and (2) bypass `save_state_safe`'s drop-protection guard and
/// the cross-process state lock entirely, gating the write on nothing but an
/// account *count* comparison (same-count-different-accounts sailed through
/// with zero confirmation). Both are fixed here: the stash moves under
/// `config_dir()/backups/` (owner-only permissions), and the whole
/// read-confirm-write sequence runs under `with_state_lock` with the actual
/// write routed through `store::save_state_restore` (a thin, explicitly-
/// authorized wrapper around `save_state_safe`). Note: the M2 concern
/// (`State::load()` failure silently becoming "0 accounts") is subsumed by
/// this restructure — `accounts_dropped_by(&new_state)?` bubbles the load
/// error up out of `with_state_lock`, and the `Err(e) => notify(...)` arm
/// below surfaces it, so the drop-warning path can't be silently skipped.
fn handle_backup_restore_dialog() {
    handle_backup_restore_dialog_with(crate::platform().file_dialog());
}

/// Testable core of the Restore… click: takes the `FileDialog` as a
/// parameter so `#[cfg(test)]` can pass a `platform::MockFileDialog` instead
/// of popping a real native panel. See `platform::Platform::file_dialog`.
fn handle_backup_restore_dialog_with(dialog: &dyn crate::platform::FileDialog) {
    let mut default_dir = None;
    if let Ok(p) = crate::store::state_json_path() {
        let backups_dir = p.parent().map(|d| d.join("backups"));
        if let Some(dir) = backups_dir {
            if dir.is_dir() {
                default_dir = Some(dir);
            }
        }
    }
    let Some(path) = dialog.pick_file(default_dir.as_deref()) else {
        return; // user cancelled
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            notify(&format!("Restore failed reading {}: {e}", path.display()));
            return;
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            notify(&format!("Restore failed: not valid JSON ({e})"));
            return;
        }
    };
    if value.get("accounts").is_none() {
        notify("Restore failed: not a usagio state file (missing 'accounts')");
        return;
    }
    let new_state = State::from_value(&value);

    let outcome = with_state_lock(|| -> Result<bool> {
        let dropped = crate::store::accounts_dropped_by(&new_state)?;
        if !dropped.is_empty() && !confirm(&restore_drop_confirmation(&dropped)) {
            return Ok(false); // user declined; not an error
        }
        let dir = crate::store::config_dir()?;
        crate::store::stash_pre_restore(&dir)?;
        crate::store::save_state_restore(new_state.clone())?;
        Ok(true)
    });

    match outcome {
        Ok(true) => notify(&format!("Restored state from {}", path.display())),
        Ok(false) => {} // user declined the drop confirmation; no-op
        Err(e) => notify(&format!("Restore failed: {e}")),
    }
}

/// Build the confirmation question shown before a restore that would drop
/// accounts. Names the specific dropped emails (H2, v0.5.0 codeaudit — the
/// prior wording was "will drop 2 account(s)", which told the user nothing
/// about *which* accounts they were about to lose).
fn restore_drop_confirmation(dropped: &[String]) -> String {
    format!("Restoring will drop {}. Continue?", dropped.join(", "))
}

/// Guards against piling up blocked threads when "Refresh usage now" is
/// clicked repeatedly: a stalled network call (see `run_cycle`) means each
/// new click would otherwise stack up its own throwaway `SwapGuard` and
/// thread. `false` = no refresh in flight; CAS to `true` before spawning,
/// reset to `false` when the spawned thread's cycle completes.
static REFRESH_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// "Refresh usage now" click (Settings ▸ Advanced ▸). Runs one poll +
/// auto-swap cycle on a background thread — same `run_cycle` the poll loop
/// calls every `WATCH_INTERVAL_SECS` — so the menu never blocks on network
/// I/O. The main-thread timer picks up the refreshed cache on its next tick.
///
/// De-duplicated via `REFRESH_IN_FLIGHT`: a second click while a refresh is
/// still running notifies instead of spawning another thread. See
/// `try_start_refresh` for the testable core.
fn handle_refresh_now() {
    try_start_refresh(|| {
        std::thread::spawn(|| {
            let mut guard = SwapGuard::default();
            let (rate_limited, _max_pct, _trigger) = run_cycle(&mut guard);
            REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
            if rate_limited {
                notify("Refresh: rate limited, backing off");
            } else {
                notify("Usage refreshed");
            }
        });
    })
}

/// CAS `REFRESH_IN_FLIGHT` from `false` to `true`; if it was already `true`
/// (a refresh is still running), notify and return without calling `spawn`.
/// Factored out from `handle_refresh_now` so a test can inject a counting
/// closure in place of a real `std::thread::spawn` and assert the second of
/// two rapid calls never invokes it.
fn try_start_refresh(spawn: impl FnOnce()) {
    use std::sync::atomic::Ordering;
    if REFRESH_IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        notify("Refresh already running");
        return;
    }
    spawn();
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
        notify(&format!(
            "Capture failed: provider '{slug}' is not registered"
        ));
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

/// If `key`'s account is currently "locked" (session or weekly window at
/// ≥99.5% with a still-future reset — the same rule `header_row`'s "locked ·
/// Xh Ym" display uses), return the human countdown string. `None` if the
/// account isn't found, the provider isn't registered, or it has headroom.
/// Pure over its `State` argument so tests don't need to touch state.json.
fn switch_target_lock_countdown(st: &State, slug: &str, key: &str) -> Option<String> {
    let provider = providers::get(slug)?;
    let acct = st
        .accounts
        .iter()
        .find(|a| a.key().eq_ignore_ascii_case(key))?;
    let row = row_from_account(acct);
    let view = acctview_from_row(
        &row,
        &st.active,
        provider.provider_id(),
        provider.window_order(),
    );
    locked_countdown_for(&view, now_utc()).map(|(cd, _win)| cd)
}

fn handle_switch(slug: &str, key: &str) {
    // v1 state only knows Claude accounts; a switch on any other slug can't
    // be persisted yet, so gate on Claude and route to the shared free
    // function that already knows the v1 identity/keychain dance. This is
    // belt-and-suspenders with `capabilities().supports_switching` (which
    // already keeps the "Switch to this account" row from being built for a
    // non-switching provider — see `build_account_submenu`, H3 in the
    // v0.5.0 codeaudit): the click-id dispatch table is a single flat match
    // in `handle_click`, so nothing stops a stray/future id shaped like
    // `switch:<other-slug>:<key>` from reaching this function directly.
    if slug != CLAUDE_SLUG {
        notify(&format!("Switching is not yet supported for {slug}"));
        return;
    }
    // Item 8 of the v0.5.0 redesign: refuse a switch into an account still
    // under its own rate-limit wall — auto-swap will pick it back up the
    // moment it resets, and switching now would just leave the user on a
    // 0%-headroom account.
    if let Ok(st) = State::load() {
        if let Some(cd) = switch_target_lock_countdown(&st, slug, key) {
            notify(&format!(
                "Can't switch to {key}: at 100% for the next {cd}. \
                 Auto-swap will pick it back up when it resets."
            ));
            return;
        }
    }
    match switch_to(key) {
        Ok(label) => notify(&format!("Switched to {label}")),
        Err(e) => notify(&format!("Switch failed: {e}")),
    }
}

/// Handle a click on a "Paste API key ▸ <Provider>" row. The paste-a-key
/// dialog isn't wired yet — every API-key provider currently returns
/// `ProviderError::Unsupported` from `capture_api_key` — so surface a plain
/// "coming soon" notification instead of silently swallowing the click. Once
/// the paste-a-key dialog lands, this dispatches into `provider.capture_api_key`
/// with the user-entered nickname + key.
fn handle_apikey_capture(slug: &str) {
    let Some(provider) = providers::get(slug) else {
        notify(&format!(
            "Paste API key: provider '{slug}' is not registered"
        ));
        return;
    };
    // Deliberately call the trait method so the error message reflects the
    // real provider state — a future wire-up (returning Ok) will just skip
    // the notify branch below without touching this handler.
    match provider.capture_api_key(String::new(), String::new()) {
        Ok(acct) => notify(&format!(
            "Captured {} API-key account (persistence lands in a later phase): {}",
            provider.display_name(),
            acct.identity
                .email
                .clone()
                .or(acct.identity.display_name.clone())
                .unwrap_or_else(|| "<unnamed>".into()),
        )),
        Err(_) => notify(&format!(
            "{} API-key capture — coming soon",
            provider.display_name()
        )),
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
        notify(&format!(
            "Launch failed: provider '{slug}' is not registered"
        ));
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

// Context Ledger menu rendering + click handler removed in 0.4.2. The
// submenu shelled out via osascript to open a new Terminal window, which
// silently failed for anyone who hadn't granted Automation permission — a
// broken menu row is worse than no menu row. The CLI (`usagio context
// [--provider slug]`) is unchanged and remains the supported entry point.

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

/// The top-level "☑ Auto-swap enabled" checkbox click: flips whatever the
/// on-disk state currently says, rather than assuming a fixed target — so a
/// stale menu (built just before an external `usagio` CLI toggle) can't
/// un-toggle a setting it never actually observed.
///
/// Read-modify-write happens entirely INSIDE `with_state_lock`, atomic with
/// respect to other in-process AND cross-process writers (the CLI `usagio`
/// binary and the menu-bar app both take the same advisory file lock — see
/// `with_state_lock`). A previous version read the current value with a
/// separate `State::load()` call *outside* the lock, decided the new bool
/// from that stale read, and only entered the lock to write it — a
/// concurrent writer between the read and the write could have its own
/// toggle silently lost ("toggle lost" under contention).
fn toggle_autoswap() {
    let r = with_state_lock(|| {
        let mut st = State::load()?;
        st.autoswap_disabled = !st.autoswap_disabled;
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save auto-swap setting: {e}"));
    }
}

/// Flip one Settings ▸ Notifications ▸ per-trigger checkbox. `trigger` is one
/// of "threshold" / "resetback" / "pace" (the three click-id suffixes
/// `build_menu` wires up) — reads-then-flips the on-disk value so a stale
/// menu snapshot can't clobber a setting changed elsewhere in the meantime.
fn toggle_notification_trigger(trigger: &str) {
    let r = with_state_lock(|| {
        let mut st = State::load()?;
        let cfg = &mut st.notification_config;
        match trigger {
            "threshold" => cfg.threshold_enabled = !cfg.threshold_enabled,
            "resetback" => cfg.reset_back_enabled = !cfg.reset_back_enabled,
            "pace" => cfg.pace_enabled = !cfg.pace_enabled,
            _ => {}
        }
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save notification setting: {e}"));
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
         default button \"Cancel\" with title \"usagio\""
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
// Start-at-login used to live here as a menu checkbox backed by `osascript
// → tell "System Events" → make login item …`. Removed in 0.4.3: the
// osascript path re-prompted for Automation permission on every brew
// upgrade (binary hash changed → macOS treated the new binary as a
// different app), and it was redundant with `usagio install` which
// registers a proper launchd LaunchAgent (no osascript, no prompt).

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::cell::Cell as StdCell;

    fn bands() -> SeverityBands {
        SeverityBands {
            amber: 80.0,
            red: 95.0,
        }
    }

    thread_local! {
        /// Wall-clock hook honored by `menubar::now_utc` under `#[cfg(test)]`.
        /// Fixed to the epoch by default so a test that doesn't care about
        /// countdown transitions gets a stable "not locked" reading (both
        /// `session_reset_at` and `weekly_reset_at` are None on the default
        /// fixture, and `is_blocking` short-circuits on `None`).
        static TEST_NOW: StdCell<DateTime<Utc>> =
            StdCell::new(Utc.timestamp_opt(0, 0).unwrap());
    }

    /// Read the current test-frozen wall clock. Called from `now_utc()` under
    /// `#[cfg(test)]` — see `now_utc`.
    pub(super) fn test_now() -> DateTime<Utc> {
        TEST_NOW.with(|c| c.get())
    }

    /// Run `f` with the test wall clock pinned to `t`, restoring it after.
    fn with_now<R>(t: DateTime<Utc>, f: impl FnOnce() -> R) -> R {
        let prev = TEST_NOW.with(|c| c.replace(t));
        let out = f();
        TEST_NOW.with(|c| c.set(prev));
        out
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
            // Default fixture: no reset instants → `compute_display` returns
            // Usage regardless of pct. Tests that exercise lock transitions
            // set these explicitly.
            session_reset_at: None,
            weekly_reset_at: None,
        }
    }

    fn one_section_snap(a: AcctView) -> Snapshot {
        Snapshot {
            sections: vec![ProviderSection {
                provider_id: CLAUDE_SLUG,
                display_name: "Claude",
                supports_switching: true,
                supports_usage: true,
                supports_launch: true,
                supports_remove: true,
                severity_bands: bands(),
                env_override_active: false,
                accounts: vec![a],
            }],
            capture_creds: vec![RegisteredProvider {
                provider_id: CLAUDE_SLUG,
                display_name: "Claude",
                installed: true,
                capture_mode: CaptureMode::CredsOnDisk,
            }],
            capture_api_key: Vec::new(),
            autoswap: false,
            threshold: 95.0,
            notification_config: crate::notifications::NotificationConfig::default(),
        }
    }

    #[test]
    fn try_start_refresh_dedupes_rapid_clicks() {
        // Reset in case a prior test in this binary left it set (best-effort
        // — tests run with --test-threads=1 so no other test races us here).
        REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);

        let spawn_count = std::rc::Rc::new(StdCell::new(0u32));
        let c1 = spawn_count.clone();
        try_start_refresh(move || {
            c1.set(c1.get() + 1);
            // Deliberately do NOT reset REFRESH_IN_FLIGHT here — this stands
            // in for "the background thread is still running" so the second
            // click below is the one under test.
        });
        assert_eq!(spawn_count.get(), 1, "first click must spawn");

        let c2 = spawn_count.clone();
        try_start_refresh(move || {
            c2.set(c2.get() + 1);
        });
        assert_eq!(
            spawn_count.get(),
            1,
            "second rapid click must NOT spawn a second refresh"
        );

        // Clean up so later tests in this binary see the flag cleared.
        REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn toggle_autoswap_read_modify_write_is_atomic_under_contention() {
        let g = crate::store::ScopedConfigDir::new();
        let home = g.home();

        State::default().save().expect("seed initial state");
        let initial = State::load().unwrap().autoswap_disabled;

        const ITERATIONS: usize = 100;
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let home = home.clone();
                std::thread::spawn(move || {
                    // HOME_OVERRIDE is thread-local (see store::ScopedConfigDir);
                    // each worker thread must repoint it at the same tempdir
                    // the parent test set up.
                    crate::store::set_home_override(Some(home));
                    for _ in 0..ITERATIONS {
                        toggle_autoswap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker thread panicked");
        }

        let total_toggles = 2 * ITERATIONS;
        let expected = if total_toggles % 2 == 1 {
            !initial
        } else {
            initial
        };
        let final_state = State::load().unwrap();
        assert_eq!(
            final_state.autoswap_disabled, expected,
            "final parity must match initial XOR (total_toggles % 2 == 1); a lost \
             toggle under contention would flip this"
        );

        drop(g);
    }

    // NOTE: M2's `restore_old_account_count` helper + its two tests were
    // deleted at aggregation time. The M2 concern (State::load failure
    // silently becoming "0 accounts" and skipping the drop-warning prompt)
    // is fully subsumed by H2's restructure: the new Restore flow runs
    // under `with_state_lock`, delegates the drop-detection to
    // `crate::store::accounts_dropped_by(&new_state)` (which propagates
    // load errors via `?`), and the outer `Err(e) => notify(...)` arm
    // surfaces the abort message to the user. Tests for the H2 path live
    // in store_tests.rs and menubar's Restore… test block above.

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
    fn main_row_colors_land_on_percentages() {
        let a = acct("you@work.com", Some(82.0), Some(96.0), true);
        let r = main_row("Claude", &a, bands());
        assert_eq!(r.plain, "Claude    you@work.com\tS 82%  W 96%");
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
    fn main_row_low_usage_has_no_colors_and_no_bold_when_inactive() {
        let a = acct("dev@side.com", Some(3.0), Some(9.0), false);
        let r = main_row("Claude", &a, bands());
        assert!(!r.bold);
        assert!(r.colors.is_empty());
    }

    #[test]
    fn main_row_offsets_hold_for_unicode_email() {
        let a = acct("café@x.com", Some(99.0), None, false);
        let r = main_row("Claude", &a, bands());
        let (off, len, _) = r.colors[0];
        let utf16: Vec<u16> = r.plain.encode_utf16().collect();
        let picked = String::from_utf16(&utf16[off..off + len]).unwrap();
        assert_eq!(picked, "99%");
    }

    #[test]
    fn main_row_includes_provider_name_padded() {
        // Item 2 of the redesign: the provider name gets its own column-like
        // padding, then the email, then the tab-stopped `S n%  W n%` run.
        let claude = main_row(
            "Claude",
            &acct("a@x.com", Some(1.0), Some(2.0), false),
            bands(),
        );
        let codex = main_row(
            "Codex",
            &acct("b@x.com", Some(1.0), Some(2.0), false),
            bands(),
        );
        let (claude_label, _) = claude.plain.split_once('\t').unwrap();
        let (codex_label, _) = codex.plain.split_once('\t').unwrap();
        assert!(claude_label.starts_with("Claude"));
        assert!(codex_label.starts_with("Codex"));
        // Both providers' emails start at the same column regardless of the
        // provider-name length ("Claude" vs "Codex").
        assert_eq!(claude_label.find("a@x.com"), codex_label.find("b@x.com"));
    }

    #[test]
    fn submenu_info_rows_never_include_a_percentage() {
        // Item 3 of the redesign: percentages move to the main-list row;
        // submenu rows are reset-window / burn-rate / footer text only.
        let a = acct("a@x.com", Some(97.0), Some(88.0), true);
        let sec = ProviderSection {
            provider_id: CLAUDE_SLUG,
            display_name: "Claude",
            supports_switching: true,
            supports_usage: true,
            supports_launch: true,
            supports_remove: true,
            severity_bands: bands(),
            env_override_active: false,
            accounts: vec![],
        };
        for row in submenu_info_rows(&sec, &a) {
            assert!(!row.contains('%'), "submenu row leaked a percentage: {row}");
        }
    }

    #[test]
    fn u16len_counts_surrogate_pairs() {
        assert_eq!(u16len("abc"), 3);
        assert_eq!(u16len("café"), 4); // é is BMP → 1 code unit
        assert_eq!(u16len("a😀b"), 4); // 😀 is astral → 2 code units
    }

    #[test]
    fn main_row_offsets_hold_for_astral_email() {
        let a = acct("😀@x.com", Some(99.0), None, false);
        let r = main_row("Claude", &a, bands());
        let (off, len, _) = r.colors[0];
        let utf16: Vec<u16> = r.plain.encode_utf16().collect();
        let picked = String::from_utf16(&utf16[off..off + len]).unwrap();
        assert_eq!(picked, "99%");
    }

    #[test]
    fn launchd_managed_detects_matching_service_name() {
        assert!(launchd_managed_from_env(Some(crate::AUTOSTART_LABEL)));
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

    // (Context Ledger menu-item wiring tests removed in 0.4.2 alongside the
    // submenu itself. The CLI-level ledger tests still cover the audit path.)

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
                    supports_launch: true,
                    supports_remove: true,
                    severity_bands: bands(),
                    env_override_active: false,
                    accounts: vec![a],
                },
                ProviderSection {
                    provider_id: "codex",
                    display_name: "Codex",
                    supports_switching: true,
                    supports_usage: true,
                    supports_launch: true,
                    supports_remove: true,
                    severity_bands: bands(),
                    env_override_active: false,
                    accounts: vec![b],
                },
            ],
            capture_creds: Vec::new(),
            capture_api_key: Vec::new(),
            autoswap: false,
            threshold: 95.0,
            notification_config: crate::notifications::NotificationConfig::default(),
        };
        let (sec, acc) = active_account(&snap).expect("active row found");
        assert_eq!(sec.provider_id, "codex");
        assert_eq!(acc.key, "work@x.com");
    }

    // --- capture-menu filter --------------------------------------------

    /// Tiny fixture provider — supports_usage + capture_mode are the only
    /// knobs `partition_capture_providers` reads, so the rest returns
    /// `Unsupported` and the id/name are what the assertions look at.
    struct FakeProvider {
        id: &'static str,
        name: &'static str,
        supports_usage: bool,
        capture_mode: CaptureMode,
    }

    impl Provider for FakeProvider {
        fn provider_id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self) -> providers::Capabilities {
            providers::Capabilities {
                supports_usage: self.supports_usage,
                supports_switching: false,
                supports_launch: false,
                supports_remove: true,
                supports_email_capture: false,
                secret_backend: providers::SecretBackend::File,
                capture_mode: self.capture_mode,
            }
        }
        fn capture_current_login(&self) -> providers::PResult<Option<providers::CapturedAccount>> {
            Ok(None)
        }
        fn parse_stored_blob(&self, _blob: &str) -> providers::PResult<providers::TokenGrant> {
            Err(providers::ProviderError::Unsupported)
        }
        fn patch_stored_blob(
            &self,
            _blob: &str,
            _grant: &providers::TokenGrant,
        ) -> providers::PResult<String> {
            Err(providers::ProviderError::Unsupported)
        }
    }

    #[test]
    fn partition_capture_providers_filters_stubs_and_buckets_by_capture_mode() {
        // A mixed registry: one full creds provider (Claude-shaped), one
        // full API-key provider (OpenRouter-shaped), one creds-on-disk stub
        // whose usage isn't wired yet (Cline-shaped), and one API-key-shaped
        // row that also has `supports_usage == false` (hypothetical stub —
        // still filtered because the capture filter only cares about
        // `supports_usage`).
        let claude_like = FakeProvider {
            id: "claude-like",
            name: "Claude-like",
            supports_usage: true,
            capture_mode: CaptureMode::CredsOnDisk,
        };
        let openrouter_like = FakeProvider {
            id: "openrouter-like",
            name: "OpenRouter-like",
            supports_usage: true,
            capture_mode: CaptureMode::ApiKey,
        };
        let cline_stub = FakeProvider {
            id: "cline-stub",
            name: "Cline-stub",
            supports_usage: false,
            capture_mode: CaptureMode::CredsOnDisk,
        };
        let apikey_stub = FakeProvider {
            id: "apikey-stub",
            name: "APIKey-stub",
            supports_usage: false,
            capture_mode: CaptureMode::ApiKey,
        };

        let regs: Vec<Box<dyn Provider>> = vec![
            Box::new(claude_like),
            Box::new(openrouter_like),
            Box::new(cline_stub),
            Box::new(apikey_stub),
        ];

        let (creds, api_key) = partition_capture_providers(&regs, &[]);

        let creds_ids: Vec<&str> = creds.iter().map(|p| p.provider_id()).collect();
        let api_key_ids: Vec<&str> = api_key.iter().map(|p| p.provider_id()).collect();

        // Full creds provider is in the creds bucket.
        assert_eq!(creds_ids, vec!["claude-like"]);
        // Full API-key provider is in the api-key bucket.
        assert_eq!(api_key_ids, vec!["openrouter-like"]);

        // The stub with capture_mode == CredsOnDisk but supports_usage == false
        // is excluded from the creds list and included in NEITHER bucket.
        assert!(
            !creds_ids.contains(&"cline-stub"),
            "creds-on-disk stub must be excluded from the creds list",
        );
        assert!(
            !api_key_ids.contains(&"cline-stub"),
            "creds-on-disk stub must not leak into the api-key list either",
        );
        // Same guarantee for a stub with capture_mode == ApiKey: no usage → not shown.
        assert!(!creds_ids.contains(&"apikey-stub"));
        assert!(!api_key_ids.contains(&"apikey-stub"));
    }

    // -----------------------------------------------------------------------
    // Menu-redesign / countdown / icons tests
    // -----------------------------------------------------------------------

    /// Build an account with explicit reset instants so the locked-vs-usage
    /// transition can be pinned in tests.
    fn acct_with_resets(
        email: &str,
        session: Option<f64>,
        weekly: Option<f64>,
        active: bool,
        session_reset_at: Option<DateTime<Utc>>,
        weekly_reset_at: Option<DateTime<Utc>>,
    ) -> AcctView {
        let mut a = acct(email, session, weekly, active);
        a.session_reset_at = session_reset_at;
        a.weekly_reset_at = weekly_reset_at;
        a
    }

    #[test]
    fn main_row_flat_shape_matches_spec_when_not_locked() {
        // "{provider}    {email}\tS {n}%  W {n}%" per the v0.5.0 redesign —
        // the crate's variant uses a TAB between the label and trailing run
        // so AppKit right-aligns it. The important structural invariants are:
        // provider, then email, one TAB, then "S n%  W n%". Any change to
        // that layout will fail this assertion — a wall against silent drift.
        let a = acct("you@work.com", Some(42.0), Some(61.0), false);
        let r = main_row("Claude", &a, bands());
        assert_eq!(r.plain, "Claude    you@work.com\tS 42%  W 61%");
        assert!(!r.checkmark, "inactive row: no leading checkmark");
    }

    #[test]
    fn main_row_switches_to_locked_countdown_when_over_threshold() {
        // Session at 100% with a reset ~90 minutes out → row must swap the
        // percentages for "locked · 1h 30m" and paint it red. This is the
        // "usage → locked" transition the redesign spec calls out.
        let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
        let reset = now + chrono::Duration::minutes(90);
        with_now(now, || {
            let a = acct_with_resets(
                "matt@example.com",
                Some(100.0),
                Some(20.0),
                true,
                Some(reset),
                None,
            );
            let r = main_row("Claude", &a, bands());
            // v0.5.0 UX: no "locked · " prefix — red color + a time (not a
            // percent) is the affordance. See main_row's locked-branch comment.
            assert_eq!(r.plain, "Claude    matt@example.com\t1h 30m");
            assert!(r.bold, "active locked row is still bold");
            assert!(r.checkmark, "active row gets a leading checkmark");
            // Exactly one colored span, red-tinted, covering the countdown run.
            assert_eq!(r.colors.len(), 1);
            let (off, len, sev) = r.colors[0];
            assert_eq!(sev, Severity::Red);
            let picked: String = r.plain.chars().skip(off).take(len).collect();
            assert_eq!(picked, "1h 30m");
        });
    }

    #[test]
    fn main_row_stays_usage_when_reset_is_stale() {
        // pct at 100 but reset already in the past — countdown treats it as
        // stale (next refresh will fix the pct), so the row stays "S / W".
        let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
        let past = now - chrono::Duration::minutes(5);
        with_now(now, || {
            let a = acct_with_resets("dev@x.com", Some(100.0), None, false, Some(past), None);
            let r = main_row("Claude", &a, bands());
            assert_eq!(r.plain, "Claude    dev@x.com\tS 100%  W —");
            assert!(!r.plain.contains("locked"));
        });
    }

    #[test]
    fn sort_by_expiration_orders_accounts_soonest_first() {
        // Regression test for the user-reported bug: newly-added accounts land
        // at the tail of `state.accounts` (upsert appends) and stayed at the
        // bottom of the menu even when their weekly window resets sooner.
        // `sort_by_expiration` fixes that — accounts sort by weekly_reset_at
        // ASC, so the "closest to expiration" is on top.
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let mut soonest = acct("soon@x.com", Some(50.0), Some(60.0), false);
        soonest.weekly_reset_at = Some(now + chrono::Duration::hours(6));
        let mut middle = acct("mid@x.com", Some(50.0), Some(60.0), false);
        middle.weekly_reset_at = Some(now + chrono::Duration::hours(48));
        let mut latest = acct("late@x.com", Some(50.0), Some(60.0), false);
        latest.weekly_reset_at = Some(now + chrono::Duration::hours(120));

        // Insert in REVERSE-expiration order (mirrors what upsert would
        // produce if the user added them latest-first).
        let mut accounts = [latest, middle, soonest];
        accounts.sort_by(sort_by_expiration);

        let keys: Vec<&str> = accounts.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, vec!["soon@x.com", "mid@x.com", "late@x.com"]);
    }

    #[test]
    fn sort_by_expiration_places_no_data_accounts_last() {
        // An account without cached usage yet has `weekly_reset_at == None`.
        // The sort treats None as "furthest in the future" so a freshly-
        // captured account (no data) sinks to the bottom rather than
        // displacing an account with a real, soon reset.
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let mut with_data = acct("data@x.com", Some(10.0), Some(20.0), false);
        with_data.weekly_reset_at = Some(now + chrono::Duration::hours(6));
        let no_data = acct("nodata@x.com", None, None, false);
        assert!(no_data.weekly_reset_at.is_none(), "precondition");

        let mut accounts = [no_data, with_data];
        accounts.sort_by(sort_by_expiration);
        let keys: Vec<&str> = accounts.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, vec!["data@x.com", "nodata@x.com"]);
    }

    #[test]
    fn build_snapshot_sorts_active_first_within_a_section() {
        // `build_snapshot` orders accounts so the active row renders first
        // within each provider block — the "active-first ordering" rule from
        // the redesign spec. We assert on the sort key `menu_order` cannot
        // itself provide (it doesn't know about `active`).
        let mut a1 = acct("second@x.com", Some(10.0), Some(20.0), false);
        a1.active = false;
        let mut a2 = acct("active@x.com", Some(50.0), Some(60.0), true);
        a2.active = true;
        let mut a3 = acct("third@x.com", Some(30.0), Some(40.0), false);
        a3.active = false;
        let mut accounts = [a1, a2, a3];
        accounts.sort_by_key(|a| std::cmp::Reverse(a.active));
        assert_eq!(accounts[0].key, "active@x.com", "active is first");
        // Stable sort: the two inactives keep their original order.
        assert_eq!(accounts[1].key, "second@x.com");
        assert_eq!(accounts[2].key, "third@x.com");
    }

    #[test]
    fn menu_styles_attaches_icon_slug_to_every_main_row() {
        // v0.5.0 dropped the separate disabled provider-header row — each
        // account's own flat main-list row now carries `icon_slug` so the
        // native walk `setImage:`s a bundled 16px PNG directly on it.
        let snap = one_section_snap(acct("a@x.com", Some(10.0), Some(20.0), true));
        let styles = menu_styles(&snap);
        let main_style = styles
            .iter()
            .find(|s| s.tab_x.is_some())
            .expect("main-list row present");
        assert_eq!(main_style.icon_slug, Some(CLAUDE_SLUG));
        // The submenu info rows (no tab stop) carry no icon.
        for s in styles.iter().filter(|s| s.tab_x.is_none()) {
            assert!(
                s.icon_slug.is_none(),
                "unexpected icon on non-main row: {}",
                s.plain,
            );
        }
    }

    #[test]
    fn menu_styles_marks_active_account_with_checkmark() {
        // The active-account row sets `checkmark = true` so the native walk
        // renders a leading ✓ glyph via `NSMenuItem::setState(.On)`. Inactive
        // rows must not.
        let mut inactive = acct("dev@x.com", Some(10.0), Some(20.0), false);
        inactive.active = false;
        let active = acct("active@x.com", Some(50.0), Some(60.0), true);
        let snap = Snapshot {
            sections: vec![ProviderSection {
                provider_id: CLAUDE_SLUG,
                display_name: "Claude",
                supports_switching: true,
                supports_usage: true,
                supports_launch: true,
                supports_remove: true,
                severity_bands: bands(),
                env_override_active: false,
                accounts: vec![active, inactive],
            }],
            capture_creds: Vec::new(),
            capture_api_key: Vec::new(),
            autoswap: false,
            threshold: 95.0,
            notification_config: crate::notifications::NotificationConfig::default(),
        };
        let styles = menu_styles(&snap);
        // Only inspect the per-account main-list rows (they carry a tab
        // stop; the Quit row is excluded by matching on the email instead).
        let mut saw_active = false;
        let mut saw_inactive = false;
        for s in styles.iter().filter(|s| s.tab_x.is_some()) {
            if s.plain.contains("active@x.com") {
                assert!(s.checkmark, "active row must carry the checkmark flag");
                saw_active = true;
            }
            if s.plain.contains("dev@x.com") {
                assert!(
                    !s.checkmark,
                    "inactive row must NOT carry the checkmark flag"
                );
                saw_inactive = true;
            }
        }
        assert!(saw_active && saw_inactive, "both rows produced a style");
    }

    #[test]
    fn build_snapshot_omits_sections_for_providers_with_zero_accounts() {
        // No captured accounts → no section (the "no header, no rows" rule).
        // Uses the pure `partition_capture_providers` sibling to prove the
        // filter logic without touching state.json. `build_snapshot`'s own
        // gate is the `if provider_rows.is_empty() { continue; }` branch —
        // this test locks in the *behavior* the gate exists to enforce.
        let snap = Snapshot::default();
        assert!(
            snap.sections.is_empty(),
            "empty snapshot has no sections — a provider with zero rows must never emit one",
        );
    }

    // -----------------------------------------------------------------------
    // H3 (v0.5.0 codeaudit) — capability-gated menu rows. `supports_switching`
    // used to be the sole gate for BOTH the "Switch to this account" row AND
    // the "Launch client" row, and "Remove…" had no gate at all. A provider
    // like Codex (usage-only, no switching, no launch wired) would still get
    // a "Launch client" row that always errored on click.
    // -----------------------------------------------------------------------

    fn section_with_caps(
        supports_switching: bool,
        supports_launch: bool,
        supports_remove: bool,
    ) -> ProviderSection {
        ProviderSection {
            provider_id: "codex",
            display_name: "Codex",
            supports_switching,
            supports_launch,
            supports_remove,
            supports_usage: true,
            severity_bands: bands(),
            env_override_active: false,
            accounts: vec![],
        }
    }

    #[test]
    fn submenu_rows_omit_launch_when_supports_launch_is_false() {
        // Codex today: supports_switching == false AND supports_launch ==
        // false. Neither the Switch nor the Launch row may appear.
        let sec = section_with_caps(false, false, true);
        let a = acct("user@example.com", Some(10.0), Some(20.0), false);
        let rows = account_submenu_rows(&sec, &a);
        assert_eq!(rows.switch_row, None, "no switch row: {rows:?}");
        assert!(!rows.launch_row, "no launch row: {rows:?}");
    }

    #[test]
    fn submenu_rows_can_omit_launch_even_when_switching_is_supported() {
        // The two capabilities are independent: a provider could (in
        // principle) support switching without a wired client launcher.
        // supports_launch alone must gate the Launch row.
        let sec = section_with_caps(true, false, true);
        let a = acct("user@example.com", Some(10.0), Some(20.0), false);
        let rows = account_submenu_rows(&sec, &a);
        assert_eq!(
            rows.switch_row,
            Some(false),
            "clickable switch row present: {rows:?}"
        );
        assert!(
            !rows.launch_row,
            "launch row absent even though switching is supported: {rows:?}"
        );
    }

    #[test]
    fn submenu_rows_include_launch_when_supports_launch_is_true() {
        let sec = section_with_caps(true, true, true);
        let a = acct("user@example.com", Some(10.0), Some(20.0), false);
        assert!(account_submenu_rows(&sec, &a).launch_row);
    }

    #[test]
    fn submenu_rows_marks_active_account_instead_of_a_clickable_switch_row() {
        let sec = section_with_caps(true, true, true);
        let a = acct("user@example.com", Some(10.0), Some(20.0), true);
        assert_eq!(account_submenu_rows(&sec, &a).switch_row, Some(true));
    }

    #[test]
    fn submenu_rows_omit_remove_when_supports_remove_is_false() {
        let sec = section_with_caps(false, false, false);
        let a = acct("user@example.com", Some(10.0), Some(20.0), false);
        assert!(!account_submenu_rows(&sec, &a).remove_row);
    }

    #[test]
    fn submenu_rows_include_remove_when_supports_remove_is_true() {
        let sec = section_with_caps(false, false, true);
        let a = acct("user@example.com", Some(10.0), Some(20.0), false);
        assert!(account_submenu_rows(&sec, &a).remove_row);
    }

    #[test]
    fn codex_capabilities_do_not_advertise_switching_or_launch() {
        // Locks in the H3 downgrade at the real provider boundary (not just
        // the ProviderSection fixture above): CodexProvider's actual
        // capabilities() must not claim switching or launch support until
        // state v2 gives it somewhere to persist a second account.
        let caps = crate::providers::codex::CodexProvider.capabilities();
        assert!(!caps.supports_switching);
        assert!(!caps.supports_launch);
        assert!(caps.supports_remove);
    }

    #[test]
    fn claude_capabilities_advertise_launch_support() {
        // Claude is the only provider with a wired `launch_client`.
        let caps = crate::providers::claude::ClaudeProvider.capabilities();
        assert!(caps.supports_switching);
        assert!(caps.supports_launch);
        assert!(caps.supports_remove);
    }

    #[test]
    fn menu_signature_folds_lock_state_and_reset_at() {
        // Same pcts + updated + windows — but toggling only the reset instant
        // (usage → locked transition) must yield a different signature so the
        // menu redraws with the "locked · Xh Ym" trailing text.
        let now = Utc.timestamp_opt(3_000_000, 0).unwrap();
        with_now(now, || {
            let a_usage = acct_with_resets("m@x.com", Some(100.0), Some(50.0), true, None, None);
            let usage_snap = one_section_snap(a_usage);
            let sig_usage = menu_signature(&usage_snap);
            assert!(sig_usage.contains("L=0"), "usage state marker present");

            let a_locked = acct_with_resets(
                "m@x.com",
                Some(100.0),
                Some(50.0),
                true,
                Some(now + chrono::Duration::hours(2)),
                None,
            );
            let locked_snap = one_section_snap(a_locked);
            let sig_locked = menu_signature(&locked_snap);
            assert!(sig_locked.contains("L=S"), "session-locked marker present");
            assert_ne!(sig_usage, sig_locked, "lock transition must trigger redraw");
        });
    }

    #[test]
    fn capture_menu_filter_hides_zero_usage_stubs_from_creds_list() {
        // Integration-level check on top of `partition_capture_providers`:
        // the live registry produced by `providers::init()` must hide any
        // stub provider (`supports_usage == false`) from the "Capture
        // current login ▸" onboarding surface. Claude + Codex (the two full
        // providers) must be present in the creds bucket.
        providers::init();
        let (creds, _api_key) = capture_menu_providers(&[]);
        let creds_ids: Vec<&str> = creds.iter().map(|p| p.provider_id()).collect();
        assert!(creds_ids.contains(&"claude"), "claude in creds bucket");
        // Every registered stub with supports_usage == false must be filtered.
        for stub in [
            "opencode",
            "gemini-cli",
            "qwen-code",
            "copilot-cli",
            "cursor-agent",
            "amazon-q",
            "cline",
            "grok",
            "kimi",
        ] {
            assert!(
                !creds_ids.contains(&stub),
                "stub `{stub}` must be filtered out of the creds capture list",
            );
        }
    }

    #[test]
    fn main_row_locked_shape_swaps_only_the_trailing_run() {
        // The locked row keeps the "{provider label}\t…" tab structure so
        // right-alignment still works — only the trailing "S n%  W n%" run
        // becomes "locked · <countdown>". A test that pins the structure so
        // a future refactor can't accidentally lose the tab.
        let now = Utc.timestamp_opt(4_000_000, 0).unwrap();
        with_now(now, || {
            let a = acct_with_resets(
                "matt@example.com",
                Some(100.0),
                None,
                false,
                Some(now + chrono::Duration::hours(23) + chrono::Duration::minutes(52)),
                None,
            );
            let r = main_row("Claude", &a, bands());
            let (label, trailing) = r.plain.split_once('\t').expect("tab preserved");
            assert_eq!(label, "Claude    matt@example.com");
            // No "locked · " prefix — red color + a time (not a percent) is
            // the affordance now. See main_row's locked-branch comment.
            assert_eq!(trailing, "23h 52m");
            assert_eq!(r.tab_x, Some(TAB_X), "right-align tab-stop preserved");
        });
    }

    // -----------------------------------------------------------------------
    // v0.5.0 redesign: locked-switch refusal, API-key-once-captured, and the
    // "white but not clickable" submenu style.
    // -----------------------------------------------------------------------

    /// Build a minimal Claude `Account` with cached usage, for
    /// `switch_target_lock_countdown` fixtures. No I/O — a plain in-memory
    /// `store::Account`, not anything touching state.json.
    fn account_with_usage(
        email: &str,
        session_pct: Option<f64>,
        session_reset: Option<DateTime<Utc>>,
    ) -> crate::store::Account {
        crate::store::Account {
            email: Some(email.to_string()),
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: 0,
            keychain_blob: "{}".into(),
            oauth_account: None,
            user_id: None,
            cached_usage: Some(crate::store::CachedUsage {
                session_pct,
                weekly_pct: Some(10.0),
                session_reset: session_reset.map(|t| t.to_rfc3339()),
                weekly_reset: None,
                opus_pct: None,
                opus_reset: None,
                fetched_at: 0,
            }),
            notif_state: crate::notifications::NotifState::default(),
            needs_relogin: false,
        }
    }

    #[test]
    fn switch_target_lock_countdown_refuses_a_maxed_out_account() {
        // Item 8 of the redesign: a session at 100% with a still-future reset
        // is "locked" — `handle_switch` must see this and refuse.
        providers::init();
        let now = Utc.timestamp_opt(5_000_000, 0).unwrap();
        with_now(now, || {
            let reset = now + chrono::Duration::minutes(45);
            let st = State {
                accounts: vec![account_with_usage(
                    "matt@example.com",
                    Some(100.0),
                    Some(reset),
                )],
                ..State::default()
            };
            let cd = switch_target_lock_countdown(&st, CLAUDE_SLUG, "matt@example.com");
            assert_eq!(cd, Some("45m".to_string()));
        });
    }

    #[test]
    fn switch_target_lock_countdown_allows_an_account_with_headroom() {
        providers::init();
        let now = Utc.timestamp_opt(5_000_000, 0).unwrap();
        with_now(now, || {
            let st = State {
                accounts: vec![account_with_usage("matt@example.com", Some(40.0), None)],
                ..State::default()
            };
            assert_eq!(
                switch_target_lock_countdown(&st, CLAUDE_SLUG, "matt@example.com"),
                None,
            );
        });
    }

    #[test]
    fn switch_target_lock_countdown_is_none_for_an_unknown_account() {
        providers::init();
        let st = State::default();
        assert_eq!(
            switch_target_lock_countdown(&st, CLAUDE_SLUG, "nobody@example.com"),
            None,
        );
    }

    #[test]
    fn partition_capture_providers_excludes_an_already_captured_api_key_provider() {
        // Item 9 of the redesign: once an API-key provider has a captured
        // account (surfaced to `partition_capture_providers` as its id being
        // in `captured_provider_ids`), it must disappear from the "Paste API
        // key ▸" bucket — the account itself now renders in the main list.
        let openrouter_like = FakeProvider {
            id: "openrouter-like",
            name: "OpenRouter-like",
            supports_usage: true,
            capture_mode: CaptureMode::ApiKey,
        };
        let regs: Vec<Box<dyn Provider>> = vec![Box::new(openrouter_like)];

        let (_, api_key_before) = partition_capture_providers(&regs, &[]);
        assert_eq!(
            api_key_before
                .iter()
                .map(|p| p.provider_id())
                .collect::<Vec<_>>(),
            vec!["openrouter-like"],
            "not yet captured → still offered under Paste API key ▸",
        );

        let (_, api_key_after) = partition_capture_providers(&regs, &["openrouter-like"]);
        assert!(
            api_key_after.is_empty(),
            "already captured → must NOT be offered under Paste API key ▸ anymore",
        );
    }

    #[test]
    fn disabled_but_white_row_gets_a_full_range_labelcolor_attribute() {
        // The "white but not clickable" style: `disabled_but_white` must
        // produce an `NSForegroundColorAttributeName` run spanning the ENTIRE
        // row (offset 0, full length) — not disabled grey, not a partial span.
        let style = RowStyle {
            disabled_but_white: true,
            ..RowStyle::plain_row("Session resets in 3h".to_string())
        };
        let attr = attributed(&style);
        let full_len = attr.length();
        assert!(full_len > 0);
        let mut effective = objc2_foundation::NSRange::new(0, 0);
        let value = unsafe {
            attr.attribute_atIndex_effectiveRange(NSForegroundColorAttributeName, 0, &mut effective)
        };
        assert!(value.is_some(), "expected a foreground-color attribute");
        assert_eq!(
            effective,
            objc2_foundation::NSRange::new(0, full_len),
            "labelColor must span the whole row",
        );
    }

    #[test]
    fn plain_row_with_no_disabled_but_white_has_no_forced_foreground_color() {
        // A normal (non-info) row must NOT get the full-range labelColor
        // treatment — only `disabled_but_white` rows opt into it.
        let style = RowStyle::plain_row("Switch to this account".to_string());
        let attr = attributed(&style);
        let mut effective = objc2_foundation::NSRange::new(0, 0);
        let value = unsafe {
            attr.attribute_atIndex_effectiveRange(NSForegroundColorAttributeName, 0, &mut effective)
        };
        assert!(
            value.is_none(),
            "a plain row must not carry a forced foreground color",
        );
    }

    // -----------------------------------------------------------------
    // Backups Save…/Restore… ▸ FileDialog trait routing
    //
    // These assert the click handlers call `FileDialog` with the expected
    // arguments WITHOUT ever popping a real native panel — the mock's
    // `save_file`/`pick_file` return `None` by default, so each handler
    // returns right after recording the call (the "user cancelled" path).
    // -----------------------------------------------------------------

    #[test]
    fn backup_save_click_calls_file_dialog_with_default_name_and_downloads_dir() {
        let dialog = crate::platform::MockFileDialog::default();
        handle_backup_save_with(&dialog);

        let calls = dialog.save_file_calls.borrow();
        assert_eq!(calls.len(), 1, "expected exactly one save_file() call");
        let (default_name, default_dir) = &calls[0];
        assert!(
            default_name.starts_with("usagio-state-") && default_name.ends_with(".json"),
            "unexpected default file name: {default_name}"
        );
        assert_eq!(default_dir.as_deref(), dirs::download_dir().as_deref());
    }

    #[test]
    fn backup_restore_click_calls_file_dialog_pick_file_once() {
        let _g = crate::store::ScopedConfigDir::new();
        let dialog = crate::platform::MockFileDialog::default();
        handle_backup_restore_dialog_with(&dialog);

        let calls = dialog.pick_file_calls.borrow();
        assert_eq!(calls.len(), 1, "expected exactly one pick_file() call");
    }

    // -----------------------------------------------------------------------
    // H2 (v0.5.0 codeaudit) — Restore… drop-confirmation wording + store-level
    // wiring. `handle_backup_restore_dialog` itself opens a native file panel
    // and shells out to osascript for confirmation, neither of which is
    // unit-testable; these tests cover the pure logic it's built from.
    // -----------------------------------------------------------------------

    #[test]
    fn restore_drop_confirmation_names_dropped_emails_not_just_a_count() {
        let msg = super::restore_drop_confirmation(&[
            "dev@getbusbar.com".to_string(),
            "matthew@pq.io".to_string(),
        ]);
        assert_eq!(
            msg,
            "Restoring will drop dev@getbusbar.com, matthew@pq.io. Continue?"
        );
    }

    /// Minimal restore-test account.
    fn restore_test_acct(email: &str) -> crate::store::Account {
        let blob = serde_json::json!({
            "claudeAiOauth": { "accessToken": "at", "refreshToken": "rt", "expiresAt": 0 }
        })
        .to_string();
        let mut a = crate::store::Account::from_keychain_blob(&blob).unwrap();
        a.email = Some(email.to_string());
        a
    }

    #[test]
    fn restore_that_only_adds_an_account_reports_no_drops() {
        use crate::store::{accounts_dropped_by, ScopedConfigDir, State};
        let _g = ScopedConfigDir::new();
        let mut current = State::default();
        current.accounts.push(restore_test_acct("a@e.com"));
        current.save().unwrap();

        let mut restore_target = State::load().unwrap();
        restore_target.accounts.push(restore_test_acct("b@e.com"));

        let dropped = accounts_dropped_by(&restore_target).unwrap();
        assert!(
            dropped.is_empty(),
            "adding an account must not be reported as a drop"
        );
    }

    #[test]
    fn restore_that_drops_an_account_reports_exactly_that_email() {
        use crate::store::{accounts_dropped_by, ScopedConfigDir, State};
        let _g = ScopedConfigDir::new();
        let mut current = State::default();
        for email in ["dev@getbusbar.com", "matthew@pq.io"] {
            current.accounts.push(restore_test_acct(email));
        }
        current.save().unwrap();

        let mut restore_target = State::default();
        restore_target
            .accounts
            .push(restore_test_acct("dev@getbusbar.com"));

        let dropped = accounts_dropped_by(&restore_target).unwrap();
        assert_eq!(dropped, vec!["matthew@pq.io".to_string()]);
        assert_eq!(
            restore_drop_confirmation(&dropped),
            "Restoring will drop matthew@pq.io. Continue?"
        );
    }

    #[test]
    fn pre_restore_stash_lands_under_config_backups_not_tmp() {
        use crate::store::{config_dir, stash_pre_restore, ScopedConfigDir, State};
        let g = ScopedConfigDir::new();
        let mut current = State::default();
        current.accounts.push(restore_test_acct("a@e.com"));
        current.save().unwrap();

        let dir = config_dir().unwrap();
        let stash = stash_pre_restore(&dir)
            .unwrap()
            .expect("live state existed");
        assert!(stash.starts_with(g.home().join(".config/usagio/backups")));
        assert!(!stash.starts_with("/tmp"));
    }
}
