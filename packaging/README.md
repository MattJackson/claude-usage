# Packaging & cross-platform builds

usagio targets macOS, Linux, and Windows. This doc covers local builds on
each platform plus how CI enforces the platform-abstraction rule.

## Linux: local build

Install the system deps `tray-icon`, `notify-rust`, `ureq` (TLS), and the
`secret-service` crate need at build time:

```sh
sudo apt-get update
sudo apt-get install -y \
  libgtk-3-dev libayatana-appindicator3-dev libxdo-dev \
  libssl-dev pkg-config libsecret-1-dev
```

Then build/test as usual:

```sh
cargo build --release --all-features
cargo test --tests --all-features -- --test-threads=1
```

These package names target Ubuntu 24.04 (`ubuntu-latest` in CI). Other
distros will have differently-named equivalents (e.g. `libappindicator3-dev`
on older Ubuntu, `gtk3-devel` / `libsecret-devel` on Fedora).

## Windows: local build

The MSVC toolchain (`x86_64-pc-windows-msvc`) is what CI and releases use.
To build locally on Windows you need:

- Rust (`rustup`, stable channel) — installs the MSVC target by default on
  Windows hosts.
- The "Desktop development with C++" workload from the Visual Studio Build
  Tools (provides the MSVC linker `link.exe`).

```powershell
cargo build --release --all-features
cargo test --tests --all-features -- --test-threads=1
```

In practice, most contributors develop on macOS or Linux and don't have a
Windows box or VM handy. Rather than cross-compiling to
`x86_64-pc-windows-msvc` locally (which needs the MSVC linker and isn't
well-supported from a non-Windows host), **prefer letting the CI matrix's
`windows-latest` runner build and test your change** — push a branch / open
a PR and let the `check (windows-latest)` job do it. Cross-compiling from
Linux to Windows via `cargo-xwin` or similar is possible but out of scope
here.

## macOS: local build

No extra system deps — Xcode Command Line Tools (`xcode-select --install`)
provide everything needed.

```sh
cargo build --release --all-features
cargo test --tests --all-features -- --test-threads=1
```

## CI matrix structure

`.github/workflows/ci.yml`'s `check` job runs as a matrix over
`[macos-latest, ubuntu-latest, windows-latest]`. Each leg:

1. Checks out the repo.
2. (Ubuntu only) installs the apt packages listed above.
3. Installs stable Rust + `rustfmt`/`clippy` components, with
   `Swatinem/rust-cache` for incremental-build caching.
4. Runs `cargo fmt --all -- --check`.
5. Runs `cargo clippy --all-features -- -D warnings`.
6. Runs `cargo build --release --all-features`.
7. Runs `cargo test --tests --all-features -- --test-threads=1`
   (`--test-threads=1` is required — several tests serialize `$HOME`
   mutation through `env_lock::scoped_env_var` and would race each other
   under parallel execution).
8. (Ubuntu only) runs the strict-cfg lint below. It's a plain text scan of
   the checked-out source, independent of the runner OS, so it only needs
   to run once per CI invocation rather than on all three legs.

A separate `build` job does a release build on `qa`/`main` pushes
(macOS-only; a straight compile sanity check, not a full re-test).

`.github/workflows/release.yml` builds and publishes three artifacts per
tagged release: a universal macOS binary (existing, also drives the
Homebrew tap bump), a Linux `x86_64-unknown-linux-gnu` tarball, and a
Windows `x86_64-pc-windows-msvc` zip. The Homebrew formula only tracks the
macOS SHA256 — there's no Linux or Windows package yet (planned as a
Snap/AppImage/AUR package and an MSI/scoop bucket respectively, both
post-v0.5.0 work).

## The "no scattered `#[cfg(target_os)]`" rule

All host-OS-specific code must live behind the `Platform` trait in
`src/platform/mod.rs`, implemented per-OS in `src/platform/{macos,linux,windows}.rs`.
Sprinkling `#[cfg(target_os = "...")]` (or `cfg!(target_os = "...")`)
elsewhere in `src/` defeats that abstraction and tends to bit-rot silently
on platforms nobody's building locally.

CI enforces this on the Ubuntu leg of the `check` job, and it's also
checked by `cargo test` locally via `tests/strict_cfg.rs`, which walks
`src/` and fails if any file outside `src/platform/` contains a
`cfg(target_os` (or `cfg!(target_os`) pattern.

To check by hand:

```sh
grep -rn 'cfg(target_os' src/ | grep -v '^src/platform/'
```

This should return nothing. If it doesn't, either move the OS-specific
logic into a `Platform` trait method / impl, or (if the match is genuinely
just `mod` inclusion gating for a platform-specific module not yet fully
behind the trait) treat it as tracked tech debt, not something to leave
unaddressed.
