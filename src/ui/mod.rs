//! Custom tray-anchored popup UI (Phase 1 — macOS `NSPopover`).
//!
//! This whole module is compiled only under
//! `cfg(all(target_os = "macos", feature = "custom-popup"))` (gated once at the
//! `mod ui;` declaration in `main.rs`), so nothing inside it needs its own
//! `#[cfg(target_os = ...)]` — which keeps `tests/strict_cfg.rs` happy without
//! scattering OS checks through `src/ui/`.
//!
//! The data layer (`menubar::build_snapshot` / `Snapshot` / the provider-group
//! and account ordering / `handle_click`'s id-routing) is UNCHANGED. This module
//! only adds a *renderer*: `menubar::popover_model` folds a `Snapshot` into the
//! toolkit-neutral [`PopoverModel`] below, and [`popover`] draws it with native
//! AppKit views, emitting the SAME click-id strings `handle_click` already
//! understands (`switch:<provider>:<key>`, `refresh:now`, `quit`, `noop`, …).
//!
//! See `docs/design/custom-tray-popup.md` for the approved design and the
//! phase plan (Phase 2 adds detail rows + the settings/capture trees + making
//! the popover the macOS default).

pub(crate) mod popover;

/// Usage-severity band for a percentage span, mirroring `menubar::Severity`
/// but kept local so the view-model layer doesn't depend on the private
/// renderer enum. The popover maps these to `NSColor` semantic system colors
/// (`systemOrangeColor` / `systemRedColor`) so dark/light mode is automatic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Sev {
    Amber,
    Red,
}

/// A colored run inside an account row's trailing `S% / W%` (or locked
/// countdown) text. Offsets/lengths are UTF-16 code units — the unit
/// `NSRange` counts in — matching how `menubar::trailing_for_account`
/// produces them.
#[derive(Clone, Debug)]
pub(crate) struct PctSpan {
    pub start: usize,
    pub len: usize,
    pub sev: Sev,
}

/// One rendered row of the popover's top-level list. Each variant carries the
/// minimum the AppKit renderer needs; account/action rows carry the exact
/// click-id string `handle_click` routes on, so no switch/launch/remove logic
/// is duplicated.
#[derive(Clone, Debug)]
pub(crate) enum PopoverRow {
    /// Bold provider-group header (e.g. "Claude"), with the provider icon slug
    /// for `crate::icons::png16_for`.
    GroupHeader {
        title: String,
        icon_slug: Option<&'static str>,
    },
    /// One account: leading checkmark when `active`, the email, and the
    /// flush-right `trailing` (`S% / W%` or a locked countdown) with its
    /// severity `colors`. `click_id` switches to the account (or `noop` when
    /// it's already active / the provider can't switch).
    Account {
        email: String,
        active: bool,
        trailing: String,
        colors: Vec<PctSpan>,
        click_id: String,
    },
    /// A horizontal separator between provider groups / before the action rows.
    Separator,
    /// A clickable bottom action row (Capture / Settings / Refresh / Quit).
    Action { label: String, click_id: String },
}

/// The full top-level popover content, in render order. Built by
/// `menubar::popover_model(&Snapshot)` and consumed by
/// [`popover::PopoverHost`].
#[derive(Clone, Debug, Default)]
pub(crate) struct PopoverModel {
    pub rows: Vec<PopoverRow>,
}
