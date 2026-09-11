//! The single IR→muri translator, shared by every platform.
//!
//! usagio builds the tray menu once as a generic, `Send`-able [`MenuTree`]
//! (`menubar::menu_tree_from_snapshot`) and this turns it into a live
//! **native** `muri::Menu` on the platform's UI thread. As of muri 0.11 the
//! `muda-compat` facade is a frozen, pure `muda`/`tray-icon` drop-in with NO
//! muri-only styling (bold, per-severity value colors, icons) — all of that
//! lives in the native API (`muri::menu::{Menu, Row, Item, Icon}` +
//! `Row::bold`/`Row::value_color`), which usagio now targets directly. The
//! resulting `Menu` is fed to a native `muri::Tray` and to the headless
//! offscreen renderer (`muri::render_menu_to_png`) alike.
//!
//! Threading: build on the platform's UI thread (macOS main thread, the GTK
//! thread on Linux, the tray UI thread on Windows). The `Send` [`MenuTree`] is
//! what crosses any channel; muri is only ever constructed here, on the far
//! side.

use super::{MenuItem, MenuTree, ValueColor};
use muri::{Color, Icon, Item, Menu, MenuId, Row};

/// Map the platform-agnostic [`ValueColor`] (severity band) to muri's `Color`
/// for `Row::value_color`. Red = "about to hit the wall", Amber = "approaching
/// it".
fn muri_color(c: ValueColor) -> Color {
    match c {
        ValueColor::Red => Color::SystemRed,
        ValueColor::Amber => Color::SystemOrange,
    }
}

/// A bundled provider PNG → a native muri `Icon`. muri owns PNG decoding
/// (lazy, at paint time); this is just a wrapper.
fn menu_icon(bytes: &[u8]) -> Icon {
    Icon::from_png(bytes.to_vec())
}

/// Translate the generic [`MenuTree`] into a live **native** `muri::Menu`.
/// Call on the platform's UI thread (see the module doc).
pub(crate) fn render_menu(tree: &MenuTree) -> Menu {
    build_menu(&tree.items)
}

fn build_menu(items: &[MenuItem]) -> Menu {
    let mut menu = Menu::new();
    for item in items {
        menu = append(menu, item);
    }
    menu
}

fn append(menu: Menu, item: &MenuItem) -> Menu {
    match item {
        MenuItem::Action {
            id,
            label,
            icon_png,
            enabled,
            checked,
            checkable,
        } => {
            // A `\t` in the label splits it into a grow label + a right-aligned
            // value (e.g. `"Quit\tusagio v0.6.1"`) — the same flush-right value
            // the account rows use, so the version reads at the right edge
            // rather than mashed onto the label.
            let base = Row::new(id.as_str()).enabled(*enabled);
            let mut row = match label.split_once('\t') {
                Some((l, v)) => base.label_value(l.trim_end(), v.trim_start()),
                None => base.label(label),
            };
            // Only checkable rows reserve the check column; a plain action must
            // not (native `checked` marks the row as a checkbox).
            if *checkable {
                row = row.checked(*checked);
            }
            if let Some(png) = icon_png {
                row = row.leading(menu_icon(png));
            }
            menu.item(Item::Row(row))
        }
        MenuItem::Static { label, icon_png } => {
            // A non-interactive header row (the per-provider "Claude"/"Codex"
            // group header, drawn dimmed) — carries the provider mark when set.
            let mut row = Row::label_only(label);
            if let Some(png) = icon_png {
                row = row.leading(menu_icon(png));
            }
            menu.section_header(row)
        }
        MenuItem::Separator => menu.separator(),
        MenuItem::Submenu {
            label,
            items,
            active,
            value_color,
            ..
        } => {
            let label_row = submenu_label(label, *active, *value_color);
            menu.submenu(label_row, build_menu(items))
        }
    }
}

/// Build a submenu's parent row. usagio's account label is `"name\tvalue"`; the
/// tab splits it into a grow label + a right-aligned value segment so the value
/// aligns and can be tinted per severity. The active account renders **bold**
/// (no checkmark → no leading gutter), and its `S% / W%` value carries the
/// severity color.
fn submenu_label(label: &str, active: bool, value_color: Option<ValueColor>) -> Row {
    // Non-interactive parent row (id = none): the submenu opens the flyout;
    // the actionable rows live inside it.
    let base = Row::new(MenuId::none());
    let mut row = match label.split_once('\t') {
        Some((name, value)) => base.label_value(name.trim_end(), value.trim_start()),
        None => base.label(label),
    };
    if active {
        row = row.bold();
    }
    if let Some(c) = value_color {
        row = row.value_color(muri_color(c));
    }
    row
}
