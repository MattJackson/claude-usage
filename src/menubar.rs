//! macOS menu-bar app: shows the active account's usage in the status bar and
//! lets you switch / capture / remove accounts and set the auto-swap threshold
//! from a dropdown. The same `watch_cycle` that powers `claude-usage watch` runs
//! on a background thread here, so the daemon behaviour is identical.
//!
//! Usage numbers come from the cache (written by the scheduler poll); the UI
//! never fetches on its own, so menu interactions can't trigger HTTP 429s.

use anyhow::Result;
use std::cell::RefCell;
use std::time::Duration;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::NSTimer;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::TrayIconBuilder;

use crate::store::State;
use crate::{
    age_str, capture_current, next_interval, notify, optimize_now, remove_account,
    row_from_account, switch_to, watch_cycle, with_state_lock, Row, SwapGuard, TARGET_CEILING_PCT,
    TRIGGER_PCT, WATCH_INTERVAL_SECS,
};

/// Name shown for our Login Item in System Events.
const LOGIN_ITEM_NAME: &str = "Claude Usage";

/// One account as shown in the menu.
struct AcctView {
    email: String,
    session: Option<f64>,
    weekly: Option<f64>,
    opus: Option<f64>,
    session_reset: String,
    weekly_reset: String,
    opus_reset: String,
    updated: String,
    active: bool,
    has_data: bool,
}

/// Everything the UI needs to render, produced by the poller thread.
#[derive(Default)]
struct Snapshot {
    accounts: Vec<AcctView>,
    autoswap: bool,
    threshold: f64,
    start_at_login: bool,
}

/// How near a limit a percentage is, for at-a-glance coloring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Severity {
    /// >= 80%: approaching the wall.
    Amber,
    /// >= 95%: about to hit it.
    Red,
}

/// Map a utilization percentage to a color band (None below 80%).
fn severity(p: Option<f64>) -> Option<Severity> {
    match p {
        Some(v) if v >= 95.0 => Some(Severity::Red),
        Some(v) if v >= 80.0 => Some(Severity::Amber),
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

/// The active-account submenu header: `email \t S% / W%`, bold if active, with
/// each high percentage colored. This is the single source of the plain title —
/// `build_menu` uses `.plain` for the submenu title so styling always matches.
fn header_row(a: &AcctView) -> RowStyle {
    let s = pct(a.session);
    let w = pct(a.weekly);
    let plain = format!("{}\t{s} / {w}", a.email);
    let mut colors = Vec::new();
    let s_off = u16len(&a.email) + 1; // + '\t'
    if let Some(sev) = severity(a.session) {
        colors.push((s_off, u16len(&s), sev));
    }
    let w_off = s_off + u16len(&s) + u16len(" / ");
    if let Some(sev) = severity(a.weekly) {
        colors.push((w_off, u16len(&w), sev));
    }
    RowStyle {
        plain,
        bold: a.active,
        colors,
        tab_x: Some(TAB_X),
    }
}

/// The top info line for the active account: `email  ·  S% / W%`, percentages
/// colored (no bold, no tab — it's a disabled header, not a row).
fn top_header_row(a: &AcctView) -> RowStyle {
    let s = pct(a.session);
    let w = pct(a.weekly);
    let plain = header_line(a);
    let mut colors = Vec::new();
    let s_off = u16len(&a.email) + u16len("  ·  ");
    if let Some(sev) = severity(a.session) {
        colors.push((s_off, u16len(&s), sev));
    }
    let w_off = s_off + u16len(&s) + u16len(" / ");
    if let Some(sev) = severity(a.weekly) {
        colors.push((w_off, u16len(&w), sev));
    }
    RowStyle {
        plain,
        bold: false,
        colors,
        tab_x: None,
    }
}

/// All styling directives for the current menu, derived from the same snapshot
/// `build_menu` renders. The native walk applies each by matching `.plain`.
fn menu_styles(snap: &Snapshot) -> Vec<RowStyle> {
    let mut styles = Vec::new();
    if let Some(a) = snap.accounts.iter().find(|a| a.active) {
        styles.push(top_header_row(a));
    }
    for a in &snap.accounts {
        styles.push(header_row(a));
        if a.has_data {
            for (label, pct_val, reset) in [
                ("Session", a.session, &a.session_reset),
                ("Weekly", a.weekly, &a.weekly_reset),
                ("Opus", a.opus, &a.opus_reset),
            ] {
                if label == "Opus" && a.opus.is_none() {
                    continue;
                }
                if let Some(sev) = severity(pct_val) {
                    let (plain, span) = stat_row(label, pct_val, reset);
                    if let Some((off, len)) = span {
                        styles.push(RowStyle {
                            plain,
                            bold: false,
                            colors: vec![(off, len, sev)],
                            tab_x: None,
                        });
                    }
                }
            }
        }
    }
    styles
}

pub fn run() -> Result<()> {
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
    if relaunch_via_launchd() {
        // kickstart -k will SIGKILL and restart this job; wait to be replaced.
        // (If it somehow doesn't, we still exit so we're not left on the old
        // binary — launchd's kickstart reliably restarts an existing job.)
        std::thread::sleep(Duration::from_secs(3));
        std::process::exit(0);
    }
    // Bare run (no launchd job): the orphaned child re-parents to launchd/init
    // and outlives our exit.
    let _ = std::process::Command::new(&stable).arg("menubar").spawn();
    std::process::exit(0);
}

/// If we're running as the launchd agent, ask launchd to kill+restart the job.
/// Returns true if the kickstart request was issued (so the caller should wait
/// to be replaced rather than self-spawn). Detected via `XPC_SERVICE_NAME`,
/// which launchd sets to the job label for a LaunchAgent — precise enough that a
/// manual `claude-usage menubar` run (which self-spawns fine) isn't misrouted.
fn relaunch_via_launchd() -> bool {
    let under_launchd = std::env::var("XPC_SERVICE_NAME")
        .map(|v| v == crate::LAUNCHD_LABEL)
        .unwrap_or(false);
    if !under_launchd {
        return false;
    }
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/{}", crate::LAUNCHD_LABEL);
    crate::logging::log(&format!("relaunching via launchctl kickstart -k {target}"));
    std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .spawn()
        .is_ok()
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

fn acctview_from_row(r: &Row, active: &Option<String>) -> AcctView {
    AcctView {
        email: r.email.clone(),
        session: r.session.pct,
        weekly: r.weekly.pct,
        opus: r.opus.as_ref().and_then(|c| c.pct),
        session_reset: r.session.resets_in(),
        weekly_reset: r.weekly.resets_in(),
        opus_reset: r.opus.as_ref().map(|c| c.resets_in()).unwrap_or_default(),
        updated: age_str(r.fetched_at),
        active: active.as_deref() == Some(r.email.as_str()),
        has_data: r.has_data(),
    }
}

/// Build the UI snapshot from local State only (no network), reading each
/// account's cached usage. Used after a poll and to react to external changes.
fn build_snapshot() -> Snapshot {
    let st = State::load().unwrap_or_default();
    let autoswap = !st.autoswap_disabled;
    let threshold = st.trigger_pct.unwrap_or(TRIGGER_PCT);
    let active = st.active.clone();
    let accounts = st
        .accounts
        .iter()
        .map(|a| acctview_from_row(&row_from_account(a), &active))
        .collect();
    Snapshot {
        accounts,
        autoswap,
        threshold,
        start_at_login: login_item_enabled(),
    }
}

// ---------------------------------------------------------------------------
// Menu building
// ---------------------------------------------------------------------------

fn build_menu(snap: &Snapshot) -> Menu {
    let menu = Menu::new();

    match snap.accounts.iter().find(|a| a.active) {
        Some(a) => {
            add(&menu, MenuItem::with_id("hdr", header_line(a), false, None));
            if !a.weekly_reset.is_empty() {
                let line = format!("weekly resets in {}", a.weekly_reset);
                add(&menu, MenuItem::with_id("hdr2", line, false, None));
            }
        }
        None => add(
            &menu,
            MenuItem::with_id("hdr", "No active account", false, None),
        ),
    }
    let _ = menu.append(&PredefinedMenuItem::separator());

    if snap.accounts.is_empty() {
        add(
            &menu,
            MenuItem::with_id("none", "Capture a login below to begin", false, None),
        );
    }
    // One submenu per account: switch / stats / remove.
    for a in &snap.accounts {
        // Plain title (native walk re-styles it: bold if active, tab-aligned
        // trailing S%/W%, high percentages colored).
        let head = header_row(a).plain;
        let sub = Submenu::with_id(format!("sub:{}", a.email), head, true);
        if a.active {
            let _ = sub.append(&MenuItem::with_id("noop", "✓ Active", false, None));
        } else {
            let _ = sub.append(&MenuItem::with_id(
                format!("acct:{}", a.email),
                "Switch to this account",
                true,
                None,
            ));
        }
        let _ = sub.append(&PredefinedMenuItem::separator());
        if a.has_data {
            let _ = sub.append(&stat_item("Session", a.session, &a.session_reset));
            let _ = sub.append(&stat_item("Weekly", a.weekly, &a.weekly_reset));
            if a.opus.is_some() {
                let _ = sub.append(&stat_item("Opus", a.opus, &a.opus_reset));
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
        let _ = sub.append(&PredefinedMenuItem::separator());
        let _ = sub.append(&MenuItem::with_id(
            format!("remove:{}", a.email),
            "Remove…",
            true,
            None,
        ));
        let _ = menu.append(&sub);
    }
    let _ = menu.append(&PredefinedMenuItem::separator());

    // Auto-swap: one submenu, Off / 90 / 95 / 98.
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
    // Immediately switch to the account auto-pick considers best right now
    // (soonest-resetting with room), so you burn quota before it resets. Stays
    // put if you're already on the best one.
    let _ = swap.append(&PredefinedMenuItem::separator());
    let _ = swap.append(&MenuItem::with_id(
        "autoswap:now",
        "Switch to best account now",
        true,
        None,
    ));
    let _ = menu.append(&swap);
    let _ = menu.append(&PredefinedMenuItem::separator());

    add(
        &menu,
        MenuItem::with_id("capture", "Capture current login…", true, None),
    );
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

        // Bold marks the active account (in place of a checkmark).
        if style.bold {
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
    fn walk(menu: &NSMenu, styles: &[RowStyle]) {
        for item in menu.itemArray().iter() {
            let title = item.title().to_string();
            if let Some(style) = styles.iter().find(|s| s.plain == title) {
                item.setAttributedTitle(Some(&attributed(style)));
            }
            if let Some(sub) = item.submenu() {
                walk(&sub, styles);
            }
        }
    }

    // SAFETY: called only on the main thread (the run-loop timer), with a live
    // NSMenu pointer from muda's ns_menu() that the tray keeps retained.
    let menu: &NSMenu = unsafe { &*(ns_menu as *const NSMenu) };
    walk(menu, styles);
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
fn menu_signature(snap: &Snapshot) -> String {
    let mut s = String::new();
    for a in &snap.accounts {
        s.push_str(&format!(
            "{}|{}|{}|{}|{}|{}|rs={}|rw={}|u={};",
            a.email,
            a.active,
            a.has_data,
            a.session.map(|v| v.round() as i64).unwrap_or(-1),
            a.weekly.map(|v| v.round() as i64).unwrap_or(-1),
            a.opus.map(|v| v.round() as i64).unwrap_or(-1),
            a.session_reset,
            a.weekly_reset,
            a.updated,
        ));
    }
    s.push_str(&format!(
        "as={} th={:.0} li={}",
        snap.autoswap, snap.threshold, snap.start_at_login
    ));
    s
}

fn header_line(a: &AcctView) -> String {
    format!("{}  ·  {} / {}", a.email, pct(a.session), pct(a.weekly))
}

fn add(menu: &Menu, item: MenuItem) {
    let _ = menu.append(&item);
}

fn add_check(menu: &Menu, item: CheckMenuItem) {
    let _ = menu.append(&item);
}

fn title_for(snap: &Snapshot) -> String {
    match snap.accounts.iter().find(|a| a.active) {
        // Session (5h) matters most day to day; fall back to weekly. Keep the
        // last-known number even if a fetch just failed (cache-backed), so a
        // transient error never blanks the title to "!".
        Some(a) => match a.session.or(a.weekly) {
            Some(p) => format!("{p:.0}%"),
            None => "—".to_string(),
        },
        None => "—".to_string(),
    }
}

fn tooltip_for(snap: &Snapshot) -> String {
    match snap.accounts.iter().find(|a| a.active) {
        Some(a) => format!(
            "{} — session {}, weekly {}",
            a.email,
            pct(a.session),
            pct(a.weekly)
        ),
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

fn handle_click(id: &str) {
    // Actions only mutate state.json; the main-thread timer re-renders from it
    // within ~1s without any network call.
    match id {
        "quit" => std::process::exit(0),
        "noop" => {}
        "autoswap:off" => set_autoswap(false),
        "autoswap:now" => match optimize_now() {
            Ok(Some(email)) => notify(&format!("Switched to {email}")),
            Ok(None) => notify("Already on the best account"),
            Err(e) => notify(&format!("Optimize failed: {e}")),
        },
        "startlogin" => toggle_login_item(),
        "capture" => match capture_current() {
            Ok((email, existed)) => notify(&format!(
                "{} {email}",
                if existed { "Refreshed" } else { "Captured" }
            )),
            Err(e) => notify(&format!("Capture failed: {e}")),
        },
        _ => {
            if let Some(t) = id.strip_prefix("autoswap:") {
                if let Ok(v) = t.parse::<f64>() {
                    set_autoswap_threshold(v);
                }
            } else if let Some(email) = id.strip_prefix("acct:") {
                match switch_to(email) {
                    Ok(label) => notify(&format!("Switched to {label}")),
                    Err(e) => notify(&format!("Switch failed: {e}")),
                }
            } else if let Some(email) = id.strip_prefix("remove:") {
                if confirm(&format!("Remove account {email}? This cannot be undone.")) {
                    match remove_account(email) {
                        Ok(_) => notify(&format!("Removed {email}")),
                        Err(e) => notify(&format!("Remove failed: {e}")),
                    }
                }
            }
        }
    }
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
    let script = if login_item_enabled() {
        format!("tell application \"System Events\" to delete login item \"{LOGIN_ITEM_NAME}\"")
    } else {
        let path = app_path();
        if path.is_empty() {
            return;
        }
        format!(
            "tell application \"System Events\" to make login item at end with properties \
             {{name:\"{LOGIN_ITEM_NAME}\", path:\"{path}\", hidden:true}}"
        )
    };
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(email: &str, session: Option<f64>, weekly: Option<f64>, active: bool) -> AcctView {
        AcctView {
            email: email.to_string(),
            session,
            weekly,
            opus: None,
            session_reset: "3h".into(),
            weekly_reset: "2d".into(),
            opus_reset: String::new(),
            updated: "1m ago".into(),
            active,
            has_data: true,
        }
    }

    #[test]
    fn severity_bands() {
        assert!(severity(None).is_none());
        assert!(severity(Some(0.0)).is_none());
        assert!(severity(Some(79.9)).is_none());
        assert_eq!(severity(Some(80.0)), Some(Severity::Amber));
        assert_eq!(severity(Some(94.9)), Some(Severity::Amber));
        assert_eq!(severity(Some(95.0)), Some(Severity::Red));
        assert_eq!(severity(Some(100.0)), Some(Severity::Red));
    }

    // For ASCII rows, UTF-16 offsets equal byte offsets, so we can slice the
    // plain title to prove each colored span lands exactly on its `NN%`.
    fn span_text(plain: &str, off: usize, len: usize) -> String {
        plain.chars().skip(off).take(len).collect()
    }

    #[test]
    fn header_row_colors_land_on_percentages() {
        let a = acct("you@work.com", Some(82.0), Some(96.0), true);
        let r = header_row(&a);
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
        let r = header_row(&a);
        assert!(!r.bold);
        assert!(r.colors.is_empty());
    }

    #[test]
    fn header_row_offsets_hold_for_unicode_email() {
        // A non-ASCII email must still color the right code-unit range.
        let a = acct("café@x.com", Some(99.0), None, false);
        let r = header_row(&a);
        let (off, len, _) = r.colors[0];
        // Slice by UTF-16 units the way NSRange would.
        let utf16: Vec<u16> = r.plain.encode_utf16().collect();
        let picked = String::from_utf16(&utf16[off..off + len]).unwrap();
        assert_eq!(picked, "99%");
    }

    #[test]
    fn top_header_row_colors_land_on_percentages() {
        let a = acct("you@work.com", Some(50.0), Some(88.0), true);
        let r = top_header_row(&a);
        assert_eq!(r.plain, "you@work.com  ·  50% / 88%");
        assert!(!r.bold, "top info line is not bold");
        assert_eq!(r.tab_x, None);
        // Only weekly (88) is in a band.
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
    fn menu_styles_skips_low_stat_rows_but_keeps_headers() {
        let snap = Snapshot {
            accounts: vec![acct("a@x.com", Some(10.0), Some(20.0), true)],
            autoswap: false,
            threshold: 95.0,
            start_at_login: false,
        };
        let styles = menu_styles(&snap);
        // top header + account header, but no stat rows (all under 80%).
        assert_eq!(styles.len(), 2);
        assert!(styles.iter().all(|s| s.colors.is_empty()));
    }
}
