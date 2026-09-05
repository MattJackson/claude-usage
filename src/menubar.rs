//! macOS menu-bar app: shows the active account's usage in the status bar and
//! lets you switch / capture / remove accounts and set the auto-swap threshold
//! from a dropdown. The same `watch_cycle` that powers `claude-usage watch` runs
//! on a background thread here, so the daemon behaviour is identical.
//!
//! Usage numbers come from the cache (written by the scheduler poll); the UI
//! never fetches on its own, so menu interactions can't trigger HTTP 429s.

use anyhow::Result;
use std::cell::RefCell;
use std::sync::mpsc::{self, RecvTimeoutError};
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

pub fn run() -> Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("the menu bar must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    // Background (menu-bar-only) app: no Dock icon, even as the bare binary.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // Poll + auto-swap on a background thread. It writes cached usage to
    // state.json; the main-thread timer reads it back to render.
    let (poll_tx, poll_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || poll_loop(poll_rx));

    // Build the tray on the main thread and keep it alive for the app's lifetime.
    let initial = build_snapshot();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(build_menu(&initial)))
        .with_title(title_for(&initial))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to create tray icon: {e}"))?;
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
        // A menu closes before its click is delivered, so handling it here won't
        // fight menu tracking.
        while let Ok(ev) = menu_rx.try_recv() {
            handle_click(&ev.id.0, &poll_tx);
        }
        let snap = build_snapshot();
        let sig = menu_signature(&snap);
        if *last_sig.borrow() != sig {
            tray.set_menu(Some(Box::new(build_menu(&snap))));
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

fn poll_loop(poll_rx: mpsc::Receiver<()>) {
    let mut guard = SwapGuard::default();
    let base = WATCH_INTERVAL_SECS;
    let mut current = base;
    loop {
        // Fetch usage + auto-swap; this writes cached usage to state.json, which
        // the main-thread timer reads back to render. "Refresh now" pokes poll_rx.
        let rate_limited = run_cycle(&mut guard);
        current = next_interval(current, base, rate_limited);
        match poll_rx.recv_timeout(Duration::from_secs(current)) {
            Ok(_) | Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
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
        let head = format!(
            "{}{}   {} / {}",
            if a.active { "✓ " } else { "" },
            a.email,
            pct(a.session),
            pct(a.weekly)
        );
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
    add(
        &menu,
        MenuItem::with_id("refresh", "Refresh now", true, None),
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

fn stat_item(label: &str, pct_val: Option<f64>, reset: &str) -> MenuItem {
    let text = if reset.is_empty() {
        format!("{label}  {}", pct(pct_val))
    } else {
        format!("{label}  {}  · resets in {reset}", pct(pct_val))
    };
    MenuItem::with_id("noop", text, false, None)
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

fn handle_click(id: &str, poll_tx: &mpsc::Sender<()>) {
    // Most actions only mutate state.json; the main-thread timer re-renders from
    // it within ~1s without any network call. Only "Refresh now" forces a poll.
    match id {
        "quit" => std::process::exit(0),
        "noop" => {}
        "refresh" => {
            let _ = poll_tx.send(());
        }
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
