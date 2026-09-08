//! Linux-only GUI smoke test: proves the real tray backend actually
//! initializes at runtime, not just compiles.
//!
//! `cargo build`/`cargo test` on Linux link `libayatana-appindicator3` (via
//! the `tray-icon` crate) but never actually call into it — every unit test
//! in `src/platform/linux.rs` exercises pure Rust logic (menu-tree
//! translation, the Secret Service fallback, XDG path resolution) with the
//! real GTK/appindicator calls behind trait seams that tests substitute
//! away. That leaves an entire class of bug unverified: the runtime
//! GTK/appindicator shared library being missing, mismatched, or ABI-broken
//! on the actual CI image, which only a real `gtk::init()` +
//! `tray_icon::TrayIcon::new(...)` call would surface.
//!
//! This test does that: initializes GTK, builds a real `TrayIcon`, mutates
//! its icon and menu, then drops it — asserting no panic and no `Err`
//! anywhere along the way. It does NOT prove the tray is visually rendered
//! (impossible headless without a pixel diff) — passing means the GTK loop
//! actually started and `libayatana-appindicator3` linked and ran, which
//! eliminates the "code compiled but the runtime library is missing/wrong
//! version" class of bug.
//!
//! `#[ignore]`d by default so a local `cargo test` (which may have no X
//! server / GTK runtime at all) doesn't fail or hang. CI always runs it
//! explicitly under a virtual X server:
//!
//! ```sh
//! sudo apt-get install -y xvfb
//! xvfb-run --auto-servernum cargo test --test integration_tray --all-features -- --ignored --test-threads=1
//! ```
//!
//! See the "Linux tray init smoke test" step in `.github/workflows/ci.yml`.

#![cfg(target_os = "linux")]

use tray_icon::menu::{Menu, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};

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

    // 2. tray_icon::TrayIcon::new(...) — via the builder — actually returns
    //    Ok. On Linux this is backed by libayatana-appindicator3; a missing
    //    or ABI-incompatible copy of that library surfaces here, not at
    //    `cargo build` time.
    let tray = TrayIconBuilder::new()
        .with_title("usagio-ci-smoke")
        .with_menu(Box::new(Menu::new()))
        .with_icon(one_pixel_icon())
        .build()
        .expect("TrayIconBuilder::build() failed — libayatana-appindicator3 missing/incompatible");

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
