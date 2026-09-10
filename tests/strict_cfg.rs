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
/// **v0.5.x menubar cross-OS wiring**: `src/menubar.rs` now renders on all
/// three OSes — macOS keeps its native NSMenu attributedTitle renderer
/// (`mac_style` module + `run`'s macOS branch); Linux/Windows route through
/// `platform::MenuBackend` instead (`cross_platform` module). The macOS-only
/// renderer needs `objc2`/`objc2-app-kit`/`objc2-foundation`/`block2` —
/// real `[target.'cfg(target_os = "macos")'.dependencies]` in Cargo.toml
/// that don't exist in the dependency graph on Linux/Windows — so those
/// pieces must be real `#[cfg(target_os = "macos")]`, not something
/// routable through a runtime trait call. Each entry below is grouped (one
/// `mod`/fn per macOS-only or non-macOS-only cluster) to keep this list as
/// short as the dependency-gating requirement allows.
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
    // src/main_tests.rs — `apply_account_rolls_back_claude_json_when_keychain_write_fails`:
    // the keychain-write-failure rollback test (v0.5.17 codeaudit #11). macOS-
    // only because it drives the Claude keychain path through the macOS mock's
    // failure-injection seam (`crate::platform::arm_keychain_set_failure`), which
    // only exists under `cfg(all(test, target_os = "macos"))`.
    ("src/main_tests.rs", 1820),
    // src/menubar.rs — `use std::cell::RefCell` (macOS thread-local NSMenu
    // context storage; only reachable via mac_style).
    ("src/menubar.rs", 15),
    // src/menubar.rs — `objc2`/`objc2-app-kit`/`objc2-foundation`/`block2`
    // imports for the macOS-only NSMenu renderer (see module doc above).
    ("src/menubar.rs", 29),
    // src/menubar.rs — `pub fn run()`, non-macOS branch: dispatches to
    // `cross_platform::run` (the `platform::MenuBackend`-based renderer).
    ("src/menubar.rs", 610),
    // src/menubar.rs — `pub fn run()`, macOS branch: the native
    // `NSApplication` run loop.
    ("src/menubar.rs", 615),
    // src/menubar.rs — `build_popover_host`: macOS + `custom-popup`-only.
    // Builds the `ui::popover::PopoverHost` anchored to the NSStatusItem
    // button (links `objc2-app-kit`'s NSPopover/NSStatusItem, macOS-only
    // deps). Gated on the `custom-popup` feature too, so it's inert in the
    // shipping default build. Goes away when the popover is the macOS default.
    ("src/menubar.rs", 741),
    // src/menubar.rs — `popover_model`: macOS + `custom-popup`-only. Folds a
    // `Snapshot` into the toolkit-neutral `ui::PopoverModel` (references
    // `crate::ui`, compiled only on macOS under `custom-popup`).
    ("src/menubar.rs", 775),
    // src/menubar.rs — `build_menu`: builds a native `tray_icon::menu::Menu`
    // for the macOS `apply_menu_styles` NSMenu walk to mutate in place.
    // Linux/Windows build the equivalent `platform::MenuTree` via
    // `cross_platform::menu_tree_from_snapshot`.
    ("src/menubar.rs", 1585),
    // src/menubar.rs — `build_provider_group`: macOS-only counterpart to
    // `cross_platform::build_provider_group_item` (v0.5.3 menu redesign).
    ("src/menubar.rs", 1896),
    // src/menubar.rs — `build_account_submenu`: macOS-only counterpart to
    // `cross_platform::build_account_submenu_item` (v0.5.3 menu redesign).
    ("src/menubar.rs", 1924),
    // src/menubar.rs — `mod mac_style`: the NSMenu attributedTitle styling
    // walk (`install_menu`/`color_for`/`attributed`/`apply_menu_styles`) plus
    // the `objc2*` imports it needs. Grouped into one module so this whole
    // cluster needs exactly one cfg site instead of one per function.
    ("src/menubar.rs", 1994),
    // src/menubar.rs — `add`: `build_menu`/`build_provider_group`/
    // `build_account_submenu` helper (native `tray_icon::menu::Menu::append`).
    ("src/menubar.rs", 2503),
    // src/menubar.rs — `mod cross_platform`: the Linux/Windows renderer
    // (`platform::MenuBackend`-based). Never compiled alongside `mac_style`.
    ("src/menubar.rs", 3079),
    // src/menubar.rs — `tests::mac_style_tests`: exercises
    // `mac_style::attributed` (NSAttributedString attribute inspection)
    // directly; needs the same `objc2*` crates as `mac_style` itself.
    ("src/menubar.rs", 5050),
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
