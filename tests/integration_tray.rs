//! Linux-only GUI smoke test: proves the real tray backend actually
//! initializes at runtime, not just compiles.
//!
//! As of the 0.6.0 muri migration the Linux tray is muri's `muda-compat`
//! facade over a **ksni StatusNotifierItem** (pure-Rust D-Bus — no
//! `libayatana-appindicator3`, no libdbus). usagio still drives a **GTK glib
//! main loop** as its own event pump (see `src/platform/linux.rs`), and the
//! muri compat `TrayIcon` is built/mutated from that thread. Every unit test
//! exercises pure Rust logic with the GTK/ksni calls behind trait seams that
//! tests substitute away, so this is the only check that the two real runtime
//! dependencies actually work: `gtk::init()` succeeds, and muri's ksni tray
//! registers on the **session bus**.
//!
//! This test does that: initializes GTK, builds a real `muri::compat`
//! `TrayIcon` (which spawns the ksni SNI service), mutates its icon and menu,
//! then drops it — asserting no panic and no `Err` anywhere. It does NOT prove
//! the tray is visually rendered (impossible headless without a pixel diff).
//!
//! `#[ignore]`d by default so a local `cargo test` (which may have no X server
//! or session bus at all) doesn't fail or hang. CI runs it explicitly under a
//! virtual X server (for `gtk::init`) AND a throwaway session bus (for ksni):
//!
//! ```sh
//! sudo apt-get install -y xvfb dbus-x11
//! xvfb-run --auto-servernum dbus-run-session -- \
//!   cargo test --test integration_tray --all-features -- --ignored --test-threads=1
//! ```
//!
//! See the "Linux tray init smoke test" step in `.github/workflows/ci.yml`.

#![cfg(target_os = "linux")]

use muri::compat::tray_icon::menu::{Menu, MenuItem};
use muri::compat::tray_icon::{Icon, TrayIconBuilder};

/// A single opaque red pixel — the smallest valid `Icon::from_rgba` input.
/// Doesn't need to look like anything; this test only cares that the tray
/// backend accepts and displays *an* icon, not which one.
fn one_pixel_icon() -> Icon {
    Icon::from_rgba(vec![255, 0, 0, 255], 1, 1).expect("building a 1x1 RGBA tray icon")
}

#[test]
#[ignore = "requires a real (or Xvfb) X server + GTK/libayatana-appindicator3; run with `cargo test --all-features -- --ignored`, or under CI's Ubuntu-only xvfb-run step"]
fn tray_icon_initializes_sets_icon_and_menu_then_tears_down_cleanly() {
    // 1. GTK actually starts. This is the first real runtime dependency:
    //    without a display (or Xvfb providing one), or without the GTK
    //    shared libraries actually installed, this call itself fails.
    gtk::init().expect("gtk::init() failed — GTK runtime missing or no display available");

    // 2. muri's compat TrayIconBuilder::build() — spawns the ksni
    //    StatusNotifierItem service, which connects to the D-Bus session bus
    //    and registers the item. A missing/unreachable session bus surfaces
    //    here, not at `cargo build` time (CI wraps the run in dbus-run-session).
    let tray = TrayIconBuilder::new()
        .with_title("usagio-ci-smoke")
        .with_menu(Box::new(Menu::new()))
        .with_icon(one_pixel_icon())
        .build()
        .expect("TrayIconBuilder::build() failed — ksni/D-Bus session bus unavailable?");

    // 3. Set a real icon post-construction (the same call path
    //    `LinuxMenu::apply_handle_msg` uses for `MenuHandle::set_icon`).
    tray.set_icon(Some(one_pixel_icon()))
        .expect("TrayIcon::set_icon failed");

    // 4. Set a real (non-empty) menu (the same call path
    //    `LinuxMenu::apply_handle_msg` uses for `MenuHandle::set_menu`).
    let menu = Menu::new();
    let item = MenuItem::new("usagio smoke test item", true, None);
    menu.append(&item).expect("Menu::append failed");
    tray.set_menu(Some(Box::new(menu)));

    // 5. Teardown cleanly: dropping the TrayIcon must not panic or abort.
    drop(tray);
}
