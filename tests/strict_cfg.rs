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

    let mut violations = Vec::new();
    for path in &files {
        if path.starts_with(&platform_dir) {
            continue;
        }
        let contents = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        for (lineno, line) in contents.lines().enumerate() {
            if NEEDLES.iter().any(|needle| line.contains(needle)) {
                violations.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    lineno + 1,
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
