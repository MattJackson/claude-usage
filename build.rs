//! Build script — Windows application manifest only.
//!
//! WHY: `usagio.exe` statically imports `SetWindowSubclass`,
//! `RemoveWindowSubclass`, `DefSubclassProc` and `TaskDialogIndirect` from
//! `comctl32.dll` (pulled in transitively by `tray-icon`/`tao` and `rfd`).
//! Those symbols exist ONLY in ComCtl32 **v6** (shipped in WinSxS), not the
//! base `C:\Windows\System32\comctl32.dll` (v5.82). Without an application
//! manifest declaring a dependency on `Microsoft.Windows.Common-Controls`
//! v6.0.0.0, the Windows loader binds the v5.82 comctl32, fails to resolve
//! those entry points, and kills the process with STATUS_ENTRYPOINT_NOT_FOUND
//! (0xC0000139) BEFORE `main` ever runs — which is exactly why the binary
//! failed to launch on clean/Server Windows and CI runners. Embedding the
//! manifest makes the loader activate ComCtl32 v6 (which exports these), so the
//! binary loads on every Windows box.
//!
//! `embed-manifest` writes the manifest and wires the MSVC linker to embed it;
//! it enables the Common-Controls v6 dependency by default. It is a no-op on
//! any non-Windows target, so macOS and Linux builds are unaffected.

fn main() {
    // Only embed a manifest when building FOR Windows. `CARGO_CFG_WINDOWS` is
    // set by Cargo for any windows target (both native and cross builds).
    #[cfg(windows)]
    {
        use embed_manifest::{embed_manifest, new_manifest};
        // `new_manifest` defaults already include the ComCtl32 v6 dependency
        // and per-monitor-v2 DPI awareness — everything a tray/GUI app needs to
        // load and render correctly. The name is the assembly identity only.
        embed_manifest(new_manifest("MattJackson.Usagio"))
            .expect("failed to embed Windows application manifest");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
