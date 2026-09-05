//! Self-update from GitHub Releases. Downloads the published macOS tarball for
//! the latest release, verifies its SHA-256 against the release's `.sha256`
//! asset, and atomically replaces the running binary. Used by the `update` CLI
//! command, the once-a-day background check, and the menu-bar "Check for
//! updates…" item.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;

const RELEASE_API: &str = "https://api.github.com/repos/MattJackson/claude-usage/releases/latest";
const USER_AGENT: &str = "claude-usage-self-update";

/// Outcome of an update attempt.
pub enum Outcome {
    Updated(String),
    UpToDate,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A GitHub release: its tag and (asset name, download url) pairs.
struct Release {
    tag: String,
    assets: Vec<(String, String)>,
}

fn fetch_latest_release() -> Result<Release> {
    let v: serde_json::Value = ureq::get(RELEASE_API)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .context("querying latest release")?
        .into_json()
        .context("parsing release JSON")?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("release has no tag_name"))?
        .to_string();
    let assets = v
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let name = a.get("name")?.as_str()?.to_string();
                    let url = a.get("browser_download_url")?.as_str()?.to_string();
                    Some((name, url))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { tag, assets })
}

/// The version of the latest release if it's newer than what we're running.
pub fn check_latest() -> Result<Option<String>> {
    let rel = fetch_latest_release()?;
    let latest = rel.tag.trim_start_matches('v');
    Ok(if is_newer(latest, current_version()) {
        Some(latest.to_string())
    } else {
        None
    })
}

/// Download the latest release for this machine, verify its checksum, and
/// replace the running binary in place.
pub fn run_update(auto: bool) -> Result<Outcome> {
    eprintln!(
        "claude-usage: checking for updates ({})",
        if auto { "auto" } else { "manual" }
    );
    let rel = fetch_latest_release()?;
    let latest = rel.tag.trim_start_matches('v').to_string();
    if !is_newer(&latest, current_version()) {
        return Ok(Outcome::UpToDate);
    }

    let (asset_name, asset_url) = pick_asset(&rel.assets)
        .ok_or_else(|| anyhow!("no macOS tarball in release {}", rel.tag))?;
    let sum_name = format!("{asset_name}.sha256");
    let (_, sum_url) = rel
        .assets
        .iter()
        .find(|(n, _)| n == &sum_name)
        .ok_or_else(|| anyhow!("release is missing {sum_name}"))?;

    let tarball = download(&asset_url).context("downloading release tarball")?;
    let expected = parse_sha256(&download_text(sum_url).context("downloading checksum")?)
        .ok_or_else(|| anyhow!("could not parse {sum_name}"))?;
    let actual = sha256_hex(&tarball);
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!("checksum mismatch for {asset_name} (expected {expected}, got {actual})");
    }

    // Extract and swap the binary in place.
    let dir = std::env::temp_dir().join(format!("claude-usage-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir).context("creating temp dir")?;
    let tarball_path = dir.join(&asset_name);
    std::fs::write(&tarball_path, &tarball).context("writing tarball")?;
    let status = std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(&tarball_path)
        .arg("-C")
        .arg(&dir)
        .status()
        .context("running tar")?;
    if !status.success() {
        bail!("tar failed to extract {asset_name}");
    }
    let new_bin = dir.join("claude-usage");
    if !new_bin.exists() {
        bail!("extracted archive did not contain the claude-usage binary");
    }
    make_executable(&new_bin)?;
    self_replace::self_replace(&new_bin).context("replacing the running binary")?;
    let _ = std::fs::remove_dir_all(&dir);

    Ok(Outcome::Updated(latest))
}

/// Re-exec the (now updated) binary with the original arguments, then exit.
pub fn relaunch() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let _ = std::process::Command::new(exe).args(args).spawn();
    }
    std::process::exit(0);
}

/// Prefer the universal tarball (runs on both arches); fall back to this
/// machine's specific architecture.
fn pick_asset(assets: &[(String, String)]) -> Option<(String, String)> {
    let want = |suffix: &str| assets.iter().find(|(n, _)| n.ends_with(suffix)).cloned();
    want("universal-apple-darwin.tar.gz")
        .or_else(|| want(&format!("{}-apple-darwin.tar.gz", std::env::consts::ARCH)))
}

fn download(url: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .call()?
        .into_reader()
        .read_to_end(&mut buf)?;
    Ok(buf)
}

fn download_text(url: &str) -> Result<String> {
    Ok(String::from_utf8_lossy(&download(url)?).into_owned())
}

/// `shasum` output is `<hex>␠␠<filename>`; take the first token.
fn parse_sha256(contents: &str) -> Option<String> {
    contents.split_whitespace().next().map(|s| s.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Compare dotted numeric versions, ignoring any pre-release suffix.
fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split('-')
            .next()
            .unwrap_or(v)
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (l, c) = (parts(latest), parts(current));
    for i in 0..l.len().max(c.len()) {
        let (a, b) = (
            l.get(i).copied().unwrap_or(0),
            c.get(i).copied().unwrap_or(0),
        );
        if a != b {
            return a > b;
        }
    }
    false
}

#[cfg(unix)]
fn make_executable(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .context("chmod +x on the new binary")
}

#[cfg(not(unix))]
fn make_executable(_path: &std::path::Path) -> Result<()> {
    Ok(())
}
