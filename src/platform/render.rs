//! The single IR→muri translator, shared by every platform.
//!
//! Before muri, macOS rendered a native `NSMenu` (with a bespoke
//! attributed-string styler) while Windows/Linux went through `muda`, so the
//! menu was described and translated separately per platform. muri now renders
//! on all three through one cross-platform compat API, so there is exactly one
//! way to turn the generic [`MenuTree`] (built once by
//! `menubar::menu_tree_from_snapshot`) into a live `muri::compat` menu.
//!
//! Threading: the returned `Menu` is `Rc`-backed (`!Send`), so it MUST be
//! built on the platform's UI thread — macOS's main thread, the GTK thread on
//! Linux, the tray UI thread on Windows. The Send-able `MenuTree` is what
//! crosses any channel; muri is only ever constructed here, on the far side.

use super::{MenuItem, MenuTree, ValueColor};
use anyhow::{bail, Context, Result};
use muri::compat::tray_icon::menu::{
    CheckMenuItem, Color, Icon, IconMenuItem, IsMenuItem, Menu, MenuItem as NativeMenuItem,
    PredefinedMenuItem, Submenu,
};

/// Decode PNG bytes (the format every bundled provider/tray icon ships as, see
/// `src/icons.rs`) into raw RGBA + dimensions. Pure Rust (`png` crate), so it
/// cross-compiles without a system libpng.
pub(crate) fn decode_png_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .context("decoding PNG header for a tray/menu icon")?;
    let buf_size = reader
        .output_buffer_size()
        .context("PNG output buffer size overflow")?;
    let mut buf = vec![0u8; buf_size];
    let info = reader
        .next_frame(&mut buf)
        .context("decoding PNG frame for a tray/menu icon")?;
    let bytes = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => {
            let (chunks, _rem) = bytes.as_chunks::<3>();
            chunks
                .iter()
                .flat_map(|c| [c[0], c[1], c[2], 255])
                .collect()
        }
        other => {
            bail!("unsupported PNG color type for a tray/menu icon: {other:?} (need RGB or RGBA)")
        }
    };
    Ok((rgba, info.width, info.height))
}

/// A bundled provider PNG → a muri menu-item `Icon` (raw RGBA). Best-effort:
/// an undecodable/unsupported PNG yields `None` and the row falls back to
/// text-only.
fn decode_menu_icon(bytes: &[u8]) -> Option<Icon> {
    let (rgba, w, h) = decode_png_rgba(bytes).ok()?;
    Icon::from_rgba(rgba, w, h).ok()
}

/// Map the platform-agnostic [`ValueColor`] (severity band) to muri's `Color`
/// for `Submenu::set_value_color`. Red = "about to hit the wall", Amber =
/// "approaching it".
fn muri_color(c: ValueColor) -> Color {
    match c {
        ValueColor::Red => Color::SystemRed,
        ValueColor::Amber => Color::SystemOrange,
    }
}

/// `Menu` and `Submenu` both expose an inherent `append(&dyn IsMenuItem)` but
/// share no common trait for it, so this bridges the two for the recursive
/// walk below.
trait Container {
    fn append_item(&self, item: &dyn IsMenuItem);
}
impl Container for Menu {
    fn append_item(&self, item: &dyn IsMenuItem) {
        let _ = self.append(item);
    }
}
impl Container for Submenu {
    fn append_item(&self, item: &dyn IsMenuItem) {
        let _ = self.append(item);
    }
}

/// Translate the generic [`MenuTree`] into a live muri (`muda-compat`) menu.
/// Call on the platform's UI thread (see the module doc).
pub(crate) fn render_menu(tree: &MenuTree) -> Menu {
    let menu = Menu::new();
    append_children(&menu, &tree.items);
    menu
}

fn append_children(container: &dyn Container, items: &[MenuItem]) {
    for item in items {
        match item {
            MenuItem::Action {
                id,
                label,
                icon_png,
                enabled,
                checked,
                checkable,
            } => {
                if *checkable {
                    container.append_item(&CheckMenuItem::with_id(
                        id.as_str(),
                        label,
                        *enabled,
                        *checked,
                        None,
                    ));
                } else if let Some(icon) = icon_png.as_deref().and_then(decode_menu_icon) {
                    container.append_item(&IconMenuItem::with_id(
                        id.as_str(),
                        label,
                        *enabled,
                        Some(icon),
                        None,
                    ));
                } else {
                    container.append_item(&NativeMenuItem::with_id(
                        id.as_str(),
                        label,
                        *enabled,
                        None,
                    ));
                }
            }
            MenuItem::Static { label, icon_png } => {
                // Disabled label row; if it carries an icon (provider-group
                // header) render it as a disabled IconMenuItem so the provider
                // mark shows next to the name.
                if let Some(icon) = icon_png.as_deref().and_then(decode_menu_icon) {
                    container.append_item(&IconMenuItem::with_id(
                        "noop",
                        label,
                        false,
                        Some(icon),
                        None,
                    ));
                } else {
                    container.append_item(&NativeMenuItem::with_id("noop", label, false, None));
                }
            }
            MenuItem::Separator => {
                container.append_item(&PredefinedMenuItem::separator());
            }
            MenuItem::Submenu {
                label,
                items,
                active,
                value_color,
                ..
            } => {
                let sub = Submenu::new(label, true);
                // The active account renders bold + leading checkmark, and its
                // trailing `S% / W%` value segment is tinted per severity,
                // driven off usagio's `RowStyle`.
                sub.set_active(*active);
                sub.set_value_color((*value_color).map(muri_color));
                append_children(&sub, items);
                container.append_item(&sub);
            }
        }
    }
}
