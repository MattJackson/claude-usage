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
/// **menubar cross-OS wiring**: `src/menubar.rs` renders on all three OSes
/// through the SAME muri `muda-compat` facade — the tray menu is a
/// `muri::compat::tray_icon::menu::Menu` on every platform (the old macOS
/// native `NSMenu` styler `mod mac_style` was deleted in the 0.6.0 muri
/// migration). What stays macOS-gated is the run-loop machinery macOS drives
/// itself: an `NSApplication`/`NSTimer` loop needing
/// `objc2`/`objc2-app-kit`/`objc2-foundation`/`block2` — real
/// `[target.'cfg(target_os = "macos")'.dependencies]` in Cargo.toml that don't
/// exist in the dependency graph on Linux/Windows — plus the macOS `build_menu`
/// tree builders (Linux/Windows build a generic `platform::MenuTree` via
/// `cross_platform` instead). Those pieces must be real
/// `#[cfg(target_os = "macos")]`, not something routable through a runtime
/// trait call. Each entry below is grouped (one `mod`/fn per macOS-only or
/// non-macOS-only cluster) to keep this list as short as the dependency-gating
/// requirement allows.
const ALLOWLIST: &[(&str, u32)] = &[
    // src/main.rs — the single gated `mod ui;` for the custom-popup NSPopover
    // renderer (docs/design/custom-tray-popup.md, Phase 1). The `src/ui/`
    // module is macOS-only AND behind the off-by-default `custom-popup`
    // feature; gating the whole module at this one `mod` line (rather than
    // scattering `cfg`s through `src/ui/*`) is the structure the design
    // mandates. It isn't routable through the `Platform` trait: the module
    // links `objc2-app-kit`'s NSPopover/NSView classes, real
    // `[target.'cfg(target_os = "macos")'.dependencies]` absent from the
    // Linux/Windows dependency graph. Goes away when the popover becomes the
    // macOS default and the feature gate is dropped (Phase 2/3).
    ("src/main.rs", 32),
    // src/menubar.rs — `use std::cell::RefCell` (macOS run-loop tick's
    // `last_sig`/`last_title` cells; only used by the macOS `run`).
    ("src/menubar.rs", 15),
    // src/menubar.rs — `block2`/`objc2`/`objc2-app-kit`/`objc2-foundation` +
    // `muri::compat::tray_icon` imports for the macOS run loop and menu build.
    ("src/menubar.rs", 30),
    // src/menubar.rs — `pub fn run()`, non-macOS branch: dispatches to
    // `cross_platform::run` (the `platform::MenuBackend`-based renderer).
    ("src/menubar.rs", 583),
    // src/menubar.rs — `pub fn run()`, macOS branch: the native
    // `NSApplication` run loop driving a passive muri tray.
    ("src/menubar.rs", 588),
    // src/menubar.rs — `build_popover_host`: macOS + `custom-popup`-only.
    // Inert under the muri backend (muri exposes no NSStatusItem anchor), but
    // still references `crate::ui::popover::PopoverHost` / `MainThreadMarker`
    // (macOS-only deps). Off-by-default feature. Goes away when the popover is
    // the macOS default (or muri grows a status-item anchor).
    ("src/menubar.rs", 715),
    // src/menubar.rs — `popover_model`: macOS + `custom-popup`-only. Folds a
    // `Snapshot` into the toolkit-neutral `ui::PopoverModel` (references
    // `crate::ui`, compiled only on macOS under `custom-popup`).
    ("src/menubar.rs", 741),
    // src/menubar.rs — `menu_label`: macOS-only helper that strips the `\t`
    // right-align marker (muri's facade has no tab stops) — the macOS
    // counterpart to `cross_platform::plain_text`.
    ("src/menubar.rs", 1556),
    // src/menubar.rs — `decode_menu_icon`: macOS-only PNG→muri `Icon` decoder
    // for provider group-header icons (counterpart to the per-OS decoders in
    // `src/platform/{linux,windows}.rs`).
    ("src/menubar.rs", 1566),
    // src/menubar.rs — `build_menu`: builds the macOS muri
    // `muri::compat::tray_icon::menu::Menu` handed to `tray.set_menu`.
    // Linux/Windows build the equivalent `platform::MenuTree` via
    // `cross_platform::menu_tree_from_snapshot`.
    ("src/menubar.rs", 1580),
    // src/menubar.rs — `build_provider_group`: macOS-only counterpart to
    // `cross_platform::build_provider_group_item` (v0.5.3 menu redesign).
    ("src/menubar.rs", 1886),
    // src/menubar.rs — `build_account_submenu`: macOS-only counterpart to
    // `cross_platform::build_account_submenu_item` (v0.5.3 menu redesign).
    ("src/menubar.rs", 1932),
    // src/menubar.rs — `add`: `build_menu`/`build_provider_group`/
    // `build_account_submenu` helper (`muri::compat::…::Menu::append`).
    ("src/menubar.rs", 2101),
    // src/menubar.rs — `mod cross_platform`: the Linux/Windows renderer
    // (`platform::MenuBackend`-based). Never compiled alongside the macOS run.
    ("src/menubar.rs", 2693),
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
    let allowlist: std::collections::HashSet<(PathBuf, u32)> = ALLOWLIST
        .iter()
        .map(|(rel, line)| (manifest_dir.join(rel), *line))
        .collect();

    let mut violations = Vec::new();
    for path in &files {
        if path.starts_with(&platform_dir) {
            continue;
        }
        let contents = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        for (lineno, line) in contents.lines().enumerate() {
            // Skip comment lines — a doc-comment example / rationale that
            // MENTIONS `#[cfg(target_os = ...)]` is not itself a violation.
            let stripped = line.trim_start();
            if stripped.starts_with("//") {
                continue;
            }
            if NEEDLES.iter().any(|needle| line.contains(needle)) {
                let line_1based = (lineno + 1) as u32;
                if allowlist.contains(&(path.clone(), line_1based)) {
                    continue;
                }
                violations.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    line_1based,
                    line.trim()
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
