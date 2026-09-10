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
use muri::compat::tray_icon::menu::{
    CheckMenuItem, Color, Icon, IconMenuItem, IsMenuItem, Menu, MenuItem as NativeMenuItem,
    PredefinedMenuItem, Submenu,
};

/// A bundled provider PNG → a muri menu-item `Icon`. muri owns PNG decoding now
/// (`Icon::from_png`, muri #24) — usagio no longer hand-rolls a `png`-crate
/// decode. Best-effort: an undecodable/unsupported PNG yields `None` and the
/// row falls back to text-only.
fn decode_menu_icon(bytes: &[u8]) -> Option<Icon> {
    Icon::from_png(bytes).ok()
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
                // The active account renders bold (no checkmark, so no leading
                // gutter/indent — muri's `GutterPolicy::Auto` drops the gutter
                // on a surface with no checkmarks), and its trailing `S% / W%`
                // value segment is tinted per severity, driven off usagio's
                // `RowStyle`.
                sub.set_bold(*active);
                sub.set_value_color((*value_color).map(muri_color));
                append_children(&sub, items);
                container.append_item(&sub);
            }
        }
    }
}
