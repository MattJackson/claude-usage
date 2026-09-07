//! One-shot migration of the on-disk config directory when the binary is
//! renamed (e.g. `claude-usage` → `usagio`). Called once early in `main()`;
//! idempotent and safe on every subsequent boot.
//!
//! The macOS Keychain service string is deliberately NOT migrated here — it
//! stays `"claude-usage"` so existing captured tokens continue to unlock
//! without any user action. Rekeying the keychain would strand every token
//! that was captured before the rename. When we eventually want to move the
//! keychain namespace, it needs its own separate, opt-in migration path.

use std::io;
use std::path::{Path, PathBuf};

// -------------------------------------------------------------------------
// Public API
// -------------------------------------------------------------------------

#[derive(Debug)]
pub enum MigrationResult {
    /// Neither the old nor new config directory existed. Fresh install.
    FreshInstall,
    /// The new directory already exists; nothing to do.
    AlreadyMigrated,
    /// The old directory existed and was moved to the new location.
    Migrated { from: PathBuf, to: PathBuf },
    /// Both directories exist. The new one is left as the source of truth;
    /// the old one is left untouched on disk for the user to inspect.
    BothExisted { new: PathBuf, old: PathBuf },
}

// The inner payloads of these variants are consumed via the `Debug` impl in
// `main.rs`'s eprintln! ("migration skipped ({e:?})") and, for
// `OldRemovalFailed`, by pattern-match in the H6/M1 dispatch that treats the
// new tree as populated. Suppress the "field never read" lint file-locally.
#[allow(dead_code)]
#[derive(Debug)]
pub enum MigrationError {
    /// Neither `$XDG_CONFIG_HOME` nor `$HOME` resolved to a usable base dir.
    NoConfigBase,
    /// Creating the parent config base (usually `~/.config/`) failed.
    ParentCreationFailed(io::Error),
    /// The primary `fs::rename` failed AND the copy-and-delete fallback also
    /// failed (or was not attempted because the error was not `EXDEV`).
    RenameFailed(io::Error),
    /// A recursive copy attempted as an EXDEV fallback failed partway.
    CopyFallbackFailed(io::Error),
    /// The recursive copy succeeded but removing the old tree failed. The new
    /// tree IS populated — the caller can proceed and the leftover old tree
    /// is safe to ignore.
    OldRemovalFailed { new: PathBuf, source: io::Error },
}

/// Default entry point — uses `~/.config/claude-usage` and `~/.config/usagio`.
/// L9 (round-1 codeaudit): the two slug names come from the module-level
/// constants in `main.rs` (`LEGACY_APP_SLUG` and `APP_SLUG`) so renaming the
/// app is a single-point edit.
pub fn migrate_config_dir_if_needed() -> Result<MigrationResult, MigrationError> {
    let base = config_base()?;
    migrate_between(
        &base.join(crate::LEGACY_APP_SLUG),
        &base.join(crate::APP_SLUG),
    )
}

/// Test-friendly form: caller supplies the exact old and new paths.
pub fn migrate_between(old: &Path, new: &Path) -> Result<MigrationResult, MigrationError> {
    // Use symlink_metadata so a symlink at `old` is treated as the entity
    // being moved (we rename the link itself, not chase its target).
    let old_exists = old.symlink_metadata().is_ok();
    let new_exists = new.symlink_metadata().is_ok();

    match (old_exists, new_exists) {
        (false, false) => Ok(MigrationResult::FreshInstall),
        (true, true) => {
            // Caller (main.rs) prints the user-facing warning with more
            // context. Keep this branch silent so users don't see two
            // near-identical lines about the same event.
            Ok(MigrationResult::BothExisted {
                new: new.to_path_buf(),
                old: old.to_path_buf(),
            })
        }
        (false, true) => Ok(MigrationResult::AlreadyMigrated),
        (true, false) => {
            // Ensure the parent exists before renaming into it.
            if let Some(parent) = new.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent)
                        .map_err(MigrationError::ParentCreationFailed)?;
                }
            }

            match std::fs::rename(old, new) {
                Ok(()) => {
                    eprintln!(
                        "usagio: migrated config {} → {}",
                        old.display(),
                        new.display()
                    );
                    Ok(MigrationResult::Migrated {
                        from: old.to_path_buf(),
                        to: new.to_path_buf(),
                    })
                }
                Err(e) if is_cross_device(&e) => copy_delete_fallback(old, new),
                Err(e) => Err(MigrationError::RenameFailed(e)),
            }
        }
    }
}

// -------------------------------------------------------------------------
// Internals
// -------------------------------------------------------------------------

fn config_base() -> Result<PathBuf, MigrationError> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        let p = PathBuf::from(x);
        if !p.as_os_str().is_empty() {
            return Ok(p);
        }
    }
    let home = std::env::var_os("HOME").ok_or(MigrationError::NoConfigBase)?;
    Ok(PathBuf::from(home).join(".config"))
}

fn is_cross_device(e: &io::Error) -> bool {
    // libc::EXDEV == 18 on Linux, 18 on macOS. `raw_os_error` returns the
    // errno directly on Unix; on other platforms this will just be false and
    // the error will surface as RenameFailed, which is the correct behavior.
    e.raw_os_error() == Some(18)
}

fn copy_delete_fallback(old: &Path, new: &Path) -> Result<MigrationResult, MigrationError> {
    copy_recursive(old, new).map_err(MigrationError::CopyFallbackFailed)?;

    if let Err(e) = std::fs::remove_dir_all(old) {
        // The new tree is populated and mode-correct — the caller can proceed
        // even if we couldn't clean up the old copy. Surface it as an error
        // variant that carries the usable new path so a caller who wants to
        // downgrade to a warning can.
        return Err(MigrationError::OldRemovalFailed {
            new: new.to_path_buf(),
            source: e,
        });
    }

    Ok(MigrationResult::Migrated {
        from: old.to_path_buf(),
        to: new.to_path_buf(),
    })
}

fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    let src_meta = src.symlink_metadata()?;
    let file_type = src_meta.file_type();

    if file_type.is_symlink() {
        let target = std::fs::read_link(src)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dst)?;
        #[cfg(not(unix))]
        {
            let _ = target;
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "symlink copy not implemented on this platform",
            ));
        }
        return Ok(());
    }

    if file_type.is_dir() {
        std::fs::create_dir_all(dst)?;
        preserve_mode(&src_meta, dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let child_src = entry.path();
            let child_dst = dst.join(entry.file_name());
            copy_recursive(&child_src, &child_dst)?;
        }
        return Ok(());
    }

    // Regular file (or anything else `copy` accepts).
    std::fs::copy(src, dst)?;
    preserve_mode(&src_meta, dst)?;
    Ok(())
}

#[cfg(unix)]
fn preserve_mode(src_meta: &std::fs::Metadata, dst: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = src_meta.permissions().mode();
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn preserve_mode(_src_meta: &std::fs::Metadata, _dst: &Path) -> io::Result<()> {
    Ok(())
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write(path: &Path, bytes: &[u8], mode: u32) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        #[cfg(not(unix))]
        let _ = mode;
    }

    #[test]
    fn fresh_install_when_neither_exists() {
        let t = tmp();
        let old = t.path().join("claude-usage");
        let new = t.path().join("usagio");

        let r = migrate_between(&old, &new).unwrap();
        assert!(matches!(r, MigrationResult::FreshInstall));
        assert!(!old.exists() && !new.exists(), "no side effects");
    }

    #[test]
    fn already_migrated_when_only_new_exists() {
        let t = tmp();
        let old = t.path().join("claude-usage");
        let new = t.path().join("usagio");
        write(&new.join("state.json"), b"{}", 0o600);

        let r = migrate_between(&old, &new).unwrap();
        assert!(matches!(r, MigrationResult::AlreadyMigrated));
        assert!(!old.exists(), "old still absent");
        assert!(new.join("state.json").exists(), "new untouched");
    }

    #[test]
    fn migrate_happy_path_moves_tree() {
        let t = tmp();
        let old = t.path().join("claude-usage");
        let new = t.path().join("usagio");
        write(&old.join("state.json"), b"{\"v\":2}", 0o600);
        write(&old.join("logs").join("app.log"), b"hello", 0o644);

        let r = migrate_between(&old, &new).unwrap();
        assert!(matches!(r, MigrationResult::Migrated { .. }));
        assert!(!old.exists(), "old removed after rename");
        assert_eq!(fs::read(new.join("state.json")).unwrap(), b"{\"v\":2}");
        assert_eq!(
            fs::read(new.join("logs").join("app.log")).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn both_exist_keeps_new_and_leaves_old() {
        let t = tmp();
        let old = t.path().join("claude-usage");
        let new = t.path().join("usagio");
        write(&old.join("state.json"), b"OLD", 0o600);
        write(&new.join("state.json"), b"NEW", 0o600);

        let r = migrate_between(&old, &new).unwrap();
        assert!(matches!(r, MigrationResult::BothExisted { .. }));
        assert_eq!(fs::read(old.join("state.json")).unwrap(), b"OLD");
        assert_eq!(fs::read(new.join("state.json")).unwrap(), b"NEW");
    }

    #[test]
    #[cfg(unix)]
    fn migrate_preserves_600_on_state_json() {
        let t = tmp();
        let old = t.path().join("claude-usage");
        let new = t.path().join("usagio");
        write(&old.join("state.json"), b"{}", 0o600);

        migrate_between(&old, &new).unwrap();
        let mode = fs::metadata(new.join("state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "mode preserved across rename");
    }

    #[test]
    #[cfg(unix)]
    fn copy_fallback_preserves_tree_and_modes() {
        // Exercises the copy_recursive path directly (portable stand-in for a
        // real EXDEV rename, which requires a second filesystem to reproduce).
        let t = tmp();
        let src = t.path().join("src");
        let dst = t.path().join("dst");
        write(&src.join("state.json"), b"{\"v\":2}", 0o600);
        write(&src.join("logs").join("a.log"), b"a", 0o644);
        fs::create_dir_all(src.join("empty")).unwrap();

        copy_recursive(&src, &dst).unwrap();

        assert_eq!(fs::read(dst.join("state.json")).unwrap(), b"{\"v\":2}");
        assert_eq!(fs::read(dst.join("logs").join("a.log")).unwrap(), b"a");
        assert!(dst.join("empty").is_dir());
        let mode = fs::metadata(dst.join("state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn old_as_symlink_moves_the_link_not_the_target() {
        // If a user has symlinked `~/.config/claude-usage` at some real
        // location, we should rename the symlink itself so their custom
        // storage location is preserved intact.
        let t = tmp();
        let real = t.path().join("real-storage");
        write(&real.join("state.json"), b"{}", 0o600);

        let old = t.path().join("claude-usage");
        std::os::unix::fs::symlink(&real, &old).unwrap();
        let new = t.path().join("usagio");

        let r = migrate_between(&old, &new).unwrap();
        assert!(matches!(r, MigrationResult::Migrated { .. }));
        assert!(!old.exists(), "old symlink gone");
        let new_meta = fs::symlink_metadata(&new).unwrap();
        assert!(new_meta.file_type().is_symlink(), "new is still a symlink");
        assert_eq!(fs::read_link(&new).unwrap(), real);
        // And it still resolves to real data.
        assert_eq!(fs::read(new.join("state.json")).unwrap(), b"{}");
    }
}
