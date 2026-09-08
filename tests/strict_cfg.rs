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
/// expression-macro form (`cfg!(target_os = "...")`).
const NEEDLES: &[&str] = &["cfg(target_os", "cfg!(target_os"];

/// Documented exceptions. Every entry is `(relative path, line number,
/// rationale)`. Additions require a comment explaining WHY the site can't
/// route through the `Platform` trait yet, and either a linked issue or a
/// note explaining what would need to happen for the exception to go away.
///
/// **Current exceptions — all in `src/main.rs`, all gating the macOS-only
/// menu-bar module and its CLI subcommand.** The Linux + Windows `Platform`
/// impls landed in v0.5.0 (tray backend, secrets, autostart, terminal
/// probe, file dialogs), but wiring `menubar.rs` itself to compile against
/// those backends — instead of straight-lining objc — is a follow-up for
/// v0.5.x. Until that happens, `mod icons` / `mod menubar` / the `menubar`
/// subcommand arm stay macOS-only, and the strict-cfg guard has to
/// acknowledge that explicitly rather than silently accept them.
const ALLOWLIST: &[(&str, u32)] = &[
    // src/main.rs — icons module is macOS-only (bundled provider PNGs are
    // baked into the objc-driven menu; Linux/Windows use tray-icon's own
    // image path when their menubar wiring lands).
    ("src/main.rs", 28),
    // src/main.rs — menubar module is macOS-only until Linux/Windows
    // menubar wiring lands (v0.5.x). Their `Platform` impls exist; the
    // menu-render code paths still call macOS-native NSMenu attributedTitle
    // helpers straight-lined here rather than through the trait.
    ("src/main.rs", 31),
    // src/main.rs — the `menubar` CLI subcommand arm dispatches into the
    // macOS-only menubar module; must be cfg-gated to the same OS to
    // avoid a link-time symbol miss on other platforms.
    ("src/main.rs", 213),
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
