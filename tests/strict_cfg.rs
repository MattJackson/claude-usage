//! Guards the "no scattered `#[cfg(target_os)]` outside `src/platform/*`"
//! rule at `cargo test` time, so a violation is caught locally rather than
//! only in CI. All host-OS-specific behavior must be routed through the
//! `Platform` trait (see `src/platform/mod.rs`) and implemented in
//! `src/platform/{macos,linux,windows}.rs`.
//!
//! This mirrors the CI step in `.github/workflows/ci.yml` ("strict-cfg
//! lint"), which runs the equivalent one-liner:
//!
//! ```sh
//! grep -rn 'cfg(target_os' src/ | grep -v '^src/platform/'
//! ```

use std::fs;
use std::path::{Path, PathBuf};

/// Patterns that indicate a direct OS check. Covers the attribute form
/// (`#[cfg(target_os = "...")]`), nested forms (`#[cfg(all(target_os = ...`,
/// `#[cfg(any(target_os = ...`, `#[cfg(not(target_os = ...`), and the
/// expression-macro form (`cfg!(target_os = "...")`). Each nested wrapper
/// needs its own needle — "cfg(not(target_os" does NOT contain "cfg(target_os"
/// as a contiguous substring (there's a `not(` in between), so a single
/// "cfg(target_os" needle would silently miss it. (Plain `cfg(unix)` /
/// `cfg(windows)` — no `target_os` — are NOT covered here and don't need to
/// be: those are the *sanctioned* per-OS-family escape hatch for code that
/// merely varies by a std-lib-recognized family, not a `Platform`-trait-sized
/// OS branch — see `menubar::relaunch_via_launchd_kickstart`.)
const NEEDLES: &[&str] = &[
    "cfg(target_os",
    "cfg!(target_os",
    "cfg(not(target_os",
    "cfg(all(target_os",
    "cfg(any(target_os",
];

/// Documented exceptions. Every entry is `(relative path, line number,
/// rationale)`. Additions require a comment explaining WHY the site can't
/// route through the `Platform` trait yet, and either a linked issue or a
/// note explaining what would need to happen for the exception to go away.
///
/// **menubar cross-OS wiring**: `src/menubar.rs` builds the tray menu ONCE for
/// every OS — a single `cross_platform::menu_tree_from_snapshot` produces the
/// generic `platform::MenuTree`, and `platform::render::render_menu` turns that
/// into a `muri::compat::tray_icon::menu::Menu` on each platform's UI thread
/// (the old macOS native `NSMenu` styler `mod mac_style` and the separate
/// per-OS menu builders were deleted in the 0.6.0 muri migration). What stays
/// macOS-gated is only the run-loop machinery macOS drives itself: an
/// `NSApplication`/`NSTimer` loop needing `objc2`/`objc2-app-kit`/
/// `objc2-foundation`/`block2` — real
/// `[target.'cfg(target_os = "macos")'.dependencies]` in Cargo.toml that don't
/// exist in the dependency graph on Linux/Windows. The Linux/Windows
/// `MenuBackend` event loop + redraw pump (`cross_platform::run`/`redraw_loop`,
/// the `MenuHandle` channel) is the mirror-image non-macOS gate. Those pieces
/// must be real `#[cfg(target_os = ...)]`, not something routable through a
/// runtime trait call; the menu *content* builders in `mod cross_platform` are
/// NOT gated (they compile everywhere and feed macOS too). Each entry below is
/// grouped to keep this list as short as the dependency-gating requirement
/// allows.
/// Each entry is `(relative path, the trimmed source line the `cfg` guards)` —
/// keyed on the *item* the attribute sits on, NOT a line number, so ordinary
/// edits that shift line numbers don't desync this list (they did, repeatedly).
/// A cfg site is allowed iff the first real line after it (skipping blank /
/// comment / other-attribute lines) matches one of these verbatim. If a gated
/// item's signature genuinely changes, the failure prints the new line to paste
/// here.
const ALLOWLIST: &[(&str, &str)] = &[
    // src/main.rs — the single gated `mod ui;` for the custom-popup NSPopover
    // renderer (docs/design/custom-tray-popup.md, Phase 1). macOS-only AND
    // behind the off-by-default `custom-popup` feature; links `objc2-app-kit`'s
    // NSPopover/NSView (real macOS-only deps), not routable through `Platform`.
    ("src/main.rs", "mod ui;"),
    // src/menubar.rs — `use std::cell::RefCell` (macOS run-loop tick's
    // `last_sig`/`last_title` cells; only used by the macOS `run`).
    ("src/menubar.rs", "use std::cell::RefCell;"),
    // src/menubar.rs — `block2`/`objc2*`/`MenuEvent`/`TrayIconBuilder` imports
    // for the macOS `NSApplication` run loop (real macOS-only deps).
    ("src/menubar.rs", "use {"),
    // src/menubar.rs — `pub fn run()` dispatcher, BOTH branches: non-macOS →
    // `cross_platform::run` (MenuBackend loop); macOS → native `NSApplication`.
    ("src/menubar.rs", "pub fn run() -> Result<()> {"),
    // src/menubar.rs — `build_popover_host`: macOS + `custom-popup`-only
    // (references `crate::ui::popover` / `MainThreadMarker`, macOS-only deps).
    ("src/menubar.rs", "fn build_popover_host("),
    // src/menubar.rs — `popover_model`: macOS + `custom-popup`-only (folds a
    // `Snapshot` into `ui::PopoverModel`, compiled only on macOS under the feature).
    (
        "src/menubar.rs",
        "fn popover_model(snap: &Snapshot) -> crate::ui::PopoverModel {",
    ),
    // src/menubar.rs — inside `mod cross_platform`, `use platform::MenuHandle`:
    // only the non-macOS `run`/`redraw_loop` consume it; macOS drives the
    // handle-free `NSTimer` loop instead.
    ("src/menubar.rs", "use crate::platform::MenuHandle;"),
    // src/menubar.rs — `initial_icon_bytes`: the tray needs decodable icon bytes
    // on Linux/Windows; macOS is happy with a text-only title (non-macOS only).
    (
        "src/menubar.rs",
        "fn initial_icon_bytes(_snap: &Snapshot) -> &'static [u8] {",
    ),
    // src/menubar.rs — `redraw_loop`: the non-macOS background redraw ticker
    // (pushes through the `Send` `MenuHandle`); macOS ticks on its own NSTimer.
    (
        "src/menubar.rs",
        "fn redraw_loop(handle: Box<dyn MenuHandle>, initial: Snapshot) {",
    ),
    // src/menubar.rs — `cross_platform::run`: the Linux/Windows MenuBackend
    // event loop. Never compiled alongside the macOS `NSApplication` run. (The
    // menu *content* builders in this module are NOT gated — they feed macOS too.)
    ("src/menubar.rs", "pub(super) fn run() -> Result<()> {"),
];

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Recursively collect every `.rs` file under `dir`.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("failed to read dir {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_scattered_target_os_cfg_outside_platform_module() {
    let root = src_root();
    let platform_dir = root.join("platform");

    let mut files = Vec::new();
    collect_rs_files(&root, &mut files);

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let allowlist: std::collections::HashSet<(PathBuf, &str)> = ALLOWLIST
        .iter()
        .map(|(rel, guarded)| (manifest_dir.join(rel), *guarded))
        .collect();

    // The first "real" source line at or after `idx` — skipping blank lines,
    // comments, and other attributes (`#[...]`). This is the item a `cfg`
    // attribute guards, and what the allowlist keys on (line-number-independent).
    let guarded_line = |lines: &[&str], idx: usize| -> Option<String> {
        lines[idx..].iter().find_map(|l| {
            let t = l.trim();
            if t.is_empty() || t.starts_with("//") || t.starts_with("#[") || t.starts_with("#!") {
                None
            } else {
                Some(t.to_string())
            }
        })
    };

    let mut violations = Vec::new();
    for path in &files {
        if path.starts_with(&platform_dir) {
            continue;
        }
        let contents = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let lines: Vec<&str> = contents.lines().collect();
        for (lineno, line) in lines.iter().enumerate() {
            // Skip comment lines — a doc-comment example / rationale that
            // MENTIONS `#[cfg(target_os = ...)]` is not itself a violation.
            if line.trim_start().starts_with("//") {
                continue;
            }
            if NEEDLES.iter().any(|needle| line.contains(needle)) {
                let guarded = guarded_line(&lines, lineno + 1).unwrap_or_default();
                if allowlist.contains(&(path.clone(), guarded.as_str())) {
                    continue;
                }
                violations.push(format!(
                    "{}:{}: {}  (guards: {})",
                    path.display(),
                    lineno + 1,
                    line.trim(),
                    guarded
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "found #[cfg(target_os)] / cfg!(target_os) outside src/platform/ — \
         route platform-specific behavior through the Platform trait \
         (src/platform/mod.rs) instead:\n{}",
        violations.join("\n")
    );
}
