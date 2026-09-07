//! Sanity checks on the macOS app-bundle packaging scaffold
//! (`packaging/macos/`) — see the header comment in
//! `packaging/macos/usagio.app.template/Contents/Info.plist` for why this
//! exists: without a real bundle, macOS Login Items shows the generic
//! "exec" glyph instead of the usagio icon.
//!
//! These don't need a real plist parser: the Info.plist template is
//! hand-written XML with one `<key>...</key><value.../>` pair per line-ish
//! entry, so a substring/regex scan is enough to assert every required key
//! is present with the value release.yml expects.

use std::path::Path;

fn info_plist_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("packaging/macos/usagio.app.template/Contents/Info.plist")
}

/// Returns the string value immediately following `<key>{key}</key>` in the
/// plist, e.g. `<key>CFBundleName</key>\n  <string>usagio</string>` yields
/// `Some("usagio")`. Returns `None` if the key is absent or its value isn't
/// a `<string>`.
fn string_value_for_key(xml: &str, key: &str) -> Option<String> {
    let marker = format!("<key>{key}</key>");
    let after_key = xml.split_once(&marker)?.1;
    let after_open = after_key.split_once("<string>")?.1;
    let (value, _) = after_open.split_once("</string>")?;
    Some(value.to_string())
}

/// Returns true if `<key>{key}</key>` is immediately followed by a
/// self-closing boolean tag (`<true/>` or `<false/>`), and specifically
/// whether that tag is `<true/>`.
fn bool_value_for_key(xml: &str, key: &str) -> Option<bool> {
    let marker = format!("<key>{key}</key>");
    let after_key = xml.split_once(&marker)?.1;
    let trimmed = after_key.trim_start();
    if trimmed.starts_with("<true/>") {
        Some(true)
    } else if trimmed.starts_with("<false/>") {
        Some(false)
    } else {
        None
    }
}

#[test]
fn packaging_info_plist_has_required_keys() {
    let xml = std::fs::read_to_string(info_plist_path()).expect("read Info.plist template");

    assert_eq!(
        string_value_for_key(&xml, "CFBundleIdentifier").as_deref(),
        Some("com.mattjackson.usagio")
    );
    assert_eq!(
        string_value_for_key(&xml, "CFBundleName").as_deref(),
        Some("usagio")
    );
    assert_eq!(
        string_value_for_key(&xml, "CFBundleDisplayName").as_deref(),
        Some("usagio")
    );
    assert_eq!(
        string_value_for_key(&xml, "CFBundleVersion").as_deref(),
        Some("{{VERSION}}")
    );
    assert_eq!(
        string_value_for_key(&xml, "CFBundleShortVersionString").as_deref(),
        Some("{{VERSION}}")
    );
    assert_eq!(
        string_value_for_key(&xml, "CFBundleExecutable").as_deref(),
        Some("usagio")
    );
    assert_eq!(
        string_value_for_key(&xml, "CFBundleIconFile").as_deref(),
        Some("AppIcon")
    );
    assert_eq!(
        string_value_for_key(&xml, "LSMinimumSystemVersion").as_deref(),
        Some("12.0")
    );
    assert_eq!(bool_value_for_key(&xml, "LSUIElement"), Some(true));
    assert_eq!(
        bool_value_for_key(&xml, "NSHighResolutionCapable"),
        Some(true)
    );
}

#[test]
fn packaging_bundle_template_has_placeholder_directories() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/macos/usagio.app.template");
    assert!(
        root.join("Contents/MacOS/.gitkeep").exists(),
        "release.yml drops the universal binary here"
    );
    assert!(
        root.join("Contents/Resources/.gitkeep").exists(),
        "release.yml drops AppIcon.icns here"
    );
}
