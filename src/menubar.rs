//! macOS menu-bar app: shows the active account's usage in the status bar and
//! lets you switch accounts / toggle the auto-swap daemon from a dropdown. The
//! same `watch_cycle` that powers `claude-usage watch` runs on a background
//! thread here, so the daemon behaviour is identical.

use anyhow::Result;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::store::{Account, State};
use crate::{
    keychain_read, notify, perform_switch, watch_cycle, SwapGuard, TARGET_CEILING_PCT, TRIGGER_PCT,
    WATCH_INTERVAL_SECS,
};

/// Name shown for our Login Item in System Events.
const LOGIN_ITEM_NAME: &str = "Claude Usage";

/// Events delivered to the main thread's run loop.
enum UserEvent {
    /// The poller refreshed the snapshot; rebuild the menu/title.
    Refresh,
    /// A menu item with this id was clicked.
    Menu(String),
}

/// One account as shown in the menu.
struct AcctView {
    name: String,
    label: String,
    session: Option<f64>,
    weekly: Option<f64>,
    resets_in: String,
    active: bool,
    max_pct: f64,
    error: Option<String>,
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
    // The bundled .app sets LSUIElement so there's no Dock icon; when run as a
    // bare binary a Dock icon may appear, which is fine for development.
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Forward menu clicks into the run loop.
    {
        let proxy = proxy.clone();
        let rx = MenuEvent::receiver();
        std::thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                let _ = proxy.send_event(UserEvent::Menu(ev.id.0));
            }
        });
    }

    let snapshot: Arc<Mutex<Snapshot>> = Arc::new(Mutex::new(Snapshot::default()));
    let (poll_tx, poll_rx) = mpsc::channel::<()>();

    // Poll + auto-swap on a background thread.
    {
        let snapshot = snapshot.clone();
        let proxy = proxy.clone();
        std::thread::spawn(move || poll_loop(snapshot, proxy, poll_rx));
    }

    // React quickly to external State changes (e.g. a CLI `claude-usage switch`)
    // by watching the state file's mtime and doing a local-only refresh.
    {
        let snapshot = snapshot.clone();
        let proxy = proxy.clone();
        std::thread::spawn(move || watch_state_file(snapshot, proxy));
    }

    // The tray must be created on the main thread once the app is initialised.
    let mut tray: Option<TrayIcon> = None;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                let snap = snapshot.lock().unwrap();
                let built = TrayIconBuilder::new()
                    .with_menu(Box::new(build_menu(&snap)))
                    .with_title(title_for(&snap))
                    .build();
                match built {
                    Ok(t) => {
                        let _ = t.set_tooltip(Some(tooltip_for(&snap)));
                        tray = Some(t);
                    }
                    Err(e) => eprintln!("failed to create tray icon: {e}"),
                }
            }
            Event::UserEvent(UserEvent::Refresh) => {
                if let Some(t) = &tray {
                    let snap = snapshot.lock().unwrap();
                    t.set_menu(Some(Box::new(build_menu(&snap))));
                    t.set_title(Some(title_for(&snap)));
                    let _ = t.set_tooltip(Some(tooltip_for(&snap)));
                }
            }
            Event::UserEvent(UserEvent::Menu(id)) => handle_click(&id, &poll_tx, control_flow),
            _ => {}
        }
    });
}

// ---------------------------------------------------------------------------
// Poller thread
// ---------------------------------------------------------------------------

fn poll_loop(
    snapshot: Arc<Mutex<Snapshot>>,
    proxy: EventLoopProxy<UserEvent>,
    poll_rx: mpsc::Receiver<()>,
) {
    let mut guard = SwapGuard::default();
    loop {
        let snap = do_poll(&mut guard);
        if let Ok(mut s) = snapshot.lock() {
            *s = snap;
        }
        let _ = proxy.send_event(UserEvent::Refresh);
        // Wake early if the UI requested an immediate refresh.
        match poll_rx.recv_timeout(Duration::from_secs(WATCH_INTERVAL_SECS)) {
            Ok(_) | Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn do_poll(guard: &mut SwapGuard) -> Snapshot {
    let st = State::load().unwrap_or_default();
    let autoswap = !st.autoswap_disabled;
    let threshold = st.trigger_pct.unwrap_or(TRIGGER_PCT);
    // With auto-swap off, use an unreachable trigger so we only observe.
    let trigger = if autoswap { threshold } else { 101.0 };
    let start_at_login = login_item_enabled();

    let (rows, active) = match watch_cycle(trigger, TARGET_CEILING_PCT, guard) {
        Ok(o) => (o.rows, o.active),
        Err(e) => {
            notify(&format!("usage poll failed: {e}"));
            (Vec::new(), None)
        }
    };

    let accounts = rows
        .iter()
        .map(|r| AcctView {
            name: r.name.clone(),
            label: if r.email.is_empty() {
                r.name.clone()
            } else {
                r.email.clone()
            },
            session: r.session.pct,
            weekly: r.weekly.pct,
            resets_in: r.weekly.resets_in(),
            active: active.as_deref() == Some(r.name.as_str()),
            max_pct: r.max_pct(),
            error: r.error.clone(),
        })
        .collect();

    Snapshot {
        accounts,
        autoswap,
        threshold,
        start_at_login,
    }
}

/// Rebuild the snapshot from local State only (no network), carrying over
/// last-known usage numbers from `prev`. Used to react instantly to external
/// state changes such as a CLI `claude-usage switch`.
fn light_snapshot(prev: &Snapshot) -> Snapshot {
    let st = State::load().unwrap_or_default();
    let autoswap = !st.autoswap_disabled;
    let threshold = st.trigger_pct.unwrap_or(TRIGGER_PCT);
    let accounts = st
        .accounts
        .iter()
        .map(|a| {
            let prior = prev.accounts.iter().find(|p| p.name == a.name);
            AcctView {
                name: a.name.clone(),
                label: a.email.clone().unwrap_or_else(|| a.name.clone()),
                session: prior.and_then(|p| p.session),
                weekly: prior.and_then(|p| p.weekly),
                resets_in: prior.map(|p| p.resets_in.clone()).unwrap_or_default(),
                active: st.active.as_deref() == Some(a.name.as_str()),
                max_pct: prior.map(|p| p.max_pct).unwrap_or(0.0),
                error: prior.and_then(|p| p.error.clone()),
            }
        })
        .collect();

    Snapshot {
        accounts,
        autoswap,
        threshold,
        // Reflect the actual registered login-item state, not a stale copy.
        start_at_login: login_item_enabled(),
    }
}

/// Watch the state file's mtime; on change, do a local-only refresh so a CLI
/// switch shows in the menu within a second or two.
fn watch_state_file(snapshot: Arc<Mutex<Snapshot>>, proxy: EventLoopProxy<UserEvent>) {
    let path = match crate::store::config_dir() {
        Ok(d) => d.join("state.json"),
        Err(_) => return,
    };
    let mtime = || std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut last = mtime();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let cur = mtime();
        if cur != last {
            last = cur;
            if let Ok(mut s) = snapshot.lock() {
                let new = light_snapshot(&s);
                *s = new;
            }
            let _ = proxy.send_event(UserEvent::Refresh);
        }
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
            if !a.resets_in.is_empty() {
                let line = format!("weekly resets in {}", a.resets_in);
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
    for a in &snap.accounts {
        let usage = match &a.error {
            Some(_) => "error".to_string(),
            None => format!("S {} / W {}", pct(a.session), pct(a.weekly)),
        };
        let text = format!("{}   {}", a.label, usage);
        add_check(
            &menu,
            CheckMenuItem::with_id(format!("acct:{}", a.name), text, true, a.active, None),
        );
    }
    let _ = menu.append(&PredefinedMenuItem::separator());

    add_check(
        &menu,
        CheckMenuItem::with_id(
            "autoswap",
            "Auto-swap at high usage",
            true,
            snap.autoswap,
            None,
        ),
    );
    let sub = Submenu::with_id(
        "thresh",
        format!("Swap threshold: {:.0}%", snap.threshold),
        true,
    );
    for t in [90i32, 95, 98] {
        let checked = snap.threshold.round() as i32 == t;
        let _ = sub.append(&CheckMenuItem::with_id(
            format!("thresh:{t}"),
            format!("{t}%"),
            true,
            checked,
            None,
        ));
    }
    let _ = menu.append(&sub);
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
    add(
        &menu,
        MenuItem::with_id("quit", "Quit claude-usage", true, None),
    );

    menu
}

fn header_line(a: &AcctView) -> String {
    format!("{}  ·  S {} / W {}", a.label, pct(a.session), pct(a.weekly))
}

fn add(menu: &Menu, item: MenuItem) {
    let _ = menu.append(&item);
}

fn add_check(menu: &Menu, item: CheckMenuItem) {
    let _ = menu.append(&item);
}

fn title_for(snap: &Snapshot) -> String {
    match snap.accounts.iter().find(|a| a.active) {
        // Session (5h) matters most day to day; fall back to weekly, then max.
        Some(a) if a.error.is_none() => {
            let p = a.session.or(a.weekly).unwrap_or(a.max_pct);
            format!("{p:.0}%")
        }
        Some(_) => "!".to_string(),
        None => "—".to_string(),
    }
}

fn tooltip_for(snap: &Snapshot) -> String {
    match snap.accounts.iter().find(|a| a.active) {
        Some(a) => format!(
            "{} — session {}, weekly {}",
            a.label,
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

fn handle_click(id: &str, poll_tx: &mpsc::Sender<()>, control_flow: &mut ControlFlow) {
    match id {
        "quit" => *control_flow = ControlFlow::Exit,
        "refresh" => {
            let _ = poll_tx.send(());
        }
        "autoswap" => {
            toggle_autoswap();
            let _ = poll_tx.send(());
        }
        "startlogin" => {
            toggle_login_item();
            let _ = poll_tx.send(());
        }
        "capture" => {
            if let Some(name) = ask_name() {
                if let Err(e) = do_capture(&name) {
                    notify(&format!("Capture failed: {e}"));
                }
            }
            let _ = poll_tx.send(());
        }
        _ => {
            if let Some(t) = id.strip_prefix("thresh:") {
                if let Ok(v) = t.parse::<f64>() {
                    set_threshold(v);
                    let _ = poll_tx.send(());
                }
            } else if let Some(name) = id.strip_prefix("acct:") {
                switch_to(name);
                let _ = poll_tx.send(());
            }
        }
    }
}

fn toggle_autoswap() {
    if let Ok(mut st) = State::load() {
        st.autoswap_disabled = !st.autoswap_disabled;
        let _ = st.save();
    }
}

fn set_threshold(v: f64) {
    if let Ok(mut st) = State::load() {
        st.trigger_pct = Some(v);
        let _ = st.save();
    }
}

fn switch_to(name: &str) {
    if let Ok(mut st) = State::load() {
        match perform_switch(&mut st, name) {
            Ok(label) => notify(&format!("Switched to {label}")),
            Err(e) => notify(&format!("Switch failed: {e}")),
        }
    }
}

fn do_capture(name: &str) -> Result<()> {
    let blob = keychain_read()
        .ok_or_else(|| anyhow::anyhow!("no claude.ai login found in the keychain"))?;
    let mut acct = Account::from_keychain_blob(name.to_string(), &blob)?;
    acct.email = crate::usage::fetch_email(&acct.access_token);
    let label = acct.email.clone().unwrap_or_else(|| name.to_string());
    let mut st = State::load()?;
    st.upsert(acct);
    st.active = Some(name.to_string());
    st.save()?;
    notify(&format!("Captured {label}"));
    Ok(())
}

/// Ask for an account name with a native dialog. None if cancelled/empty.
fn ask_name() -> Option<String> {
    let out = std::process::Command::new("osascript")
        .arg("-e")
        .arg("display dialog \"Name this account (e.g. work, personal):\" default answer \"\" with title \"claude-usage\"")
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // cancelled
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let name = s.split("text returned:").nth(1)?.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
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
