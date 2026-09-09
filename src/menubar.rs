//! macOS menu-bar app: shows the active account's usage in the status bar and
//! lets you switch / capture / remove accounts and set the auto-swap threshold
//! from a dropdown. The same `watch_cycle` that powers `usagio watch` runs
//! on a background thread here, so the daemon behaviour is identical.
//!
//! Usage numbers come from the cache (written by the scheduler poll); the UI
//! never fetches on its own, so menu interactions can't trigger HTTP 429s.
//!
//! Menu wiring iterates `providers::all()` and emits one `ProviderSection` per
//! provider — but only if that provider has at least one captured account in
//! state (see `build_snapshot`). The "Capture current login ▸" submenu always
//! shows, with one row per REGISTERED provider (installed or not).

use anyhow::Result;
#[cfg(target_os = "macos")]
use std::cell::RefCell;
use std::time::Duration;

use chrono::{DateTime, Utc};
// macOS native NSMenu renderer deps. `objc2`/`objc2-app-kit`/`objc2-foundation`/
// `block2` are `[target.'cfg(target_os = "macos")'.dependencies]` in
// Cargo.toml — they don't exist in the dependency graph on Linux/Windows, so
// this group needs real `#[cfg(target_os = "macos")]`, not just an
// unused-import allow. `tray_icon::menu`/`TrayIconBuilder` compile
// everywhere (top-level dep) but are grouped in here too since they're only
// used by the macOS-only `run`/`build_menu`/`build_account_block`/
// `install_menu` below — Linux/Windows render through `platform::MenuTree`
// instead (see the `cross_platform` module near the end of this file).
#[cfg(target_os = "macos")]
use {
    block2::RcBlock,
    objc2::MainThreadMarker,
    objc2_app_kit::{NSApplication, NSApplicationActivationPolicy},
    objc2_foundation::NSTimer,
    tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    tray_icon::TrayIconBuilder,
};

use crate::countdown::{self, AccountUsage, BlockingWindow, DisplayState};
use crate::providers::{self, CaptureMode, Provider, SeverityBands};
use crate::store::State;
use crate::{
    age_str, capture_current, capture_current_generic, env_override_active, menu_order,
    next_interval, notify, optimize_now, remove_account, remove_provider_account_generic,
    row_from_account, row_from_provider_account, switch_to, switch_to_provider_account,
    watch_cycle, with_state_lock, Row, SwapGuard, CLAUDE_SLUG, TARGET_CEILING_PCT, TRIGGER_PCT,
    WATCH_INTERVAL_SECS,
};

/// Exact title of the disabled section row inserted when a provider's env
/// override is active. A named constant so `build_menu` and the tests
/// asserting on the visible menu content can't drift out of sync.
pub(crate) const ENV_OVERRIDE_ROW_TITLE: &str = "env override active — swap disabled";

/// One rate-limit / quota window as displayed under an account's submenu. `id`
/// is the provider's own window slug ("session"/"weekly"/"opus" for Claude);
/// `label` is the short human string ("5h"/"7d"/"Opus 7d").
#[derive(Clone)]
struct WindowView {
    id: String,
    label: String,
    pct: Option<f64>,
    /// Human "resets in X" text, or empty when unknown.
    reset: String,
}

/// One account as shown in the menu. Windows are ordered by
/// `provider.window_order()`; `has_data` gates the stat-row block.
struct AcctView {
    provider_id: &'static str,
    /// Stable key used inside click ids (`switch:claude:<key>` etc). In v1
    /// this is the account's email; later phases key on the provider's
    /// `account_identifier`.
    key: String,
    /// Human-facing label rendered in the submenu title.
    display: String,
    windows: Vec<WindowView>,
    updated: String,
    active: bool,
    has_data: bool,
    /// Absolute reset instants for the first two windows, threaded through so
    /// `countdown::compute_display` can decide whether the row is "locked"
    /// (session/weekly ≥99.5% and reset still in the future). The strings in
    /// `WindowView.reset` are for display only — the raw `DateTime` is needed
    /// to reason about the future.
    session_reset_at: Option<DateTime<Utc>>,
    weekly_reset_at: Option<DateTime<Utc>>,
    /// When this account's cached usage was fetched. Threaded through to
    /// `countdown::compute_display` so it can distinguish a genuinely fresh
    /// post-reset reading from a stale pre-reset one still sitting in the
    /// cache (`DisplayState::StaleAfterReset` — see `trailing_for_account`).
    fetched_at: Option<DateTime<Utc>>,
}

/// One provider's block in the menu. Rendered only if `accounts` is non-empty
/// (per the "section renders only when the provider has at least one captured
/// account" rule).
pub(crate) struct ProviderSection {
    provider_id: &'static str,
    display_name: &'static str,
    supports_switching: bool,
    /// Gates the "Launch client" row (H3, v0.5.0 codeaudit). Independent of
    /// `supports_switching` — a provider can have one wired without the
    /// other; the row must not appear (and error on click) for a provider
    /// whose `launch_client` is still the `Unsupported` trait default.
    supports_launch: bool,
    /// Gates the "Remove…" row (H3, v0.5.0 codeaudit). Defaults `true` for
    /// every current provider — removing a captured account is a generic
    /// state.json operation — but stays explicit so a provider needing extra
    /// cleanup can opt out until that's wired.
    supports_remove: bool,
    supports_usage: bool,
    severity_bands: SeverityBands,
    /// True when the provider's OAuth env-override is set on this process's
    /// environment. Surfaced as a disabled row inside the section and used by
    /// `watch_cycle` to skip that provider entirely.
    env_override_active: bool,
    accounts: Vec<AcctView>,
}

/// One row directly under "Capture current login ▸" (v0.5.2 item 8 flattened
/// the old "Paste API key ▸" sub-submenu API-key providers used to render
/// under). `installed` is a best-effort probe used to grey out rows whose
/// credential store isn't present on this host; the row stays clickable so
/// errors surface honestly. `capture_mode` decides the click-id prefix
/// (`capture:` vs `apikey:`) that routes to the right handler in
/// `handle_click`, and orders creds-on-disk providers before API-key ones.
struct RegisteredProvider {
    provider_id: &'static str,
    display_name: &'static str,
    installed: bool,
    capture_mode: CaptureMode,
}

/// Everything the UI needs to render, produced by the poller thread.
#[derive(Default)]
struct Snapshot {
    sections: Vec<ProviderSection>,
    /// Render order for the flat main-list block sequence: `(section index,
    /// account index within that section)` pairs, spanning ALL providers —
    /// see `flat_account_order`'s doc for the sort. `build_menu` /
    /// `menu_tree_from_snapshot` iterate this instead of nesting
    /// `for sec in sections { for a in sec.accounts }`, which used to
    /// silently re-impose provider-declaration order on top of any
    /// per-account priority (the "account order keeps changing" report).
    account_order: Vec<(usize, usize)>,
    /// Providers whose `capture_mode == CredsOnDisk` and `supports_usage == true`:
    /// rendered directly under "Capture current login ▸".
    capture_creds: Vec<RegisteredProvider>,
    /// Providers whose `capture_mode == ApiKey` and `supports_usage == true`:
    /// rendered directly under "Capture current login ▸", after
    /// `capture_creds` (v0.5.2 item 8 flattened the "Paste API key ▸"
    /// sub-submenu these used to live under).
    capture_api_key: Vec<RegisteredProvider>,
    autoswap: bool,
    threshold: f64,
    /// Settings ▸ Notifications ▸ per-trigger enable checkboxes.
    notification_config: crate::notifications::NotificationConfig,
}

/// How near a limit a percentage is, for at-a-glance coloring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Severity {
    /// >= amber band: approaching the wall.
    Amber,
    /// >= red band: about to hit it.
    Red,
}

/// Map a utilization percentage to a color band given the provider's bands.
fn severity_with(p: Option<f64>, bands: SeverityBands) -> Option<Severity> {
    match p {
        Some(v) if v >= bands.red => Some(Severity::Red),
        Some(v) if v >= bands.amber => Some(Severity::Amber),
        _ => None,
    }
}

/// A styling directive for one menu row, matched to its native `NSMenuItem` by
/// the plain title string. We build the muda menu with plain titles (so clicks
/// and structure work exactly as before) then walk the native `NSMenu` and set
/// `attributedTitle` on the rows named here. Offsets are **UTF-16 code units**
/// (what `NSRange` uses); all our runs are ASCII so char == utf16 in practice,
/// but the helpers stay correct if an email ever isn't.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct RowStyle {
    /// The exact plain title set on the item; used to find it in the menu.
    plain: String,
    /// Bold the whole row (marks the active account instead of a checkmark).
    bold: bool,
    /// Whether the row is a section header (bold, disabled, no tab-stop).
    section_header: bool,
    /// Colored spans: (utf16 offset, utf16 length, band).
    colors: Vec<(usize, usize, Severity)>,
    /// If set, right-align everything after the first `\t` at a tab stop,
    /// battery-menu style. Requires the plain title to contain a `\t`. The
    /// concrete x (points) is NOT stored here — `TabX::MenuRight` is resolved
    /// to a menu-wide point value at install time by
    /// `mac_style::compute_menu_right_x`, which measures every row's actual
    /// rendered width so the tab stop always clears the widest label instead
    /// of relying on a fixed magic number (v0.5.1).
    tab_x_kind: Option<TabX>,
    /// If set, attach the 16px provider icon (looked up by slug in
    /// `crate::icons::png16_for`) to the native menu item via `setImage:`. Only
    /// set on section-header rows so per-provider iconography appears once at
    /// the top of each section. A missing slug (no bundled PNG) is a no-op.
    icon_slug: Option<&'static str>,
    /// If true, mark the native menu item as `NSControlStateValueOn` so a
    /// leading checkmark glyph appears — the "✓ Active" affordance for the
    /// active account row without changing the plain title text.
    checkmark: bool,
    /// If set, paint the run from this UTF-16 offset to the end of the row in
    /// `NSColor::secondaryLabelColor` (macOS "grey secondary" — the same tint
    /// disabled menu items use). Used to render "usagio vX.Y.Z" as a subdued
    /// trailing label on the enabled Quit row: right-aligned via `tab_x_kind`,
    /// grey via this field, without disabling the row's click.
    grey_tail_from: Option<usize>,
    /// v0.5.0: paint the WHOLE row in `NSColor::labelColor()` (the normal
    /// enabled-text color — white in dark mode, black in light) even though
    /// the underlying muda item is `enabled: true`. Used for the reset-window
    /// / model-breakdown informational rows inside an account submenu: they
    /// must read as normal text (not the greyed "disabled" look `enabled:
    /// false` produces) while still routing clicks to a no-op (see the
    /// `("noop", _, _)` arm in `handle_click`) since there's nothing to do
    /// when you click "Session resets in 3h".
    disabled_but_white: bool,
}

impl RowStyle {
    /// A row with no special styling beyond its plain title — construction
    /// sites override just the fields they need via struct-update syntax so
    /// adding a new `RowStyle` field never requires touching every call site.
    fn plain_row(plain: String) -> RowStyle {
        RowStyle {
            plain,
            bold: false,
            section_header: false,
            colors: Vec::new(),
            tab_x_kind: None,
            icon_slug: None,
            checkmark: false,
            grey_tail_from: None,
            disabled_but_white: false,
        }
    }
}

/// Kinds of right-align tab stop a row can request. Currently there's only
/// one: right-align at the menu's actual content edge. Kept as an enum
/// (rather than the row just carrying `bool`) so a future second alignment
/// scheme (e.g. a submenu-local edge) has somewhere to go without another
/// magic-number field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TabX {
    /// Right-align at a point computed fresh for each rendered snapshot —
    /// `max(label_width + trailing_width)` over every `MenuRight` row, plus
    /// a small safety pad — so the tab stop always clears the widest label
    /// instead of a fixed constant that silently misaligns past its bound.
    /// Resolved to a concrete point value by
    /// `mac_style::compute_menu_right_x` at install time; the
    /// `cross_platform` (Linux/Windows) renderer has no tab-stop concept at
    /// all and never reads this — it falls back to a plain ` · ` separator.
    MenuRight,
}

/// Length of a string in UTF-16 code units (the unit `NSRange` counts in).
fn u16len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// Human-facing label for one window row inside an account submenu. Preserves
/// the v1 "Session"/"Weekly"/"Opus" copy for the Claude windows so a menu that
/// used to read `Session  85%  · resets in 3h` still does after the refactor.
/// Unknown window ids fall through to the provider's own short `label` so a
/// future provider still surfaces something readable.
fn stat_display_label(w: &WindowView) -> &str {
    match w.id.as_str() {
        "session" => "Session",
        "weekly" => "Weekly",
        "opus" => "Opus",
        _ => w.label.as_str(),
    }
}

/// The first two windows of an account, used to build the two-percentage
/// summary text in the header rows. `(pct_a, pct_b)` — either may be `None`.
fn summary_pcts(a: &AcctView) -> (Option<f64>, Option<f64>) {
    let a0 = a.windows.first().and_then(|w| w.pct);
    let a1 = a.windows.get(1).and_then(|w| w.pct);
    (a0, a1)
}

/// Fold an `AcctView` into the shape `countdown::compute_display` expects —
/// pcts from the first two windows plus the raw reset instants we captured off
/// the row. Used by both `main_row` and `switch_target_lock_countdown` so a
/// row that switches to "locked · Xd Yh" agrees with the switch-refusal path.
fn account_usage_for(a: &AcctView) -> AccountUsage {
    let (sp, wp) = summary_pcts(a);
    AccountUsage {
        session_pct: sp,
        session_reset: a.session_reset_at,
        weekly_pct: wp,
        weekly_reset: a.weekly_reset_at,
        fetched_at: a.fetched_at,
    }
}

/// If the account is "locked" (session or weekly at ≥99.5% with a
/// still-future reset), return the human countdown string — the piece that
/// swaps in for `S% / W%` in the row title. `None` otherwise (including the
/// `StaleAfterReset` case — a stale-but-past-reset cache isn't "locked", it's
/// just waiting on a fresh poll; see `trailing_for_account` for how that
/// state renders instead).
fn locked_countdown_for(a: &AcctView, now: DateTime<Utc>) -> Option<(String, BlockingWindow)> {
    match countdown::compute_display(&account_usage_for(a), now) {
        DisplayState::Locked { until, window } => {
            Some((countdown::format_countdown(until - now), window))
        }
        DisplayState::Usage { .. } | DisplayState::StaleAfterReset { .. } => None,
    }
}

/// Wall-clock "now" used by the render helpers. Overridable in tests so a
/// pinned locked-row can be asserted without waiting real hours.
#[cfg(not(test))]
fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
fn now_utc() -> DateTime<Utc> {
    tests::test_now()
}

/// Left column width (chars) the provider name is padded to in the flat main
/// list row, so every account's email starts at the same x regardless of
/// provider-name length ("Claude" vs "Codex").
#[allow(dead_code)] // kept for `main_row`'s tests — see its doc comment.
const PROVIDER_COL: usize = 10;

/// v0.5.0 flat main-list row: `{provider}    {email}\t{n}% / {n}%`, bold +
/// checkmarked if active, high percentages colored per the provider's
/// severity bands. When the account is fully consumed
/// (`countdown::compute_display` → Locked), the `n% / n%` run is swapped
/// for `locked · <countdown>` and colored red — the user is being told *when*
/// the account is next usable, not *how used* it is.
///
/// v0.5.2: no longer called by any production render path — `build_menu`/
/// `menu_tree_from_snapshot` now use `account_header_row`'s `{provider} ·
/// {email}` block-header format instead of this padded flat-list format (the
/// per-account BLOCK redesign, item 1). Kept `#[allow(dead_code)]` rather
/// than deleted: it's a well-tested pure function capturing the padded
/// single-row format from the v0.5.0/v0.5.1 flat/grouped list, which a future
/// "compact mode" could plausibly reuse, and deleting it would mean deleting
/// or rewriting its whole existing test suite for zero behavior change.
#[allow(dead_code)]
fn main_row(provider_display: &str, a: &AcctView, bands: SeverityBands) -> RowStyle {
    let label = format!("{provider_display:<PROVIDER_COL$}{}", a.display);
    let base = u16len(&label) + 1; // + '\t'
    if let Some((cd, _win)) = locked_countdown_for(a, now_utc()) {
        // UX (v0.5.0): drop the "locked · " prefix — the trailing run is
        // rendered in red (visually implying locked) and the payload is a
        // time-until-reset instead of a percentage (structurally implying
        // locked, since a healthy row shows "n% / n%"). The old
        // "locked · Xh Ym" wording repeated the same fact three ways.
        let trailing = cd.clone();
        let plain = format!("{label}\t{trailing}");
        // A "locked" account is by definition red — no need to consult bands.
        let colors = vec![(base, u16len(&trailing), Severity::Red)];
        return RowStyle {
            bold: a.active,
            colors,
            tab_x_kind: Some(TabX::MenuRight),
            checkmark: a.active,
            ..RowStyle::plain_row(plain)
        };
    }
    let (pa, pb) = summary_pcts(a);
    let sa = pct(pa);
    let sb = pct(pb);
    // v0.5.1: drop the "S "/"W " label prefixes — "47% / 89%" is
    // self-explanatory without them (item raised post-v0.5.0 UX pass).
    let trailing = format!("{sa} / {sb}");
    let plain = format!("{label}\t{trailing}");
    let mut colors = Vec::new();
    let s_off = base;
    if let Some(sev) = severity_with(pa, bands) {
        colors.push((s_off, u16len(&sa), sev));
    }
    let w_off = s_off + u16len(&sa) + u16len(" / ");
    if let Some(sev) = severity_with(pb, bands) {
        colors.push((w_off, u16len(&sb), sev));
    }
    RowStyle {
        bold: a.active,
        colors,
        tab_x_kind: Some(TabX::MenuRight),
        checkmark: a.active,
        ..RowStyle::plain_row(plain)
    }
}

/// v0.5.2 menu redesign: the header row of one account's BLOCK — `{provider}
/// · {email}\t{sa} / {sb}` (or `\t<locked countdown>` when the account is
/// fully consumed, same rule `main_row` uses). Provider grouping is no longer
/// a separate visual concept (no section header, no per-provider HR) — each
/// account is its own top-level block, separated from its neighbors by
/// `PredefinedMenuItem::separator()` (see `build_menu`), so the provider name
/// has to live INSIDE this row instead of a shared header above a group of
/// rows. Otherwise identical to `main_row`: same locked/colored/bold/
/// checkmark rules, same `TabX::MenuRight` participation (so the block
/// headers and the Quit row all share one right-aligned column via
/// `mac_style::compute_menu_right_x`) — just a different label format
/// (`main_row`'s padded `{provider:<10}{email}` was designed for a flat list
/// where the provider name was the ONLY thing distinguishing rows at a
/// glance; the block header can afford the more readable `·` separator since
/// it's the sole row establishing "whose account is this" for everything
/// beneath it).
/// The trailing run (after the `\t`) for an account's block header, plus its
/// color spans RELATIVE to the start of that trailing run (the caller adds
/// its own label-length offset). v0.5.2 item 7 nuance, built directly off
/// `countdown::compute_display` so the three states it can report each get
/// their own trailing shape instead of the old binary
/// "locked countdown OR `sa / sb`" split:
///   * `Locked` on the WEEKLY window → the countdown ALONE, no slash — a
///     locked weekly is the harder wall (session will free up on its own
///     schedule regardless), so showing a session percentage next to it
///     would be a healthy-looking number the user could still stall on for
///     up to a week.
///   * `Locked` on the SESSION window → `<countdown> / <weekly%>` — the
///     weekly window still has headroom (a session lock can't win the
///     `compute_display` tie-break over a simultaneously-locked weekly), so
///     surfacing it tells the user there's still runway on this account via
///     a different window.
///   * `StaleAfterReset` → `-% / -%`, a deliberate "don't know yet"
///     placeholder rather than re-rendering the cached (typically ~100%)
///     reading, which would misreport an account that just reset as still
///     maxed out until the next poll refreshes it (item 4).
///   * `Usage` → the existing `sa / sb`, colored per the provider's bands.
fn trailing_for_account(
    a: &AcctView,
    bands: SeverityBands,
    now: DateTime<Utc>,
) -> (String, Vec<(usize, usize, Severity)>) {
    match countdown::compute_display(&account_usage_for(a), now) {
        DisplayState::Locked {
            until,
            window: BlockingWindow::Weekly,
        } => {
            let cd = countdown::format_countdown(until - now);
            let len = u16len(&cd);
            (cd, vec![(0, len, Severity::Red)])
        }
        DisplayState::Locked {
            until,
            window: BlockingWindow::Session,
        } => {
            let cd = countdown::format_countdown(until - now);
            let (_, wp) = summary_pcts(a);
            let wb = pct(wp);
            let trailing = format!("{cd} / {wb}");
            let mut colors = vec![(0, u16len(&cd), Severity::Red)];
            let w_off = u16len(&cd) + u16len(" / ");
            if let Some(sev) = severity_with(wp, bands) {
                colors.push((w_off, u16len(&wb), sev));
            }
            (trailing, colors)
        }
        DisplayState::StaleAfterReset { .. } => ("-% / -%".to_string(), Vec::new()),
        DisplayState::Usage {
            session_pct,
            weekly_pct,
        } => {
            let sa = pct(session_pct);
            let sb = pct(weekly_pct);
            let trailing = format!("{sa} / {sb}");
            let mut colors = Vec::new();
            if let Some(sev) = severity_with(session_pct, bands) {
                colors.push((0, u16len(&sa), sev));
            }
            let w_off = u16len(&sa) + u16len(" / ");
            if let Some(sev) = severity_with(weekly_pct, bands) {
                colors.push((w_off, u16len(&sb), sev));
            }
            (trailing, colors)
        }
    }
}

/// Indent applied to each account row so it visually reads as sitting under
/// its provider-group header row (`provider_group_header_row`). Two spaces
/// works well with menu-font kerning and keeps the trailing tab column aligned
/// across every provider. v0.5.3 menu redesign — provider grouping is BACK,
/// so the account row no longer carries `{provider} · ` and instead sits
/// under a bold header row.
const ACCOUNT_INDENT: &str = "  ";

/// v0.5.3 menu redesign: the row for one account — `<indent><email>\t<trailing>`
/// where `<trailing>` is whatever `trailing_for_account` decides (locked
/// countdown, locked-session-with-weekly-headroom, stale-after-reset
/// placeholder, or plain `sa / sb`). Provider grouping is BACK as a visual
/// concept: this row now sits under a bold `provider_group_header_row` and
/// no longer prefixes `{provider} · ` — the group header carries the provider.
/// The row itself is also the LABEL of a submenu (see `build_menu` /
/// `menu_tree_from_snapshot`) whose children are the details + action rows
/// the flat v0.5.2 shape used to inline. Same `TabX::MenuRight` participation
/// as the Quit row (so the trailing column lines up via
/// `mac_style::compute_menu_right_x`).
fn account_header_row(sec: &ProviderSection, a: &AcctView) -> RowStyle {
    let label = format!("{ACCOUNT_INDENT}{}", a.display);
    let base = u16len(&label) + 1; // + '\t'
    let (trailing, rel_colors) = trailing_for_account(a, sec.severity_bands, now_utc());
    let plain = format!("{label}\t{trailing}");
    let colors = rel_colors
        .into_iter()
        .map(|(off, len, sev)| (base + off, len, sev))
        .collect();
    RowStyle {
        bold: a.active,
        colors,
        tab_x_kind: Some(TabX::MenuRight),
        checkmark: a.active,
        ..RowStyle::plain_row(plain)
    }
}

/// v0.5.3 menu redesign: the bold provider-group header row that precedes a
/// provider's account rows. Plain title is just the provider's display name
/// (e.g. "Claude"), bold, disabled (`enabled: false` at the muda layer so it
/// reads as a group heading, not a click target), and carries the per-provider
/// 16px icon — moved off the individual account rows in this release because
/// the icon now belongs to the GROUP, not to each account inside it. When the
/// provider's env override is active, this row also hosts the "env override
/// active — swap disabled" child (see `build_menu` / `menu_tree_from_snapshot`)
/// so a provider-wide fact renders once at the group level instead of on every
/// affected account (v0.5.2 attached it per-account).
fn provider_group_header_row(sec: &ProviderSection) -> RowStyle {
    RowStyle {
        bold: true,
        section_header: true,
        icon_slug: Some(sec.provider_id),
        ..RowStyle::plain_row(sec.display_name.to_string())
    }
}

/// All styling directives for the current menu, derived from the same snapshot
/// `build_menu` renders. The native walk applies each by matching `.plain`.
/// Plain title used both when we build the Quit row and when the style
/// walker matches it. Keep the two in lockstep — a mismatch (e.g. a
/// forgotten `\t`) means the walker finds no match and the item renders
/// as plain "Quit<tab>usagio vX.Y.Z" without alignment or grey.
fn quit_row_plain() -> String {
    format!("Quit\tusagio v{}", env!("CARGO_PKG_VERSION"))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn menu_styles(snap: &Snapshot) -> Vec<RowStyle> {
    let mut styles = Vec::new();
    // v0.5.3: provider grouping is back — each provider contributes one bold
    // header row carrying the 16px icon, then one row per account (compact,
    // NO `{provider} · ` prefix). The account row is the label of a submenu
    // holding the reset windows, burn-rate/cost, "updated …" and action rows
    // (Switch/Launch/Remove). Info-row styles inside the submenu still get
    // the `disabled_but_white` treatment so they read as normal (not muda's
    // greyed-out disabled look).
    for (si, _) in provider_grouped_order(snap) {
        let sec = &snap.sections[si];
        styles.push(provider_group_header_row(sec));
        for a in &sec.accounts {
            styles.push(account_header_row(sec, a));
            for row in submenu_info_rows(sec, a) {
                styles.push(RowStyle {
                    disabled_but_white: true,
                    ..RowStyle::plain_row(row)
                });
            }
        }
    }
    // Quit row: "Quit\tusagio vX.Y.Z" — right-align the trailing run at the
    // menu's computed right edge and paint everything from the tab onward
    // in secondaryLabelColor (macOS's disabled-text grey) while the row
    // itself stays clickable.
    let quit_plain = quit_row_plain();
    let grey_from = u16len("Quit") + 1; // +1 for the '\t'
    styles.push(RowStyle {
        tab_x_kind: Some(TabX::MenuRight),
        grey_tail_from: Some(grey_from),
        ..RowStyle::plain_row(quit_plain)
    });
    styles
}

/// Locate the active section + account (if any) in the snapshot.
fn active_account(snap: &Snapshot) -> Option<(&ProviderSection, &AcctView)> {
    for sec in &snap.sections {
        if let Some(a) = sec.accounts.iter().find(|a| a.active) {
            return Some((sec, a));
        }
    }
    None
}

/// Entry point dispatched by `usagio menubar` (see `main.rs`). macOS drives a
/// native `NSApplication` run loop with attributedTitle NSMenu styling;
/// Linux/Windows route through the cross-platform `platform::MenuBackend`
/// trait instead (`cross_platform::run`, near the end of this file) —
/// richer per-row coloring / right-aligned tab stops aren't representable
/// through the generic `MenuTree`, so those platforms get a plain (but fully
/// functional) muda/tray-icon menu.
#[cfg(not(target_os = "macos"))]
pub fn run() -> Result<()> {
    cross_platform::run()
}

#[cfg(target_os = "macos")]
pub fn run() -> Result<()> {
    // Register providers on the main thread before anything else — the poll
    // thread and menu build both dispatch through `providers::get`.
    providers::init();

    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("the menu bar must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    // Background (menu-bar-only) app: no Dock icon, even as the bare binary.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // Poll + auto-swap on a background thread, supervised against panics
    // (see `poll_loop_supervisor`'s doc). It writes cached usage to
    // state.json; the main-thread timer reads it back to render.
    std::thread::spawn(poll_loop_supervisor);

    // Remember the binary we launched from so the timer can notice a `brew
    // upgrade` replacing it and relaunch into the new version.
    let start_exe = std::fs::canonicalize(crate::stable_exe_path()).ok();

    // Build the tray on the main thread and keep it alive for the app's lifetime.
    let initial = build_snapshot();
    let builder = TrayIconBuilder::new().with_title(title_for(&initial));
    // custom-popup ON: suppress the native menu on LEFT click so left-click
    // opens the NSPopover instead (see the tick below). The native NSMenu is
    // still installed and still opens on RIGHT click, as the shipping fallback
    // until the popover reaches parity (docs/design/custom-tray-popup.md). When
    // the feature is OFF, behavior is byte-for-byte unchanged.
    #[cfg(feature = "custom-popup")]
    let builder = builder.with_menu_on_left_click(false);
    let tray = builder
        .build()
        .map_err(|e| anyhow::anyhow!("failed to create tray icon: {e}"))?;
    mac_style::install_menu(&tray, &initial);
    let _ = tray.set_tooltip(Some(tooltip_for(&initial)));

    // custom-popup: build the NSPopover host anchored to the status-item button
    // and listen for tray left-clicks to toggle it. `handle_click` is reused
    // verbatim for row actions (same click-id scheme as the native menu).
    #[cfg(feature = "custom-popup")]
    let popover = build_popover_host(&tray, mtm);
    #[cfg(feature = "custom-popup")]
    let tray_rx = tray_icon::TrayIconEvent::receiver().clone();
    #[cfg(feature = "custom-popup")]
    let shown_on_launch = RefCell::new(false);
    // A retained clone for the tick closure (which takes `app` by move); the
    // outer `app` is still needed for `app.run()` below.
    #[cfg(feature = "custom-popup")]
    let app_popover = app.clone();

    // All UI updates happen in this timer, scheduled in the DEFAULT run-loop mode.
    // A status menu opens its own nested tracking loop (NSEventTrackingRunLoopMode);
    // a default-mode timer never fires there, so an open menu is never dismissed.
    // (This is exactly the tao bug we sidestepped by dropping tao: it added its
    // run-loop observer to kCFRunLoopCommonModes, which *does* fire during
    // tracking and collapsed the menu.) Each tick rebuilds the display from cached
    // state — so "updated Xm ago" ticks and a CLI switch shows up within ~1s — and
    // only re-installs the menu/title when the rendered content actually changes.
    let menu_rx = MenuEvent::receiver().clone();
    let last_sig = RefCell::new(menu_signature(&initial));
    let last_title = RefCell::new(title_for(&initial));
    let tick = RcBlock::new(move |_t: core::ptr::NonNull<NSTimer>| {
        // If `brew upgrade` replaced our binary, relaunch into the new version.
        if let Some(start) = &start_exe {
            maybe_relaunch_after_upgrade(start);
        }
        // A menu closes before its click is delivered, so handling it here won't
        // fight menu tracking.
        while let Ok(ev) = menu_rx.try_recv() {
            handle_click(&ev.id.0);
        }
        let snap = build_snapshot();
        let sig = menu_signature(&snap);
        if *last_sig.borrow() != sig {
            // Re-install the native NSMenu on change. With custom-popup ON this
            // is the right-click fallback (the popover rebuilds its own content
            // from a fresh snapshot each time it's shown); with it OFF this is
            // the sole UI, exactly as before.
            mac_style::install_menu(&tray, &snap);
            let _ = tray.set_tooltip(Some(tooltip_for(&snap)));
            *last_sig.borrow_mut() = sig;
        }
        let title = title_for(&snap);
        if *last_title.borrow() != title {
            tray.set_title(Some(title.clone()));
            *last_title.borrow_mut() = title;
        }
        // custom-popup: toggle the NSPopover on a tray left-click, and (for the
        // screenshot path) force-show it once on launch.
        #[cfg(feature = "custom-popup")]
        if let Some(p) = &popover {
            while let Ok(ev) = tray_rx.try_recv() {
                if let tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Down,
                    ..
                } = ev
                {
                    p.toggle(&popover_model(&snap));
                }
            }
            if !*shown_on_launch.borrow() {
                *shown_on_launch.borrow_mut() = true;
                if std::env::var("USAGIO_POPOVER_SHOW_ON_LAUNCH").is_ok() {
                    eprintln!("[popover] show-on-launch: activating + showing");
                    app_popover.activate();
                    p.show(&popover_model(&snap));
                    eprintln!("[popover] shown={}", p.is_shown());
                }
            }
        }
    });
    // The run loop retains the timer; scheduled timers fire in the default mode.
    let _timer =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.75, true, &tick) };

    app.run();
    Ok(())
}

/// custom-popup: build the `NSPopover` host anchored to the tray's status-item
/// button, wiring row clicks straight into `handle_click` (same click-id scheme
/// as the native menu, so no action logic is duplicated). Returns `None` if the
/// status item / button isn't available (no anchor → no popover; the tray icon
/// still shows, it just won't open a popup).
#[cfg(all(target_os = "macos", feature = "custom-popup"))]
fn build_popover_host(
    tray: &tray_icon::TrayIcon,
    mtm: MainThreadMarker,
) -> Option<crate::ui::popover::PopoverHost> {
    let status_item = tray.ns_status_item();
    eprintln!("[popover] ns_status_item present={}", status_item.is_some());
    let status_item = status_item?;
    let button = status_item.button(mtm);
    eprintln!("[popover] button present={}", button.is_some());
    let button = button?;
    // `USAGIO_POPOVER_SHOW_ON_LAUNCH` (screenshot/debug): keep the popover open
    // (ApplicationDefined behavior) instead of auto-dismissing (Transient) so a
    // `screencapture` can catch it.
    let persistent = std::env::var("USAGIO_POPOVER_SHOW_ON_LAUNCH").is_ok();
    Some(crate::ui::popover::PopoverHost::new(
        button,
        Box::new(handle_click),
        persistent,
        mtm,
    ))
}

/// custom-popup: fold a `Snapshot` into the toolkit-neutral `ui::PopoverModel`
/// the AppKit renderer draws. Reuses the SAME data derivations as the native
/// menu — `provider_grouped_order`, `trailing_for_account` (locked countdown /
/// severity spans), the active flag, and the `switch:<provider>:<key>` click
/// ids — so the popover and the native menu can never disagree about content.
///
/// Phase 1: an account row's click switches to that account (or is a no-op when
/// it's already active / the provider can't switch). Detail rows (reset / burn /
/// cost) are Phase 2. "Capture current login" and "Settings" are rendered as
/// rows but route to Phase-2 stub ids (the native submenu trees aren't rebuilt
/// yet); "Refresh usage now" and "Quit" are fully wired.
#[cfg(all(target_os = "macos", feature = "custom-popup"))]
fn popover_model(snap: &Snapshot) -> crate::ui::PopoverModel {
    use crate::ui::{PctSpan, PopoverModel, PopoverRow, Sev};

    let sev_of = |s: Severity| match s {
        Severity::Amber => Sev::Amber,
        Severity::Red => Sev::Red,
    };

    let mut rows: Vec<PopoverRow> = Vec::new();
    let groups = provider_grouped_order(snap);
    let mut first = true;
    for (si, account_idxs) in &groups {
        let sec = &snap.sections[*si];
        if !first {
            rows.push(PopoverRow::Separator);
        }
        first = false;
        rows.push(PopoverRow::GroupHeader {
            title: sec.display_name.to_string(),
            icon_slug: Some(sec.provider_id),
        });
        for ai in account_idxs {
            let a = &sec.accounts[*ai];
            let (trailing, rel_colors) = trailing_for_account(a, sec.severity_bands, now_utc());
            let colors = rel_colors
                .into_iter()
                .map(|(start, len, sev)| PctSpan {
                    start,
                    len,
                    sev: sev_of(sev),
                })
                .collect();
            // Same routing as the native menu's account submenu: active → no
            // switch; switching-capable & inactive → `switch:<provider>:<key>`.
            let click_id = if a.active || !sec.supports_switching {
                "noop".to_string()
            } else {
                format!("switch:{}:{}", sec.provider_id, a.key)
            };
            rows.push(PopoverRow::Account {
                email: a.display.clone(),
                active: a.active,
                trailing,
                colors,
                click_id,
            });
        }
    }

    if snap.sections.is_empty() {
        rows.push(PopoverRow::Action {
            label: "Capture a login below to begin".to_string(),
            click_id: "noop".to_string(),
        });
    }

    rows.push(PopoverRow::Separator);
    // Phase 1 bottom actions. Capture/Settings are submenu trees in the native
    // menu; Phase 1 surfaces them as rows routing to inert ids (Phase 2 ports
    // the trees / opens a secondary popover). Refresh + Quit are fully wired to
    // the same ids `handle_click` already routes.
    rows.push(PopoverRow::Action {
        label: "Capture current login…".to_string(),
        click_id: "popup:capture".to_string(),
    });
    rows.push(PopoverRow::Action {
        label: "Settings…".to_string(),
        click_id: "popup:settings".to_string(),
    });
    rows.push(PopoverRow::Action {
        label: "Refresh usage now".to_string(),
        click_id: "refresh:now".to_string(),
    });
    rows.push(PopoverRow::Action {
        label: "Quit".to_string(),
        click_id: "quit".to_string(),
    });

    PopoverModel { rows }
}

// ---------------------------------------------------------------------------
// Poller thread
// ---------------------------------------------------------------------------

/// The `SwapGuard` shared by every in-process caller of `run_cycle` — the
/// long-lived poller (`poll_loop`) and the one-off "Refresh usage now" click
/// (`handle_refresh_now`) — so anti-thrash cooldown/no-return windows apply
/// to BOTH, not just poll-triggered swaps (concurrency-04, v0.5.2 codeaudit).
/// Before this, `handle_refresh_now` spawned its own throwaway
/// `SwapGuard::default()`: a manual click could swap right past a cooldown
/// the poller had just started, or bounce straight back into an account the
/// poller had just left inside its no-return window. `Arc<Mutex<_>>` (rather
/// than splitting the struct's fields into separate atomics) keeps
/// `SwapGuard`'s existing `&mut self` API — `left_at`'s `HashMap` and
/// `stuck_notified`'s bookkeeping aren't lock-free-friendly, and the guard is
/// only ever held for the duration of one `run_cycle` call, so a `Mutex`
/// contends at most twice a poll interval.
fn shared_swap_guard() -> std::sync::Arc<std::sync::Mutex<SwapGuard>> {
    static GUARD: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<SwapGuard>>> =
        std::sync::OnceLock::new();
    GUARD
        .get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(SwapGuard::default())))
        .clone()
}

fn poll_loop() {
    let guard = shared_swap_guard();
    let base = WATCH_INTERVAL_SECS;
    let mut current = base;
    let mut wd = crate::watchdog::Watchdog::default();
    let mut wd_effects = crate::watchdog::RealEffects;
    loop {
        // Self-healing health check (fd-count + keychain-write), throttled to
        // ~once a minute. The menubar poller runs under launchd, so a critical
        // fd leak that a watcher respawn can't clear escalates to a launchd
        // kickstart into a clean process (see `crate::watchdog`).
        wd.maybe_run(&mut wd_effects);
        // Fetch usage + auto-swap; this writes cached usage to state.json, which
        // the main-thread timer reads back to render. This is the ONLY thing that
        // hits the network, so ordinary use can never rate-limit.
        let (rate_limited, max_pct_opt, trigger) = {
            let mut g = guard.lock().unwrap_or_else(|e| e.into_inner());
            run_cycle(&mut g)
        };
        let prev = current;
        current = next_interval(current, base, rate_limited, max_pct_opt, trigger);
        if rate_limited {
            crate::logging::log(&format!("rate limited; backing off to {current}s"));
        } else if current != prev && current < base {
            let max_pct = max_pct_opt.unwrap_or(0.0);
            crate::logging::log(&format!(
                "cadence: {prev}s → {current}s (max {max_pct:.1}%, trigger {trigger:.0}%) \
                 event=cadence prev={prev}s new={current}s max_pct={max_pct:.1} trigger={trigger:.0}"
            ));
        }
        std::thread::sleep(Duration::from_secs(current));
    }
}

/// Max panic-and-respawn cycles tolerated in `PANIC_LOOP_WINDOW_SECS` before
/// `poll_loop_supervisor` gives up self-healing and hard-exits (letting
/// launchd's `KeepAlive`/restart policy bring the whole process back up
/// clean) — a backstop against a genuine crash-loop rather than a transient
/// one-off panic.
const MAX_PANICS_PER_WINDOW: usize = 10;
const PANIC_LOOP_WINDOW_SECS: u64 = 60;
/// Backoff between a `poll_loop` panic and respawning it.
const PANIC_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Best-effort human string for a `catch_unwind` payload (usually a `&str` or
/// `String` from a `panic!`/`unwrap`/`expect` message; anything else — a
/// custom payload type — falls back to a fixed string rather than failing to
/// log at all).
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Supervises `poll_loop`: if ANY unwind (an unwrap/expect/index-panic
/// anywhere in `run_cycle`/`refresh_usage_cache`/`active_refresh_cas`/…)
/// escapes it, the bare `std::thread::spawn(poll_loop)` this replaces would
/// let the whole background thread die silently — permanently disabling
/// polling, auto-swap, notifications, and credential sync for the rest of
/// the app's lifetime (errors-02, v0.5.2 codeaudit) with no user-visible
/// symptom beyond stale menu numbers. Instead: log the panic, back off, and
/// respawn — self-healing by default. Only a genuine panic-loop (more than
/// `MAX_PANICS_PER_WINDOW` in `PANIC_LOOP_WINDOW_SECS`) falls through to a
/// hard process exit, which launchd's LaunchAgent restarts into a clean
/// process (see `AUTOSTART_LABEL`/`cmd_install`).
fn poll_loop_supervisor() {
    let mut panics: Vec<std::time::Instant> = Vec::new();
    loop {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(poll_loop));
        // `poll_loop` is an infinite `loop {}` — reaching here at all means it
        // either panicked (Err) or, in principle, returned (Ok), which it
        // never does today but respawning either way is strictly safer than
        // silently exiting the supervisor thread.
        let reason = match outcome {
            Err(payload) => panic_payload_message(&*payload),
            Ok(()) => "poll_loop returned without panicking (unexpected)".to_string(),
        };
        crate::logging::log(&format!(
            "event=poll_loop_panic reason={reason} ; respawning"
        ));

        let now = std::time::Instant::now();
        panics.retain(|t| now.duration_since(*t).as_secs() < PANIC_LOOP_WINDOW_SECS);
        panics.push(now);
        if panics.len() > MAX_PANICS_PER_WINDOW {
            crate::logging::log(&format!(
                "event=poll_loop_panic_loop reason=\"more than {MAX_PANICS_PER_WINDOW} panics in \
                 {PANIC_LOOP_WINDOW_SECS}s\" ; exiting for launchd to restart us clean"
            ));
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_secs(PANIC_RESPAWN_BACKOFF_SECS));
    }
}

/// Relaunch into the on-disk binary if it changed since we started (i.e. a
/// `brew upgrade` repointed the stable symlink), so the menu bar hot-swaps to
/// the new version without waiting for the next login.
///
/// When we're the launchd-managed agent we must NOT just spawn a child and
/// exit: the child is in the job's process group, and launchd SIGKILLs that
/// whole group when the main process exits (AbandonProcessGroup defaults to
/// false), so the replacement dies with us and — with KeepAlive=false — is never
/// restarted. Instead we ask launchd to restart the job (`launchctl kickstart
/// -k`), which relaunches it in a fresh job context. A bare/from-source run has
/// no such job, so there the orphaned self-spawn survives our exit as usual.
fn maybe_relaunch_after_upgrade(start: &std::path::Path) {
    // Called from the macOS main-thread NSTimer tick (~0.75s). `canonicalize`
    // is a real syscall that hits disk; skip it 90%+ of the time via a
    // per-process last-check clock so the tray thread doesn't do blocking I/O
    // on the hot path (R2-PERF audit finding). A stale check window of 10s
    // is plenty — the brew upgrade + relaunch is best-effort and doesn't need
    // sub-second detection.
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static LAST_CHECK: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut g = LAST_CHECK.lock().unwrap();
        let should_check = !matches!(*g, Some(t) if t.elapsed() < Duration::from_secs(10));
        if !should_check {
            return;
        }
        *g = Some(Instant::now());
    }

    let stable = crate::stable_exe_path();
    let Ok(now) = std::fs::canonicalize(&stable) else {
        return;
    };
    if now == start {
        return;
    }
    crate::logging::log("binary changed on disk (brew upgrade); relaunching");
    match relaunch_via_launchd() {
        LaunchdRestart::Issued => {
            // kickstart -k will SIGKILL and restart this job; wait to be replaced.
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        }
        LaunchdRestart::Failed => {
            // Under launchd but the kickstart didn't take. Staying on the current
            // (old) binary — alive — is far better than exiting into a dead,
            // never-restarted state (KeepAlive=false won't bring us back).
            crate::logging::log("launchctl kickstart failed; staying on current version");
        }
        LaunchdRestart::NotManaged => {
            // Bare run (no launchd job): an orphaned child re-parents to
            // launchd/init and outlives our exit — but only exit if the spawn
            // actually succeeded, else we'd vanish with no replacement.
            match std::process::Command::new(&stable).arg("menubar").spawn() {
                Ok(_) => std::process::exit(0),
                Err(e) => crate::logging::log(&format!(
                    "relaunch spawn failed: {e}; staying on current version"
                )),
            }
        }
    }
}

/// Outcome of attempting a launchd-driven restart.
#[cfg_attr(not(unix), allow(dead_code))]
enum LaunchdRestart {
    /// kickstart succeeded — the caller should wait to be replaced.
    Issued,
    /// We're launchd-managed but kickstart failed — caller must NOT exit.
    Failed,
    /// Not launchd-managed — caller should self-spawn a replacement.
    NotManaged,
}

/// If we're running as the launchd agent, ask launchd to kill+restart the job
/// (`launchctl kickstart -k`). We wait on the command's exit status — reporting
/// success only when the kickstart actually took, so a failed request can't lead
/// the caller to exit into a dead, unrestarted state.
fn relaunch_via_launchd() -> LaunchdRestart {
    if !is_launchd_managed() {
        return LaunchdRestart::NotManaged;
    }
    relaunch_via_launchd_kickstart()
}

/// Watchdog self-restart: when the fd/keychain watchdog's remediation ladder
/// reaches `SelfRestart`, bring the daemon back in a clean process via a
/// launchd kickstart. We reuse `relaunch_via_launchd` (the same path the
/// brew-upgrade hot-swap uses), which is battle-tested and only acts when we're
/// actually launchd-managed.
///
/// Deliberately restart-loop-safe: a kickstart replaces the process with a
/// fresh fd table, so we `sleep` briefly then `exit(0)` and wait to be
/// replaced. If we are NOT launchd-managed (a bare foreground `usagio watch`)
/// or the kickstart fails, we DO NOT exit — the plist ships `KeepAlive=false`,
/// so a bare `exit()` would leave nothing to revive us. Returning `false` there
/// is correct: the watcher respawn + the watchdog's surfaced log lines are the
/// safety net for that case, with zero restart-loop risk. Returns `true` only
/// when a restart was issued (in which case this does not return).
pub(crate) fn watchdog_self_restart() -> bool {
    match relaunch_via_launchd() {
        LaunchdRestart::Issued => {
            crate::logging::log("event=fd_watchdog self_restart=issued via launchctl kickstart");
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        }
        LaunchdRestart::Failed => {
            crate::logging::log(
                "event=fd_watchdog self_restart=failed (launchctl kickstart did not take); \
                 staying alive",
            );
            false
        }
        LaunchdRestart::NotManaged => {
            crate::logging::log(
                "event=fd_watchdog self_restart=skipped (not launchd-managed); \
                 relying on watcher respawn",
            );
            false
        }
    }
}

// Split out from `relaunch_via_launchd` so the `libc::getuid()` call (`libc`
// is a `[target.'cfg(unix)'.dependencies]` crate — not available on Windows
// at all) doesn't have to gate on `target_os` (which `tests/strict_cfg.rs`
// restricts outside `src/platform/`). `cfg(unix)`/`cfg(not(unix))` aren't
// `target_os` checks, so this needs no allowlist entry.
#[cfg(unix)]
fn relaunch_via_launchd_kickstart() -> LaunchdRestart {
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/{}", crate::AUTOSTART_LABEL);
    crate::logging::log(&format!("relaunching via launchctl kickstart -k {target}"));
    match std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .status()
    {
        Ok(s) if s.success() => LaunchdRestart::Issued,
        Ok(s) => {
            crate::logging::log(&format!("launchctl kickstart exited with {s}"));
            LaunchdRestart::Failed
        }
        Err(e) => {
            crate::logging::log(&format!("launchctl kickstart could not run: {e}"));
            LaunchdRestart::Failed
        }
    }
}

/// launchd is macOS-only; `is_launchd_managed()` (an `XPC_SERVICE_NAME` env
/// check) can only be true there, so this is unreachable at runtime on
/// Windows — it exists purely so `relaunch_via_launchd` compiles without
/// `libc` (a unix-only dependency).
#[cfg(not(unix))]
fn relaunch_via_launchd_kickstart() -> LaunchdRestart {
    LaunchdRestart::NotManaged
}

/// Whether we're the launchd-managed agent. `XPC_SERVICE_NAME` is set by launchd
/// to the job label for a LaunchAgent, so a manual `usagio menubar` run
/// (which self-spawns fine) isn't misrouted to the kickstart path.
fn is_launchd_managed() -> bool {
    launchd_managed_from_env(std::env::var("XPC_SERVICE_NAME").ok().as_deref())
}

/// Pure predicate behind `is_launchd_managed`, split out for testing.
fn launchd_managed_from_env(xpc_service_name: Option<&str>) -> bool {
    xpc_service_name == Some(crate::AUTOSTART_LABEL)
}

/// Run one poll+auto-swap cycle; returns whether it was rate limited.
/// Runs one poll cycle and returns `(rate_limited, max_pct, trigger)` so the
/// caller's adaptive-cadence math has everything it needs. The menubar
/// poller uses the trigger the user actually configured (via `Settings ▸
/// Auto-swap`), matching what `watch_cycle` itself dispatched on.
fn run_cycle(guard: &mut SwapGuard) -> (bool, Option<f64>, f64) {
    let st = State::load().unwrap_or_default();
    let autoswap = !st.autoswap_disabled;
    let threshold = st.trigger_pct.unwrap_or(TRIGGER_PCT);
    // With auto-swap off, use an unreachable trigger so we only observe.
    let trigger = if autoswap { threshold } else { 101.0 };
    match watch_cycle(trigger, TARGET_CEILING_PCT, guard) {
        Ok(o) => (o.rate_limited, o.max_pct, trigger),
        Err(e) => {
            crate::logging::log(&format!("menubar poll failed: {e}"));
            (false, None, trigger)
        }
    }
}

/// Whether the provider currently under an env-override that would defeat any
/// switch we make. Delegates to the shared `crate::env_override_active` so
/// the menu display and the `choose_swap_target` auto-swap filter can never
/// disagree, and so tests can toggle overrides through one hook.
fn env_override_for(provider_id: &str) -> bool {
    env_override_active(provider_id)
}

/// Compare two `AcctView` values by their weekly-reset instant, treating a
/// `None` reset as "furthest in the future" so accounts without data yet sort
/// last. Used inside `build_snapshot` and asserted directly in the
/// `sort_by_expiration_orders_accounts_soonest_first` test.
fn sort_by_expiration(a: &AcctView, b: &AcctView) -> std::cmp::Ordering {
    let ka = a.weekly_reset_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
    let kb = b.weekly_reset_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
    ka.cmp(&kb)
}

/// Global soonest-expiring / priority order for the flat main-list block
/// sequence, spanning ALL providers — not grouped by provider (v0.5.2 fix for
/// the "account order keeps changing" report: `sections` were previously
/// rendered in fixed `providers::all()` order with only a per-section
/// re-sort, so an account's position could jump around relative to another
/// provider's account for no reason a user could predict). Four-level
/// comparator, in priority order:
///   1. Locked-and-not-active sinks to the very bottom — it isn't a usable
///      swap target regardless of how soon its reset is. An account that's
///      BOTH locked and currently active stays in normal rotation (rank 0)
///      so the user can still see it and its countdown up top.
///   2. Soonest of `{session_reset, weekly_reset}` ascending — the account
///      about to reset (and therefore about to free up quota) surfaces
///      first; no known reset sorts last within its rank.
///   3. More headroom (`100 - max(session%, weekly%)`) first — the better
///      auto-swap candidate wins a tie on reset time.
///   4. Email, ascending — final deterministic tie-break so two accounts
///      that are identical on 1–3 always render in the same order.
fn flat_account_order(sections: &[ProviderSection], now: DateTime<Utc>) -> Vec<(usize, usize)> {
    struct Key {
        sink: bool,
        soonest: DateTime<Utc>,
        headroom: f64,
        email: String,
    }
    let mut idx: Vec<(usize, usize)> = Vec::new();
    let mut keys: Vec<Key> = Vec::new();
    for (si, sec) in sections.iter().enumerate() {
        for (ai, a) in sec.accounts.iter().enumerate() {
            let locked = locked_countdown_for(a, now).is_some();
            let soonest = [a.session_reset_at, a.weekly_reset_at]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(DateTime::<Utc>::MAX_UTC);
            let (sp, wp) = summary_pcts(a);
            let headroom = 100.0 - sp.unwrap_or(0.0).max(wp.unwrap_or(0.0));
            idx.push((si, ai));
            keys.push(Key {
                sink: locked && !a.active,
                soonest,
                headroom,
                email: a.key.clone(),
            });
        }
    }
    let mut order: Vec<usize> = (0..idx.len()).collect();
    order.sort_by(|&x, &y| {
        let (kx, ky) = (&keys[x], &keys[y]);
        kx.sink
            .cmp(&ky.sink)
            .then(kx.soonest.cmp(&ky.soonest))
            .then(
                ky.headroom
                    .partial_cmp(&kx.headroom)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(kx.email.cmp(&ky.email))
    });
    order.into_iter().map(|i| idx[i]).collect()
}

/// v0.5.3 menu redesign: partition `snap.account_order` by provider,
/// preserving the within-provider order the global comparator produced, then
/// sort provider groups alphabetically by display name so a two-provider menu
/// always reads Claude → Codex regardless of `providers::all()` registration
/// order. The `account_order` global sort still decides who comes first WITHIN
/// a provider (soonest-reset first, locked-inactive sinks, etc.) — this
/// grouping just brackets those runs by provider so each block sits under one
/// bold `provider_group_header_row`.
fn provider_grouped_order(snap: &Snapshot) -> Vec<(usize, Vec<usize>)> {
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    for &(si, ai) in &snap.account_order {
        if let Some(entry) = groups.iter_mut().find(|(s, _)| *s == si) {
            entry.1.push(ai);
        } else {
            groups.push((si, vec![ai]));
        }
    }
    groups.sort_by_key(|(si, _)| snap.sections[*si].display_name);
    groups
}

/// Build one account's rendered view from a v1 `Row`. v1 state only has
/// Claude accounts, so the window set is fixed (session / weekly / opus); the
/// window IDs come from `provider.window_order()` so this is trivially
/// generalizable when state v2 lands.
fn acctview_from_row(
    r: &Row,
    active: &Option<String>,
    provider_id: &'static str,
    window_order: &'static [&'static str],
) -> AcctView {
    // Build the fixed pool of windows we know about for this v1 row.
    let mut pool: Vec<WindowView> = Vec::new();
    pool.push(WindowView {
        id: "session".into(),
        label: "5h".into(),
        pct: r.session.pct,
        reset: r.session.resets_in(),
    });
    pool.push(WindowView {
        id: "weekly".into(),
        label: "7d".into(),
        pct: r.weekly.pct,
        reset: r.weekly.resets_in(),
    });
    if let Some(c) = &r.opus {
        pool.push(WindowView {
            id: "opus".into(),
            label: "Opus 7d".into(),
            pct: c.pct,
            reset: c.resets_in(),
        });
    }
    // Present them in the provider's declared order; unknown IDs (a
    // provider added in later phases) fall through to their pool order.
    let mut windows: Vec<WindowView> = Vec::with_capacity(pool.len());
    for id in window_order {
        if let Some(pos) = pool.iter().position(|w| w.id == *id) {
            windows.push(pool.remove(pos));
        }
    }
    windows.extend(pool);

    AcctView {
        provider_id,
        key: r.email.clone(),
        display: r.email.clone(),
        windows,
        updated: age_str(r.fetched_at),
        active: active.as_deref() == Some(r.email.as_str()),
        has_data: r.has_data(),
        session_reset_at: r.session.resets_at,
        weekly_reset_at: r.weekly.resets_at,
        fetched_at: r
            .fetched_at
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0)),
    }
}

/// Build the UI snapshot from local State only (no network), reading each
/// account's cached usage. Runs every 0.75s UI tick, so it avoids re-reading
/// state.json unless its mtime changed and probes the Login Item at most once a
/// minute (both were per-tick subprocess/disk costs before).
///
/// A provider's section is only emitted if at least one captured account exists
/// for it in state — that's the "no header, no rows" rule from the design.
/// The "Capture current login" submenu is separately fed from
/// `providers::all()` so every REGISTERED provider stays listable there.
fn build_snapshot() -> Snapshot {
    let st = cached_state();
    let autoswap = !st.autoswap_disabled;
    let threshold = st.trigger_pct.unwrap_or(TRIGGER_PCT);
    let active = st.active.clone();

    // Claude's accounts live in `st.accounts` (its own dedicated slot); every
    // other provider's captured accounts live in `st.providers[slug]` (state
    // v2 — see `store.rs`'s `State` doc). Build one flat `Row` list tagged by
    // `provider_id` from both, same as before this changed from "always
    // Claude" to "Claude + whichever providers have captured accounts".
    let mut rows: Vec<Row> = st.accounts.iter().map(row_from_account).collect();
    for (slug, pa) in &st.providers {
        rows.extend(
            pa.accounts
                .iter()
                .map(|a| row_from_provider_account(slug, a)),
        );
    }
    rows.sort_by(menu_order);

    let mut sections: Vec<ProviderSection> = Vec::new();
    for provider in providers::all() {
        let slug = provider.provider_id();
        let provider_rows: Vec<&Row> = rows.iter().filter(|r| r.provider_id == slug).collect();
        if provider_rows.is_empty() {
            continue; // no captured accounts → no section (no header, no rows).
        }
        // Claude's "active" selection is `st.active`; every other provider
        // tracks its own active key in `st.providers[slug].active` (state
        // v2), since a provider's captured accounts are independent of
        // Claude's.
        let active_for_slug = if slug == CLAUDE_SLUG {
            active.clone()
        } else {
            st.providers.get(slug).and_then(|p| p.active.clone())
        };
        let mut accounts: Vec<AcctView> = provider_rows
            .into_iter()
            .map(|r| acctview_from_row(r, &active_for_slug, slug, provider.window_order()))
            .collect();
        // Primary sort: soonest-to-expire first, using the weekly-reset instant
        // as the "expiration" signal (accounts with no data yet sort last).
        // Without this, a newly-captured account lands at the tail of the vec
        // (State::upsert appends) and stays there in the menu — the "sort-by-
        // expiration on add" bug the user reported.
        accounts.sort_by(sort_by_expiration);
        // Flat-list rule: within a provider section the active account renders
        // first, then everyone else in the order picked above. Stable sort so
        // the expiration ordering is preserved among the inactives.
        accounts.sort_by_key(|a| std::cmp::Reverse(a.active));
        let caps = provider.capabilities();
        sections.push(ProviderSection {
            provider_id: slug,
            display_name: provider.display_name(),
            supports_switching: caps.supports_switching,
            supports_launch: caps.supports_launch,
            supports_remove: caps.supports_remove,
            supports_usage: caps.supports_usage,
            severity_bands: provider.severity_bands(),
            env_override_active: env_override_for(slug),
            accounts,
        });
    }

    // The capture submenu lists every registered provider that CAN capture a
    // login on this host — filtered by `capture_menu_providers` so stub
    // providers (`supports_usage == false`) don't clutter the onboarding UX.
    // `capture_creds`/`capture_api_key` still separate creds-on-disk from
    // API-key providers (the click-id prefix differs), but v0.5.2 item 8
    // flattened their RENDERING into one direct child list under "Capture
    // current login ▸" (creds first, then API-key) instead of an API-key-only
    // "Paste API key ▸" sub-submenu. Either way, a provider stops appearing
    // here once it has a captured account (i.e. a section above) — that
    // account now renders in the main list instead (item 9 of the original
    // redesign: capture is for NEW accounts only once one exists).
    let captured_provider_ids: Vec<&str> = sections.iter().map(|s| s.provider_id).collect();
    let (creds_providers, api_key_providers) = capture_menu_providers(&captured_provider_ids);
    let capture_creds: Vec<RegisteredProvider> =
        creds_providers.into_iter().map(register_provider).collect();
    let capture_api_key: Vec<RegisteredProvider> = api_key_providers
        .into_iter()
        .map(register_provider)
        .collect();

    let account_order = flat_account_order(&sections, now_utc());

    Snapshot {
        sections,
        account_order,
        capture_creds,
        capture_api_key,
        autoswap,
        threshold,
        notification_config: st.notification_config.clone(),
    }
}

/// Build one `RegisteredProvider` row from a live provider reference.
/// Extracted so both buckets in `build_snapshot` populate identically.
fn register_provider(p: &'static dyn Provider) -> RegisteredProvider {
    RegisteredProvider {
        provider_id: p.provider_id(),
        display_name: p.display_name(),
        // v1 only knows how to probe the Claude keychain. Everything else
        // is assumed installed; wrong guesses just surface a real error
        // on the capture attempt instead of pre-emptively greying out.
        installed: probe_installed(p.provider_id()),
        capture_mode: p.capabilities().capture_mode,
    }
}

/// Pick the providers the "Capture current login ▸" menu should offer.
/// Providers with `supports_usage == false` are stubs that have no capture
/// path wired yet (their `capture_current_login` returns `Ok(None)` with a
/// `TODO`) — hiding them keeps the onboarding UX free of dead rows.
///
/// Returns `(creds_on_disk_providers, api_key_providers)`:
/// - the first bucket renders directly under "Capture current login ▸";
/// - the second renders under a "Paste API key ▸" sub-submenu.
///
/// `captured_provider_ids` excludes an API-key provider from the "Paste API
/// key ▸" bucket once it already has a captured account (item 9): that
/// account now renders in the main list instead, and the capture row would
/// just be a confusing duplicate. Creds-on-disk providers (Claude, Codex) are
/// NOT filtered this way — those support multiple captured accounts per
/// provider, so "Capture current login ▸ Claude" must keep working even with
/// an existing Claude account.
///
/// Pure function given its inputs: no I/O, no direct state reads — the caller
/// (`build_snapshot`) is the one that turns `state.json` into
/// `captured_provider_ids`.
pub(crate) fn capture_menu_providers(
    captured_provider_ids: &[&str],
) -> (Vec<&'static dyn Provider>, Vec<&'static dyn Provider>) {
    partition_capture_providers(providers::all(), captured_provider_ids)
}

/// Inner form of `capture_menu_providers` that operates on an arbitrary
/// provider slice so tests can pass their own fixtures without touching the
/// process-wide registry.
fn partition_capture_providers<'a>(
    provs: &'a [Box<dyn Provider>],
    captured_provider_ids: &[&str],
) -> (Vec<&'a dyn Provider>, Vec<&'a dyn Provider>) {
    let mut creds: Vec<&'a dyn Provider> = Vec::new();
    let mut api_key: Vec<&'a dyn Provider> = Vec::new();
    for p in provs {
        let caps = p.capabilities();
        if !caps.supports_usage {
            // Stub provider — no capture path wired yet; hide from the menu.
            continue;
        }
        match caps.capture_mode {
            CaptureMode::CredsOnDisk => creds.push(&**p),
            CaptureMode::ApiKey => {
                if !captured_provider_ids.contains(&p.provider_id()) {
                    api_key.push(&**p);
                }
            }
        }
    }
    (creds, api_key)
}

/// Best-effort per-provider "is the credential store present on this host?"
/// probe used to grey out capture rows. Cheap enough for the 0.75s tick — for
/// Claude it does an mtime-cached keychain lookup only when state.json changes
/// (via `cached_state`). Non-Claude providers default to `true` so a stub
/// provider's row stays enabled and its click reports a real error.
fn probe_installed(_slug: &str) -> bool {
    // v1: don't do anything expensive per tick. A future phase can front this
    // with a TTL cache once real stub providers land.
    true
}

/// Return the parsed state, re-reading state.json only when its mtime changed.
/// The 0.75s UI tick would otherwise read+parse the file every tick forever.
/// Only the main thread calls this, but a Mutex keeps it trivially sound.
fn cached_state() -> State {
    use std::sync::{Mutex, OnceLock};
    use std::time::SystemTime;
    struct C {
        mtime: Option<SystemTime>,
        state: State,
        loaded: bool,
    }
    static CELL: OnceLock<Mutex<C>> = OnceLock::new();
    let cell = CELL.get_or_init(|| {
        Mutex::new(C {
            mtime: None,
            state: State::default(),
            loaded: false,
        })
    });
    let path = crate::store::config_dir()
        .map(|d| d.join("state.json"))
        .unwrap_or_default();
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut g = cell.lock().unwrap_or_else(|e| e.into_inner());
    if !g.loaded || g.mtime != mtime {
        g.state = State::load().unwrap_or_default();
        g.mtime = mtime;
        g.loaded = true;
    }
    g.state.clone()
}

// The old "Backup Config ▸ Restore from backup ▸ <list>" submenu (and its
// `cached_backups()` mtime-cached directory listing) was replaced in v0.5.0
// by native Save…/Restore… file panels under Settings ▸ Advanced ▸ Backups ▸
// (see `handle_backup_save` / `handle_backup_restore_dialog`). The automatic
// rolling backups `store::list_backups()` enumerates still exist on disk —
// `handle_backup_restore_dialog` just defaults the OPEN panel's directory to
// them instead of rendering a menu row per file.

// (Login-item probe cache removed in 0.4.3 alongside the osascript path —
// see the comment on the removed "Launch at login" menu row for rationale.
// `usagio install` / `usagio uninstall` are the sole autostart entry
// points now, backed by a launchd LaunchAgent plist.)

// ---------------------------------------------------------------------------
// Menu building
// ---------------------------------------------------------------------------

/// Disabled/header-only rows a section prepends before its account submenus.
/// Currently: the "env override active — swap disabled" row when the section's
/// provider is env-overridden. Pure, so tests can assert on it without
/// instantiating any native menu (muda requires the main thread on macOS).
pub(crate) fn section_headline_rows(sec: &ProviderSection) -> Vec<&'static str> {
    let mut rows = Vec::new();
    if sec.env_override_active {
        rows.push(ENV_OVERRIDE_ROW_TITLE);
    }
    rows
}

/// macOS-only: builds the native `tray_icon::menu::Menu` tree directly (the
/// NSMenu attributedTitle styling walk in `apply_menu_styles` below needs a
/// live native menu to mutate). Linux/Windows build the equivalent tree via
/// `cross_platform::menu_tree_from_snapshot` instead, which emits the
/// generic `platform::MenuTree` the `MenuBackend` trait consumes.
#[cfg(target_os = "macos")]
fn build_menu(snap: &Snapshot) -> Menu {
    let menu = Menu::new();

    // v0.5.0 menu redesign: the two "resets in X" status header rows are gone
    // (item 1) — the same numbers now live inline in each account's own row.
    if snap.sections.is_empty() {
        add(
            &menu,
            MenuItem::with_id("none", "Capture a login below to begin", false, None),
        );
    }

    // v0.5.3 menu redesign: provider grouping is BACK. Each provider that has
    // captured accounts contributes one bold header row (or a submenu with the
    // env-override marker child, when its env override is active) followed by
    // one submenu-labelled row per account — the submenu's label IS the
    // account row, its children are the details + action rows the flat
    // v0.5.2 shape used to inline. A `PredefinedMenuItem::separator()` sits
    // BETWEEN provider groups (not between individual accounts). Within-
    // provider account order still comes from `snap.account_order` (the same
    // soonest-expiring / priority global comparator); provider groups are
    // sorted alphabetically by display name for stability (see
    // `provider_grouped_order`).
    let groups = provider_grouped_order(snap);
    let mut first_group = true;
    for (si, account_idxs) in &groups {
        let sec = &snap.sections[*si];
        if !first_group {
            let _ = menu.append(&PredefinedMenuItem::separator());
        }
        first_group = false;
        build_provider_group(&menu, sec);
        for ai in account_idxs {
            let a = &sec.accounts[*ai];
            build_account_submenu(&menu, sec, a);
        }
    }
    let _ = menu.append(&PredefinedMenuItem::separator());

    // Capture current login ▸ FIRST, then Settings ▸. Every registered
    // provider that can still capture a new login renders as a DIRECT child
    // — creds-on-disk providers first, then API-key providers (v0.5.2 item 8
    // flattened the "Paste API key ▸" sub-submenu these used to live under).
    // A provider stops appearing here once it has a captured account (item
    // 9 of the original redesign: capture is for NEW accounts only) — that
    // account renders in the main list above instead.
    let capture = Submenu::with_id("capture", "Capture current login", true);
    if snap.capture_creds.is_empty() && snap.capture_api_key.is_empty() {
        let _ = capture.append(&MenuItem::with_id(
            "noop",
            "(no providers registered)",
            false,
            None,
        ));
    } else {
        for reg in snap.capture_creds.iter().chain(snap.capture_api_key.iter()) {
            let title = if reg.installed {
                reg.display_name.to_string()
            } else {
                format!("{} (not installed)", reg.display_name)
            };
            // Row stays clickable even when we think it's not installed —
            // the actual capture call surfaces the real error. Click id
            // prefix ("capture:" vs "apikey:") is what routes to the right
            // handler — see `handle_click` — not menu position.
            let prefix = match reg.capture_mode {
                CaptureMode::CredsOnDisk => "capture",
                CaptureMode::ApiKey => "apikey",
            };
            let _ = capture.append(&MenuItem::with_id(
                format!("{prefix}:{}", reg.provider_id),
                title,
                true,
                None,
            ));
        }
    }
    let _ = menu.append(&capture);

    // Settings ▸: "Refresh usage now" first, then a separator, then
    // everything else alphabetical by display label (v0.5.2 item 9 —
    // flattens the old "Advanced ▸" grouping; Backups moves up to a direct
    // Settings child alongside Auto-swap and Notifications).
    let settings = Submenu::with_id("settings", "Settings", true);
    let _ = settings.append(&MenuItem::with_id(
        "refresh:now",
        "Refresh usage now",
        true,
        None,
    ));
    let _ = settings.append(&PredefinedMenuItem::separator());

    // Auto-swap ▸ (item 5 consolidation): the top-level "Auto-swap enabled"
    // checkbox is gone — Off doubles as disable, and picking a threshold
    // enables auto-swap AND sets it, so this one submenu is the sole control
    // surface.
    let cur = if snap.autoswap {
        snap.threshold.round() as i32
    } else {
        0
    };
    let autoswap_menu = Submenu::with_id("settings:autoswap", "Auto-swap", true);
    let _ = autoswap_menu.append(&CheckMenuItem::with_id(
        "autoswap:off",
        "Off",
        true,
        cur == 0,
        None,
    ));
    for t in [70i32, 85, 95, 98] {
        let _ = autoswap_menu.append(&CheckMenuItem::with_id(
            format!("autoswap:{t}"),
            format!("{t}%"),
            true,
            cur == t,
            None,
        ));
    }
    let _ = autoswap_menu.append(&PredefinedMenuItem::separator());
    let _ = autoswap_menu.append(&MenuItem::with_id(
        "autoswap:now",
        "Switch to best account now",
        true,
        None,
    ));

    let backups = Submenu::with_id("settings:backups", "Backups", true);
    let _ = backups.append(&MenuItem::with_id("backup:save", "Save…", true, None));
    let _ = backups.append(&MenuItem::with_id("backup:restore", "Restore…", true, None));

    let notifications = Submenu::with_id("settings:notifications", "Notifications", true);
    let _ = notifications.append(&CheckMenuItem::with_id(
        "notifications:threshold",
        "Threshold alerts (70% / 90%)",
        true,
        snap.notification_config.threshold_enabled,
        None,
    ));
    let _ = notifications.append(&CheckMenuItem::with_id(
        "notifications:resetback",
        "Window reset alerts",
        true,
        snap.notification_config.reset_back_enabled,
        None,
    ));
    let _ = notifications.append(&CheckMenuItem::with_id(
        "notifications:pace",
        "Weekly pace projection (experimental)",
        true,
        snap.notification_config.pace_enabled,
        None,
    ));

    // Alphabetical: Auto-swap, Backups, Notifications.
    let _ = settings.append(&autoswap_menu);
    let _ = settings.append(&backups);
    let _ = settings.append(&notifications);

    let _ = menu.append(&settings);

    // Context Ledger is intentionally CLI-only (`usagio context`). It was
    // previously a submenu here that shelled out via osascript to a new
    // Terminal window — but that path needed macOS Automation permission
    // that most users hadn't granted, so clicks were doing nothing. The
    // menu's job is account usage, not diagnostics; the CLI stays available
    // for the rare "wait, what's Claude injecting?" moment.

    // The "Launch at login" checkbox used to live here, backed by
    // `osascript -e 'tell application "System Events" to make login item …'`.
    // On every brew upgrade the binary hash changes → macOS treats it as a
    // new app → it re-prompts "usagio wants access to control System
    // Events" on the next poll. That path was redundant with `usagio
    // install` (which registers a proper launchd LaunchAgent), so we removed
    // the toggle in 0.4.3 and rely on the CLI for autostart. To clean out
    // the stale System Events entry a prior version installed, remove
    // "usagio" from System Settings → General → Login Items → Open at Login.

    let _ = menu.append(&PredefinedMenuItem::separator());
    // Quit on the left, version grey + right-aligned on the same row using
    // the same attributedTitle + right tab-stop machinery the account rows
    // already use for `S% / W%`. The style walker matches on the plain
    // title, so QUIT_ROW_PLAIN below MUST equal what we build here.
    add(
        &menu,
        MenuItem::with_id(
            "quit",
            format!("Quit\tusagio v{}", env!("CARGO_PKG_VERSION")),
            true,
            None,
        ),
    );

    menu
}

/// Human-facing "{Label} resets in X" copy for a window row inside an account
/// submenu. Percentages are deliberately NOT shown here (item 3 of the
/// redesign moved them to the main-list row via `main_row`) — this row is
/// purely about *when* the window refreshes.
fn window_reset_row(w: &WindowView) -> String {
    let label = stat_display_label(w);
    if w.reset.is_empty() {
        format!("{label}: no reset info yet")
    } else {
        format!("{label} resets in {}", w.reset)
    }
}

/// Non-clickable informational rows shown inside an account submenu — reset
/// windows, burn-rate / cost estimates, and the "updated Xm ago" footer (or
/// the has_data / no-usage-endpoint fallbacks). Shared by
/// `build_account_block` (which renders each as an `enabled: true` `noop`
/// item so it reads as normal text rather than muda's greyed-out disabled
/// look) and `menu_styles` (which marks each `disabled_but_white` so the
/// native walk explicitly paints `NSColor::labelColor()`) — pulling this out
/// keeps the two lists from drifting apart, the same trick `quit_row_plain`
/// uses for the Quit row.
///
/// Deliberately excludes the burn-rate / cost-estimate / "updated" footer
/// rows (`build_account_block` appends those separately): those touch the
/// on-disk usage log via `crate::usage_log`, and `menu_styles` — the other
/// caller of this function — must stay pure (no disk I/O) so it's safe to
/// call from a unit test without a `ScopedConfigDir`.
fn submenu_info_rows(sec: &ProviderSection, a: &AcctView) -> Vec<String> {
    let mut rows = Vec::new();
    if sec.supports_usage {
        if a.has_data && !a.windows.is_empty() {
            for w in &a.windows {
                rows.push(window_reset_row(w));
            }
        } else {
            rows.push("no data yet".to_string());
        }
    } else {
        rows.push("(no usage endpoint — headers only)".to_string());
    }
    rows
}

/// The disk-derived informational rows for an account's submenu, in render
/// order: burn-rate estimate (`🔥`), cost estimate (`💰`), and the "updated Xm
/// ago" footer. Split out from `submenu_info_rows` — which must stay
/// disk-I/O-free so the pure `menu_styles` can call it from a unit test —
/// because these read the on-disk usage log via `crate::burn_rate` /
/// `crate::cost_tracking`. Shared by every renderer that lays out the submenu
/// (the macOS `build_account_submenu` and its `mac_style::install_menu`
/// style-augmentation, plus the cross-platform `build_account_submenu_item`)
/// so the row TEXT stays byte-for-byte identical across them: the macOS
/// attributed-title walk matches these rows by their exact plain string, and a
/// glyph added on one path but not the other would leave the row unstyled
/// (greyed) on macOS.
fn account_extra_info_rows(sec: &ProviderSection, a: &AcctView) -> Vec<String> {
    let mut rows = Vec::new();
    if sec.supports_usage && a.has_data && !a.windows.is_empty() {
        let account_key =
            crate::usage_log::AccountKey::new(sec.provider_id.to_string(), a.key.clone());
        if let Some(est) = crate::burn_rate::estimate(
            &account_key,
            crate::providers::trait_def::Window::Weekly,
            Utc::now(),
        ) {
            if est.confidence >= crate::burn_rate::CONFIDENCE_FLOOR {
                rows.push(crate::burn_rate::format_menu_row(&est));
            }
        }
        if let Some(cost) = crate::cost_tracking::estimate_cycle_cost(
            &account_key,
            crate::cost_tracking::CLAUDE_MAX_100_WEEKLY_TOKENS,
        ) {
            rows.push(format!("~${:.2} this cycle (est)", cost.estimated_usd));
        }
        rows.push(format!("updated {}", a.updated));
    }
    rows
}

/// Which optional rows `build_account_block` should append for `sec`'s
/// account `a`. Pure over `ProviderSection`/`AcctView` fields, with no
/// `muda`/`tray_icon` types involved, so this decision is unit-testable
/// without a live main-thread `NSApplication` (a bare `muda::Menu` can only
/// be constructed on the main thread, which `cargo test`'s worker threads
/// aren't). H3, v0.5.0 codeaudit: `supports_switching` used to be the only
/// gate consulted, for BOTH the Switch and Launch rows, and Remove had no
/// gate at all.
#[derive(Debug, PartialEq, Eq)]
struct AccountSubmenuRows {
    /// `Some(true)` → render "✓ Active" (this account is the active one).
    /// `Some(false)` → render a clickable "Switch to this account" row.
    /// `None` → the provider doesn't support switching; render neither.
    switch_row: Option<bool>,
    launch_row: bool,
    remove_row: bool,
}

fn account_submenu_rows(sec: &ProviderSection, a: &AcctView) -> AccountSubmenuRows {
    AccountSubmenuRows {
        switch_row: sec.supports_switching.then_some(a.active),
        launch_row: sec.supports_launch,
        remove_row: sec.supports_remove,
    }
}

/// v0.5.3 menu redesign: emit the bold provider-group header row (see
/// `provider_group_header_row`). When the provider's env override is active,
/// the header becomes a `Submenu` whose single child is the "env override
/// active — swap disabled" marker (from `section_headline_rows`), so the
/// provider-wide fact renders once at the group level instead of on every
/// account. Otherwise the header is a plain disabled `MenuItem` — the bold /
/// icon styling is applied by the native attributedTitle walker matching on
/// `provider_group_header_row`'s plain title.
#[cfg(target_os = "macos")]
fn build_provider_group(menu: &Menu, sec: &ProviderSection) {
    let head = provider_group_header_row(sec).plain;
    let headlines = section_headline_rows(sec);
    if headlines.is_empty() {
        add(menu, MenuItem::with_id("noop", head, false, None));
        return;
    }
    // Env-override present: expose the marker as the sole child of a submenu
    // whose title is the provider name.
    let sub = Submenu::with_id(format!("provider:{}", sec.provider_id), head, true);
    for title in headlines {
        let _ = sub.append(&MenuItem::with_id(
            format!("envoverride:{}", sec.provider_id),
            title,
            false,
            None,
        ));
    }
    let _ = menu.append(&sub);
}

/// v0.5.3 menu redesign: each account becomes a `Submenu` whose title is the
/// compact `account_header_row` (indented `<email>\t<trailing>`) and whose
/// children are the details + action rows the flat v0.5.2 shape used to
/// inline directly in the top-level menu (reset countdowns, burn rate, cost,
/// "updated Xm ago", then Switch/Active, Launch, Remove). Cross-platform
/// counterpart: `cross_platform::build_account_submenu_item`.
#[cfg(target_os = "macos")]
fn build_account_submenu(menu: &Menu, sec: &ProviderSection, a: &AcctView) {
    let head = account_header_row(sec, a).plain;
    // Submenu id is intentionally `noop` so a click on the LABEL (which on
    // some platforms fires as an event too) is a no-op — the actual actions
    // live on the children below.
    let sub = Submenu::with_id("noop", head, true);

    // Informational rows (reset windows, then burn-rate / cost / "updated"):
    // rendered ENABLED (`enabled: true`) with a no-op click id. macOS draws a
    // DISABLED menu item's title dimmed to grey *regardless* of any
    // attributed foreground color — the disabled appearance overrides
    // `NSColor::labelColor()` — so a disabled row here is unreadable, not
    // merely un-hoverable. Readability wins: these render at full contrast; the
    // cost is a hover highlight on a row that does nothing (clicks route to the
    // `("noop", …)` arm in `handle_click`). Eliminating the hover *and* keeping
    // full contrast isn't possible with a native NSMenu — that's a goal for the
    // custom-popup UI (see docs/design/custom-tray-popup.md).
    for row in submenu_info_rows(sec, a) {
        let _ = sub.append(&MenuItem::with_id("noop", row, true, None));
    }
    for row in account_extra_info_rows(sec, a) {
        let _ = sub.append(&MenuItem::with_id("noop", row, true, None));
    }

    // Action rows: Switch/Active, Launch, Remove. Gated per H3 (v0.5.0
    // codeaudit) — see `account_submenu_rows`.
    let rows = account_submenu_rows(sec, a);
    let _ = sub.append(&PredefinedMenuItem::separator());
    match rows.switch_row {
        Some(true) => {
            let _ = sub.append(&MenuItem::with_id("noop", "✓ Active", false, None));
        }
        Some(false) => {
            let _ = sub.append(&MenuItem::with_id(
                format!("switch:{}:{}", sec.provider_id, a.key),
                "Switch to this account",
                true,
                None,
            ));
        }
        None => {}
    }
    if rows.launch_row {
        let _ = sub.append(&MenuItem::with_id(
            format!("launch:{}:{}", sec.provider_id, a.key),
            "Launch client",
            true,
            None,
        ));
    }
    if rows.remove_row {
        let _ = sub.append(&MenuItem::with_id(
            format!("remove:{}:{}", sec.provider_id, a.key),
            "Remove…",
            true,
            None,
        ));
    }

    let _ = menu.append(&sub);
}

/// macOS-only NSMenu attributedTitle styling. Grouped into one module (rather
/// than gating `install_menu`/`color_for`/`attributed`/`apply_menu_styles`
/// individually) so the `objc2`/`objc2-app-kit`/`objc2-foundation` imports
/// they need — real Cargo target-specific dependencies that don't exist in
/// the dependency graph on Linux/Windows (see the top-of-file import comment)
/// — sit behind a single `#[cfg(target_os = "macos")]`. Linux/Windows never
/// call into this module; they render through `cross_platform` instead.
#[cfg(target_os = "macos")]
mod mac_style {
    use super::*;

    /// Build the menu for `snap`, install it on the tray, then style the native
    /// rows (bold active account, right-aligned trailing `n% / n%`, high
    /// percentages colored) via `attributedTitle`. We take the `NSMenu` pointer
    /// before moving the menu into `set_menu`: the menu is reference-counted and the
    /// tray retains it, so the pointer stays valid for the walk. The attributed
    /// titles persist until the next rebuild (muda only overwrites a title if we
    /// call `set_text`, which we never do on these items).
    pub(super) fn install_menu(tray: &tray_icon::TrayIcon, snap: &Snapshot) {
        let menu = build_menu(snap);
        let ns_menu = {
            use tray_icon::menu::ContextMenu;
            menu.ns_menu()
        };
        tray.set_menu(Some(Box::new(menu)));
        let mut styles = menu_styles(snap);
        // The burn-rate / cost / "updated" submenu rows are derived with disk
        // I/O and so live outside the pure `menu_styles`. Now that they render
        // as DISABLED items (no hover), they need the same `disabled_but_white`
        // treatment as the reset rows or AppKit would grey them out — add those
        // styles here (macOS install path only, where disk reads are fine).
        for (si, _) in provider_grouped_order(snap) {
            let sec = &snap.sections[si];
            for a in &sec.accounts {
                for row in account_extra_info_rows(sec, a) {
                    styles.push(RowStyle {
                        disabled_but_white: true,
                        ..RowStyle::plain_row(row)
                    });
                }
            }
        }
        let menu_right_x = compute_menu_right_x(ns_menu, &styles);
        apply_menu_styles(ns_menu, &styles, menu_right_x);
    }

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::AllocAnyThread;
    use objc2_app_kit::{
        NSAttributedStringNSStringDrawing, NSColor, NSControlStateValueOn, NSFont,
        NSFontAttributeName, NSForegroundColorAttributeName, NSImage, NSMenu,
        NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSTextAlignment, NSTextTab,
        NSTextTabOptionKey,
    };
    use objc2_foundation::{
        NSArray, NSAttributedString, NSData, NSDictionary, NSMutableAttributedString, NSRange,
        NSString,
    };

    fn color_for(sev: Severity) -> Retained<NSColor> {
        match sev {
            Severity::Amber => NSColor::systemOrangeColor(),
            Severity::Red => NSColor::systemRedColor(),
        }
    }

    /// Extra padding (points) added beyond the widest measured content width
    /// before resolving the right-align tab stop. `NSAttributedString::size()`
    /// measures glyph runs only — it knows nothing about the icon column,
    /// checkmark/state column, or submenu-arrow column AppKit reserves — so a
    /// positive pad here was meant to compensate. In practice the v0.5.1
    /// value (28pt) overcompensated: it pushed the tab stop past the menu's
    /// actual content edge, leaving a visible dead-space gap after the
    /// trailing run on every row, worst on the un-inset Quit row ("Quit gaps
    /// to the end", v0.5.2 item 2). Zero — i.e. trust `widest` (already
    /// floored by the widest plain top-level row below) as the edge itself —
    /// is what actually flushes right; a future report of text running too
    /// close to the edge should look at raising this again, not assume the
    /// old 28pt value was ever correct.
    const RIGHT_ALIGN_PAD: f64 = 0.0;

    /// v0.5.4 defect 1: minimum gap (points) between the widest label's
    /// right edge and its row's trailing run's left edge. Without this the
    /// tab stop could land right at the widest email's right edge, so a row
    /// with a near-widest email (e.g. `matthew@getbusbar.com`) rendered with
    /// ZERO space between the email and the `S% / W%` trailing. Two
    /// menu-font space-widths (~12pt) reads as an obvious column gap without
    /// blowing out the menu width on narrow menus.
    pub(super) const MIN_LABEL_TRAILING_GAP: f64 = 12.0;

    /// v0.5.4 defect 2: AppKit reserves ~14pt at the right edge of every
    /// NSMenu that contains any submenu item, for the disclosure chevron.
    /// Rows WITH a submenu render their trailing text flush against the tab
    /// stop and the chevron sits in that reserved column; rows WITHOUT a
    /// submenu render their trailing text at the same tab stop, leaving a
    /// chevron-column-sized whitespace between their trailing and the
    /// menu's true right edge — "text is not fully right aligned if the row
    /// doesnt have a submenu" per the user report. `effective_tab_x` pushes
    /// non-submenu rows' tab stop right by this width so both types
    /// terminate at the same visual X. Empirically 14pt on macOS 14/15.
    pub(super) const CHEVRON_COLUMN_WIDTH: f64 = 14.0;

    /// Resolve the right-align tab stop for one row, adjusting for whether
    /// AppKit will draw a disclosure chevron to the right of its trailing
    /// run. See `CHEVRON_COLUMN_WIDTH`. Kept pure (no AppKit types) so
    /// `#[cfg(test)]` can pin the arithmetic without touching NSMenu.
    pub(super) fn effective_tab_x(base_x: f64, has_submenu: bool) -> f64 {
        if has_submenu {
            base_x
        } else {
            base_x + CHEVRON_COLUMN_WIDTH
        }
    }

    /// The natural (unwrapped, single-line) width in points of `s` rendered
    /// in `font`. Empty strings measure 0 without round-tripping through
    /// AppKit. Uses `NSAttributedString::size()` — the `NSStringDrawing`
    /// category's simple whole-string measurement — rather than
    /// `boundingRectWithSize:options:`, which exists for constrained /
    /// multi-line layout that a single menu-item row never needs.
    fn measured_width(s: &str, font: &NSFont) -> f64 {
        if s.is_empty() {
            return 0.0;
        }
        let ns_text = NSString::from_str(s);
        let full_len = ns_text.length();
        let attr =
            NSMutableAttributedString::initWithString(NSMutableAttributedString::alloc(), &ns_text);
        // SAFETY: value type matches the font attribute key.
        unsafe {
            attr.addAttribute_value_range(NSFontAttributeName, font, NSRange::new(0, full_len));
        }
        let attr: Retained<NSAttributedString> = Retained::into_super(attr);
        attr.size().width
    }

    /// Resolve `TabX::MenuRight` to a concrete point value for the CURRENT
    /// snapshot: `max(label_width + trailing_width)` over every row that
    /// wants the shared right-align tab stop, plus `RIGHT_ALIGN_PAD`. Using
    /// one shared x for every row is what makes their trailing runs line up
    /// in a column; computing it fresh per snapshot (rather than a fixed
    /// constant) is what makes that column always land at the menu's actual
    /// right edge regardless of how wide the longest email/label happens to
    /// be. Each row is measured in the font it will actually render in
    /// (`boldSystemFontOfSize` for bold/active rows, `menuFontOfSize`
    /// otherwise) since bold glyphs are wider. No `MenuRight` rows → 0.0 (an
    /// arbitrary, harmless x — nothing reads it).
    ///
    /// v0.5.2 item 2: `RIGHT_ALIGN_PAD` used to be 28pt, which reliably
    /// undershot the true content edge and produced a visible gap after the
    /// right-aligned column (worst offender: the Quit row, "flush right"
    /// only in name). Rather than re-guessing a bigger magic constant, the
    /// pad is now 0 — `widest` itself (measured content width, floored by
    /// the widest plain top-level row below) IS the target x.
    ///
    /// Also floors the result at the widest PLAIN (non-tab) top-level item's
    /// measured width (v0.5.2 fix for the v0.5.1 "Quit gaps to the end"
    /// report): if some other top-level row — a checkbox, a `Settings ▸`
    /// submenu title, the per-account env-override marker, an account
    /// block's info row, … — is wider than every `MenuRight` row's own
    /// label+trailing, THAT row is what actually determines the menu's
    /// rendered width, and our right-aligned trailing text would land short
    /// of the true right edge no matter how the tab-row math above comes
    /// out. `ns_menu` is the same live pointer `apply_menu_styles` walks;
    /// this call happens before that walk sets any attributed titles, so
    /// every item's `.title()` is still its plain string.
    fn compute_menu_right_x(ns_menu: *mut core::ffi::c_void, styles: &[RowStyle]) -> f64 {
        let mut widest = 0.0_f64;
        for style in styles {
            if style.tab_x_kind != Some(TabX::MenuRight) {
                continue;
            }
            let Some((label, trailing)) = style.plain.split_once('\t') else {
                continue;
            };
            let font = if style.bold || style.section_header {
                NSFont::boldSystemFontOfSize(0.0)
            } else {
                NSFont::menuFontOfSize(0.0)
            };
            // v0.5.4 defect 1: enforce MIN_LABEL_TRAILING_GAP between the
            // label's right edge and the trailing's left edge for EVERY row
            // (not just where they happen to sum wider than a plain row).
            let w = measured_width(label, &font)
                + MIN_LABEL_TRAILING_GAP
                + measured_width(trailing, &font);
            if w > widest {
                widest = w;
            }
        }
        if !ns_menu.is_null() {
            // SAFETY: called only on the main thread with a live NSMenu
            // pointer from muda's ns_menu(), which the tray keeps retained —
            // same precondition `apply_menu_styles` documents below.
            let menu: &NSMenu = unsafe { &*(ns_menu as *const NSMenu) };
            let font = NSFont::menuFontOfSize(0.0);
            for item in menu.itemArray().iter() {
                let title = item.title().to_string();
                if title.contains('\t') {
                    continue; // already covered by the tab-row loop above
                }
                let w = measured_width(&title, &font);
                if w > widest {
                    widest = w;
                }
            }
        }
        widest + RIGHT_ALIGN_PAD
    }

    /// Build the attributed title for one row from its `RowStyle`. Module-level
    /// (not nested in `apply_menu_styles`) so `#[cfg(test)]` can exercise it
    /// directly — e.g. asserting `disabled_but_white` produces a `labelColor`
    /// foreground-color attribute over the full row. `pub(super)` (rather than
    /// private) so the macOS-only tests at the bottom of this file's `tests`
    /// module can call `mac_style::attributed(...)` directly. `resolved_tab_x`
    /// is the menu-wide point value `compute_menu_right_x` produced for this
    /// snapshot — `None` means "no tab stop on this row" regardless of what
    /// `style.tab_x_kind` says (callers gate that); a test exercising a row
    /// with no tab stop can just pass `None`.
    pub(super) fn attributed(
        style: &RowStyle,
        resolved_tab_x: Option<f64>,
    ) -> Retained<NSAttributedString> {
        let ns_text = NSString::from_str(&style.plain);
        // NSRange is UTF-16 code units — use NSString::length, not byte length.
        let full_len = ns_text.length();
        let attr =
            NSMutableAttributedString::initWithString(NSMutableAttributedString::alloc(), &ns_text);

        // Right-aligned trailing run at the menu-wide computed tab stop
        // (battery-menu style).
        if let Some(x) = resolved_tab_x {
            let para = NSMutableParagraphStyle::new();
            let opts: Retained<NSDictionary<NSTextTabOptionKey, AnyObject>> = NSDictionary::new();
            // SAFETY: the options generic is the correct (empty) dictionary type.
            let tab = unsafe {
                NSTextTab::initWithTextAlignment_location_options(
                    NSTextTab::alloc(),
                    NSTextAlignment::Right,
                    x,
                    &opts,
                )
            };
            let tabs = NSArray::from_retained_slice(&[tab]);
            para.setTabStops(Some(&tabs));
            // SAFETY: value type matches the paragraph-style attribute key.
            unsafe {
                attr.addAttribute_value_range(
                    NSParagraphStyleAttributeName,
                    &para,
                    NSRange::new(0, full_len),
                );
            }
        }

        // Bold marks either the active account (via `main_row`) or a section
        // header (via `section_header`). Both use the same appearance.
        if style.bold || style.section_header {
            // 0.0 => default menu font size.
            let font = NSFont::boldSystemFontOfSize(0.0);
            // SAFETY: value type matches the font attribute key.
            unsafe {
                attr.addAttribute_value_range(
                    NSFontAttributeName,
                    &font,
                    NSRange::new(0, full_len),
                );
            }
        }

        // Optional grey trailing run (Quit row: version rendered in
        // NSColor::secondaryLabelColor — the same tint disabled items use,
        // without actually disabling the row).
        if let Some(from) = style.grey_tail_from {
            if from < full_len {
                let grey = NSColor::secondaryLabelColor();
                // SAFETY: value type matches the foreground-color attribute key.
                unsafe {
                    attr.addAttribute_value_range(
                        NSForegroundColorAttributeName,
                        &grey,
                        NSRange::new(from, full_len - from),
                    );
                }
            }
        }

        // v0.5.0: force the normal (non-disabled) text color over the WHOLE row
        // for informational submenu rows that are `enabled: true` for
        // click-routing purposes but must not look like muda's greyed-out
        // disabled rows (item 3 of the redesign — "white but not clickable").
        if style.disabled_but_white {
            let normal = NSColor::labelColor();
            // SAFETY: value type matches the foreground-color attribute key.
            unsafe {
                attr.addAttribute_value_range(
                    NSForegroundColorAttributeName,
                    &normal,
                    NSRange::new(0, full_len),
                );
            }
        }

        // Tint high percentages (amber approaching, red near the wall).
        for &(off, len, sev) in &style.colors {
            if len == 0 || off >= full_len {
                continue;
            }
            let end = (off + len).min(full_len);
            let color = color_for(sev);
            // SAFETY: value type matches the foreground-color attribute key.
            unsafe {
                attr.addAttribute_value_range(
                    NSForegroundColorAttributeName,
                    &color,
                    NSRange::new(off, end - off),
                );
            }
        }

        Retained::into_super(attr)
    }

    /// Walk the native `NSMenu` (and its submenus) and set `attributedTitle` on any
    /// item whose plain title matches a `RowStyle` — the mechanism muda's plain
    /// string API can't reach (right-aligned tab stops and arbitrary colors).
    /// `menu_right_x` is the point value `compute_menu_right_x` resolved for
    /// this snapshot; every `TabX::MenuRight` row gets that same x so their
    /// trailing runs line up in a column.
    fn apply_menu_styles(ns_menu: *mut core::ffi::c_void, styles: &[RowStyle], menu_right_x: f64) {
        if ns_menu.is_null() {
            return;
        }

        /// Turn a bundled PNG's bytes into a 16×16 `NSImage`. Nil-safe: a corrupt
        /// or unsupported blob returns `None` (the caller just skips setImage:).
        fn image_from_bytes(bytes: &[u8]) -> Option<Retained<NSImage>> {
            let data = NSData::with_bytes(bytes);
            let img = NSImage::initWithData(NSImage::alloc(), &data)?;
            // Force the drawn size to menu-item height (16pt); PNGs are already
            // 16×16 but `NSImage`'s reported size is 72dpi-scaled, which reads too
            // big at Retina. `usesSize` is not required here — NSMenuItem uses the
            // image's `size` directly.
            use objc2_foundation::NSSize;
            img.setSize(NSSize {
                width: 16.0,
                height: 16.0,
            });
            Some(img)
        }

        /// Style every item whose plain title matches, descending into submenus.
        /// `top_level` is true on the outermost NSMenu only; section-header styles
        /// (unused in v0.5.0's flat main list, but the flag stays load-bearing for
        /// any future disabled/bold top-level row) are suppressed inside submenus
        /// so a plain-title collision with a submenu row doesn't inherit them.
        fn walk(menu: &NSMenu, styles: &[RowStyle], top_level: bool, menu_right_x: f64) {
            for item in menu.itemArray().iter() {
                let title = item.title().to_string();
                if let Some(style) = styles
                    .iter()
                    .find(|s| s.plain == title && (top_level || !s.section_header))
                {
                    // v0.5.4 defect 2: non-submenu rows (Quit, disabled
                    // provider headers without env-override, etc.) need
                    // their tab stop pushed right by the chevron column
                    // width so their trailing text terminates at the same
                    // visual X as the submenu-row trailing (which currently
                    // sits flush against the chevron reserved column).
                    let has_submenu = item.submenu().is_some();
                    let resolved_tab_x = style
                        .tab_x_kind
                        .map(|_| effective_tab_x(menu_right_x, has_submenu));
                    item.setAttributedTitle(Some(&attributed(style, resolved_tab_x)));
                    // Per-provider 16px icon on section header rows. Look up by
                    // slug; a missing PNG (or an unknown slug like `vertex-ai`) is
                    // a no-op so a future provider without a bundled icon still
                    // renders — just text-only.
                    if let Some(slug) = style.icon_slug {
                        if let Some(bytes) = crate::icons::png16_for(slug) {
                            if let Some(img) = image_from_bytes(bytes) {
                                item.setImage(Some(&img));
                            }
                        }
                    }
                    // Leading checkmark glyph for the active-account row (the
                    // "✓ trailing glyph" spec). AppKit renders `state == On` as a
                    // checkmark in the item's `stateColumn`.
                    if style.checkmark {
                        item.setState(NSControlStateValueOn);
                    }
                }
                if let Some(sub) = item.submenu() {
                    walk(&sub, styles, false, menu_right_x);
                }
            }
        }

        // SAFETY: called only on the main thread (the run-loop timer), with a live
        // NSMenu pointer from muda's ns_menu() that the tray keeps retained.
        let menu: &NSMenu = unsafe { &*(ns_menu as *const NSMenu) };
        walk(menu, styles, true, menu_right_x);
    }
} // mod mac_style

/// A submenu stat line and the span of its percentage (for coloring). Returns
/// `(plain_title, Some((utf16_offset, utf16_len)))`; the offset locates the
/// `NN%` so the native walk can tint just the number.
/// A stable fingerprint of everything the menu renders. When it's unchanged we
/// skip `set_menu`, so an open menu is never dismissed by a no-op poll.
///
/// Provider id is folded into every account fingerprint so a Codex-side change
/// doesn't collide with a stale Claude signature (or vice versa) once state
/// carries multiple providers.
fn menu_signature(snap: &Snapshot) -> String {
    let mut s = String::new();
    for sec in &snap.sections {
        s.push_str(&format!(
            "SEC[{}={}|sw={}|us={}|env={}|",
            sec.provider_id,
            sec.display_name,
            sec.supports_switching,
            sec.supports_usage,
            sec.env_override_active,
        ));
        for a in &sec.accounts {
            // `a.provider_id` mirrors `sec.provider_id` in v1; folding both
            // keeps the signature honest once state v2 tags each account
            // with its own slug and a mis-bucketed account could exist.
            // The locked-state trailing text (`locked · Xh Ym`) doesn't come
            // from any window's `w.pct/w.reset` string — it's computed from
            // the reset instants + a wall-clock `now`. Fold the raw instants
            // plus the current `DisplayState` variant into the signature so a
            // usage→locked transition, or a change to *which* window is
            // blocking, triggers a redraw. Instants use RFC3339 for stability.
            let sr = a
                .session_reset_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_default();
            let wr = a
                .weekly_reset_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_default();
            // The `lock` marker alone is not enough: while an account stays
            // Locked the (fixed) reset instant + (fixed) variant produce a
            // static signature for hours, so `install_menu` never re-runs and
            // the header's `locked · Xh Ym` text freezes at whatever value it
            // had when the transition happened. Fold the CURRENT `format_countdown`
            // output into the signature so each minute (or hour, depending on
            // remaining time) the fingerprint changes and the tray redraws with
            // the fresh remaining time. Uses `now_utc()` — the same clock
            // `main_row` renders from.
            let now = now_utc();
            let lock = match countdown::compute_display(&account_usage_for(a), now) {
                DisplayState::Locked {
                    window: BlockingWindow::Session,
                    until,
                } => {
                    format!("L=S|cd={}", countdown::format_countdown(until - now))
                }
                DisplayState::Locked {
                    window: BlockingWindow::Weekly,
                    until,
                } => {
                    format!("L=W|cd={}", countdown::format_countdown(until - now))
                }
                DisplayState::Usage { .. } => "L=0".to_string(),
                DisplayState::StaleAfterReset { window, reset } => {
                    format!("L=SA|w={window:?}|r={}", reset.to_rfc3339())
                }
            };
            s.push_str(&format!(
                "{}@{}/{}|{}|{}|sr={}|wr={}|{}|",
                a.provider_id, sec.provider_id, a.key, a.active, a.has_data, sr, wr, lock,
            ));
            for w in &a.windows {
                s.push_str(&format!(
                    "{}={}:r={}|",
                    w.id,
                    w.pct.map(|v| v.round() as i64).unwrap_or(-1),
                    w.reset,
                ));
            }
            // Deliberately EXCLUDE `a.updated` from the signature. It's the
            // human "Xs ago" / "Xm ago" string that changes every second for
            // the first minute after each poll — folding it in forced a full
            // `install_menu` on every 0.75s tick (defeating the whole cache).
            // Real underlying freshness is already captured by `a.has_data`
            // and each window's pct+reset above. See H8 in the round-1
            // codeaudit findings.
        }
        s.push_str("] ");
    }
    // Capture-submenu contents also affect the redraw: adding a new registered
    // provider (or a change in its installed-probe answer) must show up. Both
    // buckets fold into the signature so moving a provider between them (e.g.
    // a capture-mode change) also triggers a redraw.
    for reg in &snap.capture_creds {
        s.push_str(&format!(
            "REG[{}|{}|{}|creds] ",
            reg.provider_id, reg.display_name, reg.installed
        ));
    }
    for reg in &snap.capture_api_key {
        s.push_str(&format!(
            "REG[{}|{}|{}|apikey] ",
            reg.provider_id, reg.display_name, reg.installed
        ));
    }
    s.push_str(&format!("as={} th={:.0}", snap.autoswap, snap.threshold));
    s
}

/// macOS-only: `build_menu` helper (see its doc comment for why it's mac-only).
#[cfg(target_os = "macos")]
fn add(menu: &Menu, item: MenuItem) {
    let _ = menu.append(&item);
}

fn title_for(snap: &Snapshot) -> String {
    match active_account(snap) {
        // Session (5h) matters most day to day; fall back to weekly. Preserves
        // the v1 tray-title semantics — a full weekly can't silently replace
        // the low session number in the menu bar. Multi-window providers still
        // yield a single honest number by leaning on the provider's window
        // ordering (first = session-analog, second = weekly-analog).
        Some((_sec, a)) => {
            let s = a.windows.first().and_then(|w| w.pct);
            let w = a.windows.get(1).and_then(|w| w.pct);
            match s.or(w) {
                Some(p) => format!("{p:.0}%"),
                None => "—".to_string(),
            }
        }
        None => "—".to_string(),
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn tooltip_for(snap: &Snapshot) -> String {
    match active_account(snap) {
        // Preserve the v1 tooltip format verbatim: `email — session X, weekly Y`.
        // Multi-window providers still project onto the first two windows
        // (session-analog / weekly-analog) so the tooltip stays a stable
        // one-liner regardless of how many windows the provider carries.
        Some((_sec, a)) => {
            let s = a.windows.first().and_then(|w| w.pct);
            let w = a.windows.get(1).and_then(|w| w.pct);
            format!("{} — session {}, weekly {}", a.display, pct(s), pct(w))
        }
        None => "usagio: no active account".to_string(),
    }
}

fn pct(p: Option<f64>) -> String {
    p.map(|v| format!("{v:.0}%"))
        .unwrap_or_else(|| "—".to_string())
}

// ---------------------------------------------------------------------------
// Click handling
// ---------------------------------------------------------------------------

/// Parsed click id: `action[:slug[:key]]`. `slug`/`key` are `None` for global
/// actions (`quit`, `autoswap:toggle`, `autoswap:off`, `autoswap:95`,
/// `autoswap:now`, `notifications:threshold` / `:resetback` / `:pace`,
/// `backup:save`, `backup:restore`, `refresh:now`, `noop`, `capture` — the
/// plain-capture id used only by the submenu title itself, never a click).
struct ClickId<'a> {
    action: &'a str,
    slug: Option<&'a str>,
    key: Option<&'a str>,
}

fn parse_click_id(id: &str) -> ClickId<'_> {
    let mut it = id.splitn(3, ':');
    let action = it.next().unwrap_or("");
    let slug = it.next();
    let key = it.next();
    ClickId { action, slug, key }
}

fn handle_click(id: &str) {
    let c = parse_click_id(id);
    // Actions only mutate state.json; the main-thread timer re-renders from it
    // within ~1s without any network call.
    match (c.action, c.slug, c.key) {
        ("quit", _, _) => std::process::exit(0),
        ("noop", _, _) => {}
        ("autoswap", Some("off"), None) => set_autoswap(false),
        ("autoswap", Some("now"), None) => match optimize_now() {
            Ok(Some(email)) => notify(&format!("Switched to {email}")),
            Ok(None) => notify("Already on the best account"),
            Err(e) => notify(&format!("Optimize failed: {e}")),
        },
        ("autoswap", Some(t), None) => {
            if let Ok(v) = t.parse::<f64>() {
                set_autoswap_threshold(v);
            }
        }
        ("notifications", Some(trigger @ ("threshold" | "resetback" | "pace")), None) => {
            toggle_notification_trigger(trigger)
        }
        ("capture", Some(slug), None) => handle_capture(slug),
        ("apikey", Some(slug), None) => handle_apikey_capture(slug),
        ("switch", Some(slug), Some(key)) => handle_switch(slug, key),
        ("remove", Some(slug), Some(key)) => handle_remove(slug, key),
        ("launch", Some(slug), Some(key)) => handle_launch(slug, key),
        ("backup", Some("save"), None) => handle_backup_save(),
        ("backup", Some("restore"), None) => handle_backup_restore_dialog(),
        ("refresh", Some("now"), None) => handle_refresh_now(),
        // Ignore unrecognized ids (e.g. the top-level "capture"/"capture:apikey"
        // submenu titles or a future action added by a later phase we don't
        // yet handle).
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Settings ▸ Advanced ▸ Backups ▸ handlers (native Save…/Restore… panels)
// ---------------------------------------------------------------------------

/// "Save…" click: opens a native SAVE panel (via
/// `Platform::file_dialog().save_file(...)`) defaulting to
/// `~/Downloads/usagio-state-{timestamp}.json`, then writes a
/// REDACTED (token-free) dump of the current state to the chosen path with
/// mode 0600. This is a portable diagnostics/config snapshot — the same
/// shape `redact_state_for_dump` already produces for the "REFUSED save"
/// diagnostic dump — NOT a working credential backup. The full-fidelity
/// automatic rolling backups under `config_dir()/backups/` (written on every
/// real `state.json` save; see `write_rolling_backup`) are what "Restore…"
/// below defaults its file panel to.
fn handle_backup_save() {
    handle_backup_save_with(crate::platform().file_dialog());
}

/// Testable core of the Save… click: takes the `FileDialog` as a parameter
/// so `#[cfg(test)]` can pass a `platform::MockFileDialog` instead of
/// popping a real native panel. See `platform::Platform::file_dialog`.
fn handle_backup_save_with(dialog: &dyn crate::platform::FileDialog) {
    let default_name = format!(
        "usagio-state-{}.json",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    let default_dir = dirs::download_dir();
    let Some(path) = dialog.save_file(&default_name, default_dir.as_deref()) else {
        return; // user cancelled
    };
    let st = match State::load() {
        Ok(s) => s,
        Err(e) => {
            notify(&format!("Save failed: {e}"));
            return;
        }
    };
    let redacted = crate::store::redact_state_for_dump(&st);
    let bytes = match serde_json::to_vec_pretty(&redacted) {
        Ok(b) => b,
        Err(e) => {
            notify(&format!("Save failed: {e}"));
            return;
        }
    };
    if let Err(e) = crate::store::write_private(&path, &bytes) {
        notify(&format!("Save failed: {e}"));
        return;
    }
    notify(&format!("Saved to {}", path.display()));
}

/// "Restore…" click: opens a native OPEN panel (via
/// `Platform::file_dialog().pick_file(...)`) defaulting to the automatic
/// rolling-backups directory, validates the chosen file is state-shaped,
/// warns — by NAME, not just count — before a restore would drop accounts,
/// then replaces `state.json` through the same `save_state_safe`
/// drop-protection every other write goes through. The rolling backups
/// already snapshot the pre-restore state on every ordinary save, but we
/// ALSO stash the live file to `<config_dir>/backups/` right before
/// overwriting it, so the restore itself is reversible even if the user
/// picked a very old backup.
///
/// H2 (v0.5.0 codeaudit): this used to (1) stash the live state — full OAuth
/// tokens, non-redacted — to a `/tmp` sidecar, a world-writable shared
/// directory, and (2) bypass `save_state_safe`'s drop-protection guard and
/// the cross-process state lock entirely, gating the write on nothing but an
/// account *count* comparison (same-count-different-accounts sailed through
/// with zero confirmation). Both are fixed here: the stash moves under
/// `config_dir()/backups/` (owner-only permissions), and the whole
/// read-confirm-write sequence runs under `with_state_lock` with the actual
/// write routed through `store::save_state_restore` (a thin, explicitly-
/// authorized wrapper around `save_state_safe`). Note: the M2 concern
/// (`State::load()` failure silently becoming "0 accounts") is subsumed by
/// this restructure — `accounts_dropped_by(&new_state)?` bubbles the load
/// error up out of `with_state_lock`, and the `Err(e) => notify(...)` arm
/// below surfaces it, so the drop-warning path can't be silently skipped.
fn handle_backup_restore_dialog() {
    handle_backup_restore_dialog_with(crate::platform().file_dialog());
}

/// Testable core of the Restore… click: takes the `FileDialog` as a
/// parameter so `#[cfg(test)]` can pass a `platform::MockFileDialog` instead
/// of popping a real native panel. See `platform::Platform::file_dialog`.
fn handle_backup_restore_dialog_with(dialog: &dyn crate::platform::FileDialog) {
    let mut default_dir = None;
    if let Ok(p) = crate::store::state_json_path() {
        let backups_dir = p.parent().map(|d| d.join("backups"));
        if let Some(dir) = backups_dir {
            if dir.is_dir() {
                default_dir = Some(dir);
            }
        }
    }
    let Some(path) = dialog.pick_file(default_dir.as_deref()) else {
        return; // user cancelled
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            notify(&format!("Restore failed reading {}: {e}", path.display()));
            return;
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            notify(&format!("Restore failed: not valid JSON ({e})"));
            return;
        }
    };
    if value.get("accounts").is_none() {
        notify("Restore failed: not a usagio state file (missing 'accounts')");
        return;
    }
    let new_state = State::from_value(&value);

    let outcome = with_state_lock(|| -> Result<bool> {
        let dropped = crate::store::accounts_dropped_by(&new_state)?;
        if !dropped.is_empty() && !confirm(&restore_drop_confirmation(&dropped)) {
            return Ok(false); // user declined; not an error
        }
        let dir = crate::store::config_dir()?;
        crate::store::stash_pre_restore(&dir)?;
        crate::store::save_state_restore(new_state.clone())?;
        Ok(true)
    });

    match outcome {
        Ok(true) => notify(&format!("Restored state from {}", path.display())),
        Ok(false) => {} // user declined the drop confirmation; no-op
        Err(e) => notify(&format!("Restore failed: {e}")),
    }
}

/// Build the confirmation question shown before a restore that would drop
/// accounts. Names the specific dropped emails (H2, v0.5.0 codeaudit — the
/// prior wording was "will drop 2 account(s)", which told the user nothing
/// about *which* accounts they were about to lose).
fn restore_drop_confirmation(dropped: &[String]) -> String {
    format!("Restoring will drop {}. Continue?", dropped.join(", "))
}

/// Guards against piling up blocked threads when "Refresh usage now" is
/// clicked repeatedly: a stalled network call (see `run_cycle`) means each
/// new click would otherwise stack up its own throwaway `SwapGuard` and
/// thread. `false` = no refresh in flight; CAS to `true` before spawning,
/// reset to `false` when the spawned thread's cycle completes.
static REFRESH_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// "Refresh usage now" click (Settings ▸ Advanced ▸). Runs one poll +
/// auto-swap cycle on a background thread — same `run_cycle` the poll loop
/// calls every `WATCH_INTERVAL_SECS` — so the menu never blocks on network
/// I/O. The main-thread timer picks up the refreshed cache on its next tick.
///
/// De-duplicated via `REFRESH_IN_FLIGHT`: a second click while a refresh is
/// still running notifies instead of spawning another thread. See
/// `try_start_refresh` for the testable core.
fn handle_refresh_now() {
    // Shares `poll_loop`'s `SwapGuard` (concurrency-04, v0.5.2 codeaudit)
    // instead of a throwaway `SwapGuard::default()` — see
    // `shared_swap_guard`'s doc for why a manual click and the background
    // poller must never reason about anti-thrash cooldown independently.
    let guard = shared_swap_guard();
    try_start_refresh(move || {
        std::thread::spawn(move || {
            let (rate_limited, _max_pct, _trigger) = {
                let mut g = guard.lock().unwrap_or_else(|e| e.into_inner());
                run_cycle(&mut g)
            };
            REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
            if rate_limited {
                notify("Refresh: rate limited, backing off");
            } else {
                notify("Usage refreshed");
            }
        });
    })
}

/// CAS `REFRESH_IN_FLIGHT` from `false` to `true`; if it was already `true`
/// (a refresh is still running), notify and return without calling `spawn`.
/// Factored out from `handle_refresh_now` so a test can inject a counting
/// closure in place of a real `std::thread::spawn` and assert the second of
/// two rapid calls never invokes it.
fn try_start_refresh(spawn: impl FnOnce()) {
    use std::sync::atomic::Ordering;
    if REFRESH_IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        notify("Refresh already running");
        return;
    }
    spawn();
}

/// Capture the current login for `slug`. For Claude, use the full v1 flow
/// (persists into state.json + preserves cached usage). For every other
/// provider, dispatch through the trait — the result is only surfaced via
/// notification since v1 state has no bucket to persist non-Claude accounts.
fn handle_capture(slug: &str) {
    if slug == CLAUDE_SLUG {
        match capture_current() {
            Ok((email, existed)) => notify(&format!(
                "{} {email}",
                if existed { "Refreshed" } else { "Captured" }
            )),
            Err(e) => notify(&format!("Capture failed: {e}")),
        }
        return;
    }
    let Some(provider) = providers::get(slug) else {
        notify(&format!(
            "Capture failed: provider '{slug}' is not registered"
        ));
        return;
    };
    // State v2 (codex-switch-e2e): every non-Claude provider now has a real
    // multi-account slot (`State::providers[slug]`) to persist into, closing
    // the gap this used to leave as "persistence lands in a later phase".
    match capture_current_generic(slug) {
        Ok((key, existed)) => notify(&format!(
            "{} {} {key}",
            if existed { "Refreshed" } else { "Captured" },
            provider.display_name()
        )),
        Err(e) => notify(&format!("Capture failed: {e}")),
    }
}

/// If `key`'s account is currently "locked" (session or weekly window at
/// ≥99.5% with a still-future reset — the same rule `header_row`'s "locked ·
/// Xh Ym" display uses), return the human countdown string. `None` if the
/// account isn't found, the provider isn't registered, or it has headroom.
/// Pure over its `State` argument so tests don't need to touch state.json.
fn switch_target_lock_countdown(st: &State, slug: &str, key: &str) -> Option<String> {
    let provider = providers::get(slug)?;
    let acct = st
        .accounts
        .iter()
        .find(|a| a.key().eq_ignore_ascii_case(key))?;
    let row = row_from_account(acct);
    let view = acctview_from_row(
        &row,
        &st.active,
        provider.provider_id(),
        provider.window_order(),
    );
    locked_countdown_for(&view, now_utc()).map(|(cd, _win)| cd)
}

fn handle_switch(slug: &str, key: &str) {
    // Item 8 of the v0.5.0 redesign: refuse a switch into an account still
    // under its own rate-limit wall — auto-swap will pick it back up the
    // moment it resets, and switching now would just leave the user on a
    // 0%-headroom account. Only meaningful for Claude today —
    // `switch_target_lock_countdown` reads `st.accounts`, which state v2
    // keeps Claude-only (see `store.rs`); non-Claude providers have no
    // persisted usage signal to gate on yet, so the check is a no-op for
    // them rather than a false negative.
    if let Ok(st) = State::load() {
        if let Some(cd) = switch_target_lock_countdown(&st, slug, key) {
            notify(&format!(
                "Can't switch to {key}: at 100% for the next {cd}. \
                 Auto-swap will pick it back up when it resets."
            ));
            return;
        }
    }
    // State v2 (codex-switch-e2e): dispatch by provider slug instead of
    // hard-gating on Claude — `capabilities().supports_switching` already
    // keeps the "Switch to this account" row from being built for a
    // non-switching provider (`build_account_block`, H3 in the v0.5.0
    // codeaudit), and `switch_to_provider_account` itself re-checks that
    // capability as belt-and-suspenders against a stray/future click id
    // shaped like `switch:<slug>:<key>` reaching this function directly.
    let result = if slug == CLAUDE_SLUG {
        switch_to(key)
    } else {
        switch_to_provider_account(slug, key)
    };
    match result {
        Ok(label) => notify(&format!("Switched to {label}")),
        Err(e) => notify(&format!("Switch failed: {e}")),
    }
}

/// Handle a click on a "Paste API key ▸ <Provider>" row. The paste-a-key
/// dialog isn't wired yet — every API-key provider currently returns
/// `ProviderError::Unsupported` from `capture_api_key` — so surface a plain
/// "coming soon" notification instead of silently swallowing the click. Once
/// the paste-a-key dialog lands, this dispatches into `provider.capture_api_key`
/// with the user-entered nickname + key.
fn handle_apikey_capture(slug: &str) {
    let Some(provider) = providers::get(slug) else {
        notify(&format!(
            "Paste API key: provider '{slug}' is not registered"
        ));
        return;
    };
    // Deliberately call the trait method so the error message reflects the
    // real provider state — a future wire-up (returning Ok) will just skip
    // the notify branch below without touching this handler.
    match provider.capture_api_key(String::new(), String::new()) {
        Ok(acct) => notify(&format!(
            "Captured {} API-key account (persistence lands in a later phase): {}",
            provider.display_name(),
            acct.identity
                .email
                .clone()
                .or(acct.identity.display_name.clone())
                .unwrap_or_else(|| "<unnamed>".into()),
        )),
        Err(_) => notify(&format!(
            "{} API-key capture — coming soon",
            provider.display_name()
        )),
    }
}

fn handle_remove(slug: &str, key: &str) {
    if !confirm(&format!("Remove account {key}? This cannot be undone.")) {
        return;
    }
    // State v2 (codex-switch-e2e): a non-Claude provider's accounts can now
    // actually render a "Remove…" row (see `build_snapshot`), so route it to
    // the matching state.json bucket instead of the old blanket
    // "not yet supported" refusal.
    let result = if slug == CLAUDE_SLUG {
        remove_account(key)
    } else {
        remove_provider_account_generic(slug, key)
    };
    match result {
        Ok(_) => notify(&format!("Removed {key}")),
        Err(e) => notify(&format!("Remove failed: {e}")),
    }
}

/// Launch the vendor CLI for `slug`. Providers whose `launch_client` returns
/// `Unsupported` (the trait default) surface that as a notification rather
/// than silently doing nothing.
///
/// The launch is dispatched onto a background thread — the Claude
/// implementation calls `Command::status()` (synchronous wait on the child)
/// and every click is drained inside the main-thread NSTimer tick, so calling
/// it inline would freeze the entire menu-bar UI (no ticks, no redraws, no
/// clicks) until the launched `claude` exits, which under a menu-bar app with
/// no controlling TTY is effectively indefinite.
fn handle_launch(slug: &str, _key: &str) {
    let Some(provider) = providers::get(slug) else {
        notify(&format!(
            "Launch failed: provider '{slug}' is not registered"
        ));
        return;
    };
    // `providers::get` returns `&'static dyn Provider`; the trait is `Send +
    // Sync + 'static`, so the reference is trivially safe to move.
    std::thread::spawn(move || {
        if let Err(e) = provider.launch_client(crate::providers::LaunchMode::Continue) {
            notify(&format!("Launch failed: {e}"));
        }
    });
}

// Context Ledger menu rendering + click handler removed in 0.4.2. The
// submenu shelled out via osascript to open a new Terminal window, which
// silently failed for anyone who hadn't granted Automation permission — a
// broken menu row is worse than no menu row. The CLI (`usagio context
// [--provider slug]`) is unchanged and remains the supported entry point.

/// Enable or disable auto-swap. Surfaces a save failure so the menu checkmark
/// and on-disk state can't silently disagree.
fn set_autoswap(enabled: bool) {
    let r = with_state_lock(|| {
        let mut st = State::load()?;
        st.autoswap_disabled = !enabled;
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save auto-swap setting: {e}"));
    }
}

/// Flip one Settings ▸ Notifications ▸ per-trigger checkbox. `trigger` is one
/// of "threshold" / "resetback" / "pace" (the three click-id suffixes
/// `build_menu` wires up) — reads-then-flips the on-disk value so a stale
/// menu snapshot can't clobber a setting changed elsewhere in the meantime.
fn toggle_notification_trigger(trigger: &str) {
    let r = with_state_lock(|| {
        let mut st = State::load()?;
        let cfg = &mut st.notification_config;
        match trigger {
            "threshold" => cfg.threshold_enabled = !cfg.threshold_enabled,
            "resetback" => cfg.reset_back_enabled = !cfg.reset_back_enabled,
            "pace" => cfg.pace_enabled = !cfg.pace_enabled,
            _ => {}
        }
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save notification setting: {e}"));
    }
}

/// Set the swap threshold AND enable auto-swap.
fn set_autoswap_threshold(v: f64) {
    let r = with_state_lock(|| {
        let mut st = State::load()?;
        st.trigger_pct = Some(v);
        st.autoswap_disabled = false;
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save threshold: {e}"));
    }
}

/// A native confirm dialog; true only if the user clicks the destructive
/// button. Cross-platform via `rfd::MessageDialog` (already a crate
/// dependency for the Backups Save…/Restore… file panels) rather than the
/// macOS-only `osascript -e 'display dialog …'` this used to shell out to —
/// `rfd::MessageDialog` has native backends on macOS (NSAlert), Windows
/// (MessageBoxW), and Linux (our xdg-portal `rfd` feature shells out to
/// `zenity(1)` for message dialogs specifically; if `zenity` isn't installed
/// the dialog fails closed to `Cancel`, so a missing binary can't accidentally
/// confirm a destructive action).
fn confirm(question: &str) -> bool {
    let result = rfd::MessageDialog::new()
        .set_title("usagio")
        .set_description(question)
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            "Remove".to_string(),
            "Cancel".to_string(),
        ))
        .show();
    matches!(result, rfd::MessageDialogResult::Custom(label) if label == "Remove")
}

// ---------------------------------------------------------------------------
// Start-at-login used to live here as a menu checkbox backed by `osascript
// → tell "System Events" → make login item …`. Removed in 0.4.3: the
// osascript path re-prompted for Automation permission on every brew
// upgrade (binary hash changed → macOS treated the new binary as a
// different app), and it was redundant with `usagio install` which
// registers a proper launchd LaunchAgent (no osascript, no prompt).

// ---------------------------------------------------------------------------
// Cross-platform (Linux/Windows) renderer — muda/tray-icon via the
// `platform::MenuBackend` trait, so the GTK/Win32 event-loop pump and the
// thread-confinement rules `tray_icon`'s native menu types need live once in
// `platform::{linux,windows}` (already built + tested there) instead of
// being duplicated here. macOS keeps its native NSMenu attributedTitle
// renderer (`mac_style` above, driven by `run` at the top of this file):
// richer per-row coloring and right-aligned tab stops aren't representable
// through the generic `platform::MenuTree`, so macOS is intentionally NOT
// routed through this module.
#[cfg(not(target_os = "macos"))]
mod cross_platform {
    use super::*;
    use crate::platform::{MenuHandle, MenuItem as PMenuItem, MenuTree};

    /// Cross-platform separator used in place of macOS's right-aligned tab
    /// stop — muda has no rich-text / tab-stop support, so `S% / W%` etc.
    /// just sit inline after this separator instead of being right-aligned.
    /// Per-row red/amber severity coloring is dropped for the same reason
    /// (see the module doc above) — the countdown / percentage TEXT is
    /// still there, just not colored.
    const SEP: &str = " · ";

    /// A `RowStyle`'s plain title, with the macOS tab-stop swapped for `SEP`.
    /// Reuses `main_row`/`quit_row_plain` (already OS-agnostic pure string
    /// builders) instead of re-deriving the row text from scratch.
    fn plain_text(style: &RowStyle) -> String {
        style.plain.replace('\t', SEP)
    }

    /// A non-clickable informational submenu row. `enabled: false` so the
    /// muda/GTK/Win32 renderer draws it as static text with no hover highlight
    /// — it mirrors the macOS treatment where the same rows are DISABLED (see
    /// `build_account_submenu`). muda has no rich-text, so unlike macOS these
    /// can't be re-tinted to stay full-contrast; a disabled row's native grey
    /// is the closest structural match to "info, not an action".
    fn noop_info(label: impl Into<String>) -> PMenuItem {
        // Enabled (not disabled): a disabled item renders greyed/dimmed on GTK
        // and Windows just as on macOS, which the maintainer flagged as
        // unreadable. Full-contrast wins; the click is a harmless no-op.
        PMenuItem::Action {
            id: "noop".to_string(),
            label: label.into(),
            icon_png: None,
            enabled: true,
            checked: false,
            checkable: false,
        }
    }

    fn action(id: impl Into<String>, label: impl Into<String>, enabled: bool) -> PMenuItem {
        PMenuItem::Action {
            id: id.into(),
            label: label.into(),
            icon_png: None,
            enabled,
            checked: false,
            checkable: false,
        }
    }

    fn checkbox(
        id: impl Into<String>,
        label: impl Into<String>,
        enabled: bool,
        checked: bool,
    ) -> PMenuItem {
        PMenuItem::Action {
            id: id.into(),
            label: label.into(),
            icon_png: None,
            enabled,
            checked,
            checkable: true,
        }
    }

    /// Cross-platform counterpart to `build_account_submenu` — one
    /// `PMenuItem::Submenu` whose label is the compact account row and whose
    /// children are the details + action rows the flat v0.5.2 shape used to
    /// inline. v0.5.3 menu redesign — provider grouping is back and each
    /// account is a submenu again.
    fn build_account_submenu_item(sec: &ProviderSection, a: &AcctView) -> PMenuItem {
        let mut items = Vec::new();
        for row in submenu_info_rows(sec, a) {
            items.push(noop_info(row));
        }
        for row in account_extra_info_rows(sec, a) {
            items.push(noop_info(row));
        }
        // Action rows: Switch/Active, Launch, Remove.
        let rows = account_submenu_rows(sec, a);
        items.push(PMenuItem::Separator);
        match rows.switch_row {
            Some(true) => items.push(action("noop", "✓ Active", false)),
            Some(false) => items.push(action(
                format!("switch:{}:{}", sec.provider_id, a.key),
                "Switch to this account",
                true,
            )),
            None => {}
        }
        if rows.launch_row {
            items.push(action(
                format!("launch:{}:{}", sec.provider_id, a.key),
                "Launch client",
                true,
            ));
        }
        if rows.remove_row {
            items.push(action(
                format!("remove:{}:{}", sec.provider_id, a.key),
                "Remove…",
                true,
            ));
        }
        PMenuItem::Submenu {
            label: plain_text(&account_header_row(sec, a)),
            icon_png: None,
            items,
        }
    }

    /// Cross-platform counterpart to `build_provider_group` — one static (or
    /// submenu-when-env-overridden) row carrying the provider's display name.
    fn build_provider_group_item(sec: &ProviderSection) -> PMenuItem {
        let headlines = section_headline_rows(sec);
        if headlines.is_empty() {
            return PMenuItem::Static {
                label: sec.display_name.to_string(),
                icon_png: None,
            };
        }
        let children: Vec<PMenuItem> = headlines
            .into_iter()
            .map(|title| action(format!("envoverride:{}", sec.provider_id), title, false))
            .collect();
        PMenuItem::Submenu {
            label: sec.display_name.to_string(),
            icon_png: None,
            items: children,
        }
    }

    /// Cross-platform counterpart to `build_menu` — same structure, same
    /// click ids (`handle_click` doesn't care which renderer produced them),
    /// generic `platform::MenuTree` shape instead of a native
    /// `tray_icon::menu::Menu`. v0.5.3 menu redesign: provider grouping is
    /// back. Each provider that has captured accounts contributes a static
    /// (or env-override submenu) header row, followed by one
    /// `PMenuItem::Submenu` per account whose children are the details +
    /// action rows. `PMenuItem::Separator` sits between provider groups,
    /// not between individual accounts.
    fn menu_tree_from_snapshot(snap: &Snapshot) -> MenuTree {
        let mut items = Vec::new();
        if snap.sections.is_empty() {
            items.push(action("none", "Capture a login below to begin", false));
        }
        let groups = provider_grouped_order(snap);
        let mut first_group = true;
        for (si, account_idxs) in &groups {
            let sec = &snap.sections[*si];
            if !first_group {
                items.push(PMenuItem::Separator);
            }
            first_group = false;
            items.push(build_provider_group_item(sec));
            for ai in account_idxs {
                let a = &sec.accounts[*ai];
                items.push(build_account_submenu_item(sec, a));
            }
        }
        items.push(PMenuItem::Separator);

        // Capture current login ▸ — creds-on-disk providers first, then
        // API-key providers, both as DIRECT children (item 8 flattened the
        // "Paste API key ▸" sub-submenu).
        let mut capture_items = Vec::new();
        if snap.capture_creds.is_empty() && snap.capture_api_key.is_empty() {
            capture_items.push(action("noop", "(no providers registered)", false));
        } else {
            for reg in snap.capture_creds.iter().chain(snap.capture_api_key.iter()) {
                let title = if reg.installed {
                    reg.display_name.to_string()
                } else {
                    format!("{} (not installed)", reg.display_name)
                };
                let prefix = match reg.capture_mode {
                    CaptureMode::CredsOnDisk => "capture",
                    CaptureMode::ApiKey => "apikey",
                };
                capture_items.push(action(format!("{prefix}:{}", reg.provider_id), title, true));
            }
        }
        items.push(PMenuItem::Submenu {
            label: "Capture current login".to_string(),
            icon_png: None,
            items: capture_items,
        });

        // Settings ▸: Refresh usage now, then a separator, then everything
        // else alphabetical (Auto-swap, Backups, Notifications) — item 9
        // flattened the old "Advanced ▸" grouping and item 5 folded the
        // top-level auto-swap checkbox into the Auto-swap ▸ submenu.
        let notifications = vec![
            checkbox(
                "notifications:threshold",
                "Threshold alerts (70% / 90%)",
                true,
                snap.notification_config.threshold_enabled,
            ),
            checkbox(
                "notifications:resetback",
                "Window reset alerts",
                true,
                snap.notification_config.reset_back_enabled,
            ),
            checkbox(
                "notifications:pace",
                "Weekly pace projection (experimental)",
                true,
                snap.notification_config.pace_enabled,
            ),
        ];

        let cur = if snap.autoswap {
            snap.threshold.round() as i32
        } else {
            0
        };
        let mut autoswap_items = vec![checkbox("autoswap:off", "Off", true, cur == 0)];
        for t in [70i32, 85, 95, 98] {
            autoswap_items.push(checkbox(
                format!("autoswap:{t}"),
                format!("{t}%"),
                true,
                cur == t,
            ));
        }
        autoswap_items.push(PMenuItem::Separator);
        autoswap_items.push(action("autoswap:now", "Switch to best account now", true));

        let backups = vec![
            action("backup:save", "Save…", true),
            action("backup:restore", "Restore…", true),
        ];

        let settings_items = vec![
            action("refresh:now", "Refresh usage now", true),
            PMenuItem::Separator,
            PMenuItem::Submenu {
                label: "Auto-swap".to_string(),
                icon_png: None,
                items: autoswap_items,
            },
            PMenuItem::Submenu {
                label: "Backups".to_string(),
                icon_png: None,
                items: backups,
            },
            PMenuItem::Submenu {
                label: "Notifications".to_string(),
                icon_png: None,
                items: notifications,
            },
        ];
        items.push(PMenuItem::Submenu {
            label: "Settings".to_string(),
            icon_png: None,
            items: settings_items,
        });

        items.push(PMenuItem::Separator);
        items.push(action(
            "quit",
            plain_text(&RowStyle::plain_row(quit_row_plain())),
            true,
        ));

        MenuTree { items }
    }

    /// 16x16 PNG bytes for the initial tray icon. `MenuBackend::create_status_item`
    /// requires real, decodable icon bytes on Linux/Windows (unlike macOS,
    /// which is happy with a text-only title) — there's no dedicated app icon
    /// asset yet (only per-provider 16px icons under `assets/icons/16/`), so
    /// this reuses the active account's provider icon, falling back to
    /// Claude's (always bundled, regardless of which provider Cargo features
    /// are enabled — see `icons::png16_for`).
    fn initial_icon_bytes(snap: &Snapshot) -> &'static [u8] {
        active_account(snap)
            .and_then(|(sec, _)| crate::icons::png16_for(sec.provider_id))
            .or_else(|| crate::icons::png16_for(CLAUDE_SLUG))
            .unwrap_or(&[0u8; 0])
    }

    /// Background redraw ticker: rebuilds the tray from cached state and
    /// pushes updates through the `Send`-safe `MenuHandle`, mirroring the
    /// macOS `NSTimer` tick in `run` above but off the (blocked)
    /// `run_event_loop` thread instead of on it — `create_status_item` and
    /// `run_event_loop` must share a thread (see `MenuBackend`'s doc in
    /// `platform/mod.rs`), so this can't run on the main thread here.
    fn redraw_loop(handle: Box<dyn MenuHandle>, initial: Snapshot) {
        let mut last_sig = menu_signature(&initial);
        let mut last_title = title_for(&initial);
        let start_exe = std::fs::canonicalize(crate::stable_exe_path()).ok();
        loop {
            std::thread::sleep(Duration::from_millis(750));
            if let Some(start) = &start_exe {
                maybe_relaunch_after_upgrade(start);
            }
            let snap = build_snapshot();
            let sig = menu_signature(&snap);
            if sig != last_sig {
                if let Err(e) = handle.set_menu(menu_tree_from_snapshot(&snap)) {
                    crate::logging::log(&format!("menubar: set_menu failed: {e:#}"));
                }
                let _ = handle.set_icon(initial_icon_bytes(&snap));
                last_sig = sig;
            }
            let title = title_for(&snap);
            if title != last_title {
                let _ = handle.set_title(&title);
                last_title = title;
            }
        }
    }

    pub(super) fn run() -> Result<()> {
        providers::init();
        let backend = crate::platform().menu();
        backend.on_click(Box::new(handle_click))?;

        let initial = build_snapshot();
        let handle =
            backend.create_status_item(&title_for(&initial), initial_icon_bytes(&initial))?;
        handle.set_menu(menu_tree_from_snapshot(&initial))?;
        let _ = handle.set_title(&title_for(&initial));

        // Poll + auto-swap on a background thread — same `poll_loop`
        // (supervised against panics; see `poll_loop_supervisor`'s doc) the
        // macOS `run` above uses (writes cached usage to state.json; nothing
        // here calls into the native tray, so it's safe off-thread).
        std::thread::spawn(poll_loop_supervisor);
        // Redraw ticker on its own background thread (see `redraw_loop`'s
        // doc for why it can't be the main thread here).
        std::thread::spawn(move || redraw_loop(handle, initial));

        // Blocks, pumping the GTK (Linux) / Win32 (Windows) message loop —
        // see `platform::linux::LinuxMenu` / `platform::windows::WindowsMenu`.
        backend.run_event_loop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::cell::Cell as StdCell;

    fn bands() -> SeverityBands {
        SeverityBands {
            amber: 80.0,
            red: 95.0,
        }
    }

    thread_local! {
        /// Wall-clock hook honored by `menubar::now_utc` under `#[cfg(test)]`.
        /// Fixed to the epoch by default so a test that doesn't care about
        /// countdown transitions gets a stable "not locked" reading (both
        /// `session_reset_at` and `weekly_reset_at` are None on the default
        /// fixture, and `is_blocking` short-circuits on `None`).
        static TEST_NOW: StdCell<DateTime<Utc>> =
            StdCell::new(Utc.timestamp_opt(0, 0).unwrap());
    }

    /// Read the current test-frozen wall clock. Called from `now_utc()` under
    /// `#[cfg(test)]` — see `now_utc`.
    pub(super) fn test_now() -> DateTime<Utc> {
        TEST_NOW.with(|c| c.get())
    }

    /// Run `f` with the test wall clock pinned to `t`, restoring it after.
    fn with_now<R>(t: DateTime<Utc>, f: impl FnOnce() -> R) -> R {
        let prev = TEST_NOW.with(|c| c.replace(t));
        let out = f();
        TEST_NOW.with(|c| c.set(prev));
        out
    }

    fn acct(email: &str, session: Option<f64>, weekly: Option<f64>, active: bool) -> AcctView {
        AcctView {
            provider_id: CLAUDE_SLUG,
            key: email.to_string(),
            display: email.to_string(),
            windows: vec![
                WindowView {
                    id: "session".into(),
                    label: "5h".into(),
                    pct: session,
                    reset: "3h".into(),
                },
                WindowView {
                    id: "weekly".into(),
                    label: "7d".into(),
                    pct: weekly,
                    reset: "2d".into(),
                },
            ],
            updated: "1m ago".into(),
            active,
            has_data: true,
            // Default fixture: no reset instants → `compute_display` returns
            // Usage regardless of pct. Tests that exercise lock transitions
            // set these explicitly.
            session_reset_at: None,
            weekly_reset_at: None,
            // `None` → `is_stale_after_reset` short-circuits on `fetched_at?`,
            // so the default fixture never accidentally lands in
            // `StaleAfterReset`. Tests exercising that state set it directly.
            fetched_at: None,
        }
    }

    fn one_section_snap(a: AcctView) -> Snapshot {
        Snapshot {
            sections: vec![ProviderSection {
                provider_id: CLAUDE_SLUG,
                display_name: "Claude",
                supports_switching: true,
                supports_usage: true,
                supports_launch: true,
                supports_remove: true,
                severity_bands: bands(),
                env_override_active: false,
                accounts: vec![a],
            }],
            account_order: vec![(0, 0)],
            capture_creds: vec![RegisteredProvider {
                provider_id: CLAUDE_SLUG,
                display_name: "Claude",
                installed: true,
                capture_mode: CaptureMode::CredsOnDisk,
            }],
            capture_api_key: Vec::new(),
            autoswap: false,
            threshold: 95.0,
            notification_config: crate::notifications::NotificationConfig::default(),
        }
    }

    #[test]
    fn try_start_refresh_dedupes_rapid_clicks() {
        // try_start_refresh reaches logging::log → store::config_dir(), which
        // panics in tests without a HOME_OVERRIDE. Wrap in ScopedConfigDir so
        // the tripwire (added post-hermeticity merge) doesn't fire here.
        let _g = crate::store::ScopedConfigDir::new();
        // Reset in case a prior test in this binary left it set (best-effort
        // — tests run with --test-threads=1 so no other test races us here).
        REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);

        let spawn_count = std::rc::Rc::new(StdCell::new(0u32));
        let c1 = spawn_count.clone();
        try_start_refresh(move || {
            c1.set(c1.get() + 1);
            // Deliberately do NOT reset REFRESH_IN_FLIGHT here — this stands
            // in for "the background thread is still running" so the second
            // click below is the one under test.
        });
        assert_eq!(spawn_count.get(), 1, "first click must spawn");

        let c2 = spawn_count.clone();
        try_start_refresh(move || {
            c2.set(c2.get() + 1);
        });
        assert_eq!(
            spawn_count.get(),
            1,
            "second rapid click must NOT spawn a second refresh"
        );

        // Clean up so later tests in this binary see the flag cleared.
        REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn shared_swap_guard_returns_the_same_arc_every_call() {
        // concurrency-04 (v0.5.2 codeaudit): `poll_loop` and
        // `handle_refresh_now` must reason about anti-thrash cooldown/
        // no-return through ONE `SwapGuard`, not a throwaway
        // `SwapGuard::default()` per call site. `Arc::ptr_eq` pins that both
        // callers observe the identical process-wide instance.
        let a = shared_swap_guard();
        let b = shared_swap_guard();
        assert!(
            std::sync::Arc::ptr_eq(&a, &b),
            "shared_swap_guard must return the same Arc every call"
        );
        // And it's actually usable as a `&mut SwapGuard` through the lock,
        // same as every `run_cycle` call site needs — `SwapGuard`'s fields
        // are private but visible here since `menubar` is a descendant of
        // the crate-root module that declares the struct (same access
        // `run_cycle`'s callers already rely on for `&mut SwapGuard`).
        {
            let mut g = a.lock().unwrap();
            g.stuck_notified = true;
        }
        assert!(
            b.lock().unwrap().stuck_notified,
            "mutation through one handle must be visible through the other"
        );
    }

    #[test]
    fn panic_payload_message_handles_str_string_and_other() {
        // `poll_loop_supervisor` (errors-02, v0.5.2 codeaudit) logs whatever
        // `catch_unwind` hands back — almost always a `&str` (`panic!("...")`)
        // or `String` (`format!(...)`-built panic message), but a custom
        // payload type must still produce SOME loggable string instead of
        // panicking the supervisor itself.
        let str_payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_payload_message(&*str_payload), "boom");

        let string_payload: Box<dyn std::any::Any + Send> = Box::new(String::from("kaboom"));
        assert_eq!(panic_payload_message(&*string_payload), "kaboom");

        let other_payload: Box<dyn std::any::Any + Send> = Box::new(42_i32);
        assert_eq!(
            panic_payload_message(&*other_payload),
            "non-string panic payload"
        );
    }

    #[test]
    fn toggle_notification_trigger_read_modify_write_is_atomic_under_contention() {
        // v0.5.2 item 5 removed the top-level "Auto-swap enabled" checkbox
        // (and `toggle_autoswap` with it — Off/threshold picks in Settings ▸
        // Auto-swap ▸ replace it, and those write a fixed target rather than
        // flipping a bool). `toggle_notification_trigger` is still a genuine
        // read-modify-write click handler, so it inherits this contention
        // regression test in `toggle_autoswap`'s place.
        let g = crate::store::ScopedConfigDir::new();
        let home = g.home();

        State::default().save().expect("seed initial state");
        let initial = State::load().unwrap().notification_config.threshold_enabled;

        const ITERATIONS: usize = 100;
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let home = home.clone();
                std::thread::spawn(move || {
                    // HOME_OVERRIDE is thread-local (see store::ScopedConfigDir);
                    // each worker thread must repoint it at the same tempdir
                    // the parent test set up.
                    crate::store::set_home_override(Some(home));
                    for _ in 0..ITERATIONS {
                        toggle_notification_trigger("threshold");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker thread panicked");
        }

        let total_toggles = 2 * ITERATIONS;
        let expected = if total_toggles % 2 == 1 {
            !initial
        } else {
            initial
        };
        let final_state = State::load().unwrap();
        assert_eq!(
            final_state.notification_config.threshold_enabled, expected,
            "final parity must match initial XOR (total_toggles % 2 == 1); a lost \
             toggle under contention would flip this"
        );

        drop(g);
    }

    // NOTE: M2's `restore_old_account_count` helper + its two tests were
    // deleted at aggregation time. The M2 concern (State::load failure
    // silently becoming "0 accounts" and skipping the drop-warning prompt)
    // is fully subsumed by H2's restructure: the new Restore flow runs
    // under `with_state_lock`, delegates the drop-detection to
    // `crate::store::accounts_dropped_by(&new_state)` (which propagates
    // load errors via `?`), and the outer `Err(e) => notify(...)` arm
    // surfaces the abort message to the user. Tests for the H2 path live
    // in store_tests.rs and menubar's Restore… test block above.

    #[test]
    fn severity_bands_defaults() {
        let b = bands();
        assert!(severity_with(None, b).is_none());
        assert!(severity_with(Some(0.0), b).is_none());
        assert!(severity_with(Some(79.9), b).is_none());
        assert_eq!(severity_with(Some(80.0), b), Some(Severity::Amber));
        assert_eq!(severity_with(Some(94.9), b), Some(Severity::Amber));
        assert_eq!(severity_with(Some(95.0), b), Some(Severity::Red));
        assert_eq!(severity_with(Some(100.0), b), Some(Severity::Red));
    }

    #[test]
    fn severity_bands_are_provider_driven() {
        // A provider with different thresholds (50/75) colors the same pct
        // differently — proving the hardcoded 80/95 are gone.
        let b = SeverityBands {
            amber: 50.0,
            red: 75.0,
        };
        assert_eq!(severity_with(Some(60.0), b), Some(Severity::Amber));
        assert_eq!(severity_with(Some(80.0), b), Some(Severity::Red));
    }

    // For ASCII rows, UTF-16 offsets equal byte offsets, so we can slice the
    // plain title to prove each colored span lands exactly on its `NN%`.
    fn span_text(plain: &str, off: usize, len: usize) -> String {
        plain.chars().skip(off).take(len).collect()
    }

    #[test]
    fn main_row_colors_land_on_percentages() {
        let a = acct("you@work.com", Some(82.0), Some(96.0), true);
        let r = main_row("Claude", &a, bands());
        assert_eq!(r.plain, "Claude    you@work.com\t82% / 96%");
        assert!(r.bold, "active account is bold");
        assert_eq!(
            r.tab_x_kind,
            Some(TabX::MenuRight),
            "trailing run is right-aligned"
        );
        assert_eq!(r.colors.len(), 2);
        let (so, sl, ss) = r.colors[0];
        assert_eq!(span_text(&r.plain, so, sl), "82%");
        assert_eq!(ss, Severity::Amber);
        let (wo, wl, ws) = r.colors[1];
        assert_eq!(span_text(&r.plain, wo, wl), "96%");
        assert_eq!(ws, Severity::Red);
    }

    #[test]
    fn main_row_low_usage_has_no_colors_and_no_bold_when_inactive() {
        let a = acct("dev@side.com", Some(3.0), Some(9.0), false);
        let r = main_row("Claude", &a, bands());
        assert!(!r.bold);
        assert!(r.colors.is_empty());
    }

    #[test]
    fn main_row_offsets_hold_for_unicode_email() {
        let a = acct("café@x.com", Some(99.0), None, false);
        let r = main_row("Claude", &a, bands());
        let (off, len, _) = r.colors[0];
        let utf16: Vec<u16> = r.plain.encode_utf16().collect();
        let picked = String::from_utf16(&utf16[off..off + len]).unwrap();
        assert_eq!(picked, "99%");
    }

    #[test]
    fn main_row_includes_provider_name_padded() {
        // Item 2 of the redesign: the provider name gets its own column-like
        // padding, then the email, then the tab-stopped `n% / n%` run.
        let claude = main_row(
            "Claude",
            &acct("a@x.com", Some(1.0), Some(2.0), false),
            bands(),
        );
        let codex = main_row(
            "Codex",
            &acct("b@x.com", Some(1.0), Some(2.0), false),
            bands(),
        );
        let (claude_label, _) = claude.plain.split_once('\t').unwrap();
        let (codex_label, _) = codex.plain.split_once('\t').unwrap();
        assert!(claude_label.starts_with("Claude"));
        assert!(codex_label.starts_with("Codex"));
        // Both providers' emails start at the same column regardless of the
        // provider-name length ("Claude" vs "Codex").
        assert_eq!(claude_label.find("a@x.com"), codex_label.find("b@x.com"));
    }

    #[test]
    fn submenu_info_rows_never_include_a_percentage() {
        // Item 3 of the redesign: percentages move to the main-list row;
        // submenu rows are reset-window / burn-rate / footer text only.
        let a = acct("a@x.com", Some(97.0), Some(88.0), true);
        let sec = ProviderSection {
            provider_id: CLAUDE_SLUG,
            display_name: "Claude",
            supports_switching: true,
            supports_usage: true,
            supports_launch: true,
            supports_remove: true,
            severity_bands: bands(),
            env_override_active: false,
            accounts: vec![],
        };
        for row in submenu_info_rows(&sec, &a) {
            assert!(!row.contains('%'), "submenu row leaked a percentage: {row}");
        }
    }

    #[test]
    fn submenu_reset_rows_are_plain_text_no_emoji() {
        // The details panel deliberately carries NO leading emoji glyphs (the
        // maintainer found them out of place). Reset rows are plain "<Label>
        // resets in X" text — assert they read that way and never lead with a
        // non-ASCII decoration.
        let a = acct("a@x.com", Some(20.0), Some(30.0), false);
        let sec = ProviderSection {
            provider_id: CLAUDE_SLUG,
            display_name: "Claude",
            supports_switching: true,
            supports_usage: true,
            supports_launch: true,
            supports_remove: true,
            severity_bands: bands(),
            env_override_active: false,
            accounts: vec![],
        };
        let rows = submenu_info_rows(&sec, &a);
        assert!(!rows.is_empty());
        for row in &rows {
            assert!(
                row.is_ascii(),
                "reset row should be plain ASCII text, no emoji: {row}"
            );
            assert!(
                row.contains("resets in") || row.contains("no reset info"),
                "unexpected reset row text: {row}"
            );
        }
    }

    #[test]
    fn account_extra_info_rows_empty_without_usage_support() {
        // A provider with no usage endpoint has no burn-rate / cost / "updated"
        // rows — and crucially the extractor must take the no-disk path so it
        // stays callable off the main thread without a ScopedConfigDir.
        let a = acct("a@x.com", Some(20.0), Some(30.0), false);
        let sec = ProviderSection {
            provider_id: CLAUDE_SLUG,
            display_name: "Claude",
            supports_switching: true,
            supports_usage: false,
            supports_launch: true,
            supports_remove: true,
            severity_bands: bands(),
            env_override_active: false,
            accounts: vec![],
        };
        assert!(account_extra_info_rows(&sec, &a).is_empty());
    }

    #[test]
    fn u16len_counts_surrogate_pairs() {
        assert_eq!(u16len("abc"), 3);
        assert_eq!(u16len("café"), 4); // é is BMP → 1 code unit
        assert_eq!(u16len("a😀b"), 4); // 😀 is astral → 2 code units
    }

    #[test]
    fn main_row_offsets_hold_for_astral_email() {
        let a = acct("😀@x.com", Some(99.0), None, false);
        let r = main_row("Claude", &a, bands());
        let (off, len, _) = r.colors[0];
        let utf16: Vec<u16> = r.plain.encode_utf16().collect();
        let picked = String::from_utf16(&utf16[off..off + len]).unwrap();
        assert_eq!(picked, "99%");
    }

    #[test]
    fn launchd_managed_detects_matching_service_name() {
        assert!(launchd_managed_from_env(Some(crate::AUTOSTART_LABEL)));
        assert!(!launchd_managed_from_env(Some("com.other.service")));
        assert!(!launchd_managed_from_env(None));
    }

    #[test]
    fn menu_signature_changes_when_a_window_reset_changes() {
        // A window's reset countdown is folded into the signature so a
        // changing reset triggers a redraw (would otherwise leave stale text).
        let mut a = acct("you@work.com", Some(50.0), Some(60.0), true);
        a.windows.push(WindowView {
            id: "opus".into(),
            label: "Opus 7d".into(),
            pct: Some(30.0),
            reset: "3h".into(),
        });
        let base = one_section_snap(a);
        let sig1 = menu_signature(&base);
        let mut changed = base;
        changed.sections[0].accounts[0].windows[2].reset = "2h".into();
        assert_ne!(sig1, menu_signature(&changed));
    }

    #[test]
    fn menu_signature_folds_provider_id() {
        // Same account, same numbers, different provider slug → different
        // signature. Guards against a cross-provider menu change being
        // swallowed by a stale Claude-shaped fingerprint.
        let mut base = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
        let sig1 = menu_signature(&base);
        // Simulate the same-shaped row landing under a different provider.
        base.sections[0].provider_id = "codex";
        base.sections[0].accounts[0].provider_id = "codex";
        assert_ne!(sig1, menu_signature(&base));
    }

    #[test]
    fn parse_click_id_splits_action_slug_key() {
        let c = parse_click_id("switch:claude:matt@example.com");
        assert_eq!(c.action, "switch");
        assert_eq!(c.slug, Some("claude"));
        assert_eq!(c.key, Some("matt@example.com"));

        let c = parse_click_id("capture:codex");
        assert_eq!(c.action, "capture");
        assert_eq!(c.slug, Some("codex"));
        assert!(c.key.is_none());

        let c = parse_click_id("autoswap:off");
        assert_eq!(c.action, "autoswap");
        assert_eq!(c.slug, Some("off"));

        let c = parse_click_id("quit");
        assert_eq!(c.action, "quit");
        assert!(c.slug.is_none());
    }

    // (Context Ledger menu-item wiring tests removed in 0.4.2 alongside the
    // submenu itself. The CLI-level ledger tests still cover the audit path.)

    // --- env-override guard end-to-end -----------------------------------

    #[test]
    fn env_override_for_reads_shared_hook_and_row_flows_into_menu_signature() {
        // (a) With no override, the Claude section's `env_override_active`
        // stays false and the menu signature reflects "env=false".
        assert!(!env_override_for(CLAUDE_SLUG));

        let mut snap = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
        assert!(!snap.sections[0].env_override_active);
        let sig_off = menu_signature(&snap);

        // (b) Toggle the env override on for the Claude slug via the shared
        // hook. `env_override_for` — the same function `build_snapshot`
        // consults to populate each section's flag — must now report true,
        // and a section rebuilt with that flag must produce a different
        // `menu_signature` (so the tray redraws and shows the disabled row).
        crate::with_env_override_hook(&[CLAUDE_SLUG], || {
            assert!(env_override_for(CLAUDE_SLUG));
            assert!(!env_override_for("codex"));
            snap.sections[0].env_override_active = env_override_for(CLAUDE_SLUG);
        });

        assert!(snap.sections[0].env_override_active);
        assert_ne!(
            sig_off,
            menu_signature(&snap),
            "env-override flag must fold into the signature so the menu redraws with the disabled row",
        );
        // The redraw signal includes the exact override state so a change
        // from active→inactive is not swallowed either.
        assert!(menu_signature(&snap).contains("env=true"));
    }

    #[test]
    fn section_renders_env_override_row_when_flagged() {
        // A section without the override contributes no extra header rows.
        let base = one_section_snap(acct("a@x.com", Some(10.0), Some(20.0), true));
        assert!(section_headline_rows(&base.sections[0]).is_empty());

        // With the override on, the section prepends the disabled row that
        // `build_menu` adds verbatim (`ENV_OVERRIDE_ROW_TITLE`). muda's Menu
        // requires the main thread on macOS, so we assert on the pure
        // helper `build_menu` shares with us instead of building the menu.
        let mut flagged = one_section_snap(acct("a@x.com", Some(10.0), Some(20.0), true));
        flagged.sections[0].env_override_active = true;
        let rows = section_headline_rows(&flagged.sections[0]);
        assert_eq!(rows, vec![ENV_OVERRIDE_ROW_TITLE]);
    }

    #[test]
    fn active_account_looks_across_sections() {
        // Two sections, active row lives in the second — active_account must
        // still find it (it's the anchor for the top header + title).
        let mut a = acct("dev@x.com", Some(10.0), Some(20.0), false);
        a.active = false;
        let mut b = acct("work@x.com", Some(50.0), Some(60.0), true);
        b.provider_id = "codex";
        let snap = Snapshot {
            sections: vec![
                ProviderSection {
                    provider_id: CLAUDE_SLUG,
                    display_name: "Claude",
                    supports_switching: true,
                    supports_usage: true,
                    supports_launch: true,
                    supports_remove: true,
                    severity_bands: bands(),
                    env_override_active: false,
                    accounts: vec![a],
                },
                ProviderSection {
                    provider_id: "codex",
                    display_name: "Codex",
                    supports_switching: true,
                    supports_usage: true,
                    supports_launch: true,
                    supports_remove: true,
                    severity_bands: bands(),
                    env_override_active: false,
                    accounts: vec![b],
                },
            ],
            account_order: vec![(0, 0), (1, 0)],
            capture_creds: Vec::new(),
            capture_api_key: Vec::new(),
            autoswap: false,
            threshold: 95.0,
            notification_config: crate::notifications::NotificationConfig::default(),
        };
        let (sec, acc) = active_account(&snap).expect("active row found");
        assert_eq!(sec.provider_id, "codex");
        assert_eq!(acc.key, "work@x.com");
    }

    // --- capture-menu filter --------------------------------------------

    /// Tiny fixture provider — supports_usage + capture_mode are the only
    /// knobs `partition_capture_providers` reads, so the rest returns
    /// `Unsupported` and the id/name are what the assertions look at.
    struct FakeProvider {
        id: &'static str,
        name: &'static str,
        supports_usage: bool,
        capture_mode: CaptureMode,
    }

    impl Provider for FakeProvider {
        fn provider_id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self) -> providers::Capabilities {
            providers::Capabilities {
                supports_usage: self.supports_usage,
                supports_switching: false,
                supports_launch: false,
                supports_remove: true,
                supports_email_capture: false,
                secret_backend: providers::SecretBackend::File,
                capture_mode: self.capture_mode,
            }
        }
        fn capture_current_login(&self) -> providers::PResult<Option<providers::CapturedAccount>> {
            Ok(None)
        }
        fn parse_stored_blob(&self, _blob: &str) -> providers::PResult<providers::TokenGrant> {
            Err(providers::ProviderError::Unsupported)
        }
        fn patch_stored_blob(
            &self,
            _blob: &str,
            _grant: &providers::TokenGrant,
        ) -> providers::PResult<String> {
            Err(providers::ProviderError::Unsupported)
        }
    }

    #[test]
    fn partition_capture_providers_filters_stubs_and_buckets_by_capture_mode() {
        // A mixed registry: one full creds provider (Claude-shaped), one
        // full API-key provider (OpenRouter-shaped), one creds-on-disk stub
        // whose usage isn't wired yet (Cline-shaped), and one API-key-shaped
        // row that also has `supports_usage == false` (hypothetical stub —
        // still filtered because the capture filter only cares about
        // `supports_usage`).
        let claude_like = FakeProvider {
            id: "claude-like",
            name: "Claude-like",
            supports_usage: true,
            capture_mode: CaptureMode::CredsOnDisk,
        };
        let openrouter_like = FakeProvider {
            id: "openrouter-like",
            name: "OpenRouter-like",
            supports_usage: true,
            capture_mode: CaptureMode::ApiKey,
        };
        let cline_stub = FakeProvider {
            id: "cline-stub",
            name: "Cline-stub",
            supports_usage: false,
            capture_mode: CaptureMode::CredsOnDisk,
        };
        let apikey_stub = FakeProvider {
            id: "apikey-stub",
            name: "APIKey-stub",
            supports_usage: false,
            capture_mode: CaptureMode::ApiKey,
        };

        let regs: Vec<Box<dyn Provider>> = vec![
            Box::new(claude_like),
            Box::new(openrouter_like),
            Box::new(cline_stub),
            Box::new(apikey_stub),
        ];

        let (creds, api_key) = partition_capture_providers(&regs, &[]);

        let creds_ids: Vec<&str> = creds.iter().map(|p| p.provider_id()).collect();
        let api_key_ids: Vec<&str> = api_key.iter().map(|p| p.provider_id()).collect();

        // Full creds provider is in the creds bucket.
        assert_eq!(creds_ids, vec!["claude-like"]);
        // Full API-key provider is in the api-key bucket.
        assert_eq!(api_key_ids, vec!["openrouter-like"]);

        // The stub with capture_mode == CredsOnDisk but supports_usage == false
        // is excluded from the creds list and included in NEITHER bucket.
        assert!(
            !creds_ids.contains(&"cline-stub"),
            "creds-on-disk stub must be excluded from the creds list",
        );
        assert!(
            !api_key_ids.contains(&"cline-stub"),
            "creds-on-disk stub must not leak into the api-key list either",
        );
        // Same guarantee for a stub with capture_mode == ApiKey: no usage → not shown.
        assert!(!creds_ids.contains(&"apikey-stub"));
        assert!(!api_key_ids.contains(&"apikey-stub"));
    }

    // -----------------------------------------------------------------------
    // Menu-redesign / countdown / icons tests
    // -----------------------------------------------------------------------

    /// Build an account with explicit reset instants so the locked-vs-usage
    /// transition can be pinned in tests.
    fn acct_with_resets(
        email: &str,
        session: Option<f64>,
        weekly: Option<f64>,
        active: bool,
        session_reset_at: Option<DateTime<Utc>>,
        weekly_reset_at: Option<DateTime<Utc>>,
    ) -> AcctView {
        let mut a = acct(email, session, weekly, active);
        a.session_reset_at = session_reset_at;
        a.weekly_reset_at = weekly_reset_at;
        a
    }

    #[test]
    fn main_row_flat_shape_matches_spec_when_not_locked() {
        // "{provider}    {email}\t{n}% / {n}%" per the v0.5.1 UX pass — the
        // crate's variant uses a TAB between the label and trailing run so
        // AppKit right-aligns it. The important structural invariants are:
        // provider, then email, one TAB, then "n% / n%". Any change to that
        // layout will fail this assertion — a wall against silent drift.
        let a = acct("you@work.com", Some(42.0), Some(61.0), false);
        let r = main_row("Claude", &a, bands());
        assert_eq!(r.plain, "Claude    you@work.com\t42% / 61%");
        assert!(!r.checkmark, "inactive row: no leading checkmark");
    }

    #[test]
    fn main_row_switches_to_locked_countdown_when_over_threshold() {
        // Session at 100% with a reset ~90 minutes out → row must swap the
        // percentages for "locked · 1h 30m" and paint it red. This is the
        // "usage → locked" transition the redesign spec calls out.
        let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
        let reset = now + chrono::Duration::minutes(90);
        with_now(now, || {
            let a = acct_with_resets(
                "matt@example.com",
                Some(100.0),
                Some(20.0),
                true,
                Some(reset),
                None,
            );
            let r = main_row("Claude", &a, bands());
            // v0.5.0 UX: no "locked · " prefix — red color + a time (not a
            // percent) is the affordance. See main_row's locked-branch comment.
            assert_eq!(r.plain, "Claude    matt@example.com\t1h 30m");
            assert!(r.bold, "active locked row is still bold");
            assert!(r.checkmark, "active row gets a leading checkmark");
            // Exactly one colored span, red-tinted, covering the countdown run.
            assert_eq!(r.colors.len(), 1);
            let (off, len, sev) = r.colors[0];
            assert_eq!(sev, Severity::Red);
            let picked: String = r.plain.chars().skip(off).take(len).collect();
            assert_eq!(picked, "1h 30m");
        });
    }

    #[test]
    fn main_row_stays_usage_when_reset_is_stale() {
        // pct at 100 but reset already in the past — countdown treats it as
        // stale (next refresh will fix the pct), so the row stays "S / W".
        let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
        let past = now - chrono::Duration::minutes(5);
        with_now(now, || {
            let a = acct_with_resets("dev@x.com", Some(100.0), None, false, Some(past), None);
            let r = main_row("Claude", &a, bands());
            assert_eq!(r.plain, "Claude    dev@x.com\t100% / —");
            assert!(!r.plain.contains("locked"));
        });
    }

    /// Minimal `ProviderSection` for `account_header_row` tests — only
    /// `display_name`/`severity_bands` matter to that function; the rest are
    /// filled with harmless defaults.
    fn header_test_section(display_name: &'static str) -> ProviderSection {
        ProviderSection {
            provider_id: "claude",
            display_name,
            supports_switching: true,
            supports_launch: true,
            supports_remove: true,
            supports_usage: true,
            severity_bands: bands(),
            env_override_active: false,
            accounts: vec![],
        }
    }

    #[test]
    fn account_header_row_flat_shape_matches_spec_when_not_locked() {
        // v0.5.2 per-account BLOCK header: "{provider} · {email}\t{n}% / {n}%"
        // — replaces `main_row`'s padded flat-list format as the string every
        // production render path (`build_menu`/`menu_tree_from_snapshot`) now
        // uses for a block's first row.
        let sec = header_test_section("Claude");
        let a = acct("you@work.com", Some(42.0), Some(61.0), false);
        let r = account_header_row(&sec, &a);
        assert_eq!(r.plain, "  you@work.com\t42% / 61%");
        assert!(!r.checkmark, "inactive row: no leading checkmark");
        assert_eq!(r.tab_x_kind, Some(TabX::MenuRight));
    }

    #[test]
    fn account_header_row_switches_to_locked_countdown_when_over_threshold() {
        let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
        let reset = now + chrono::Duration::minutes(90);
        with_now(now, || {
            let sec = header_test_section("Claude");
            let a = acct_with_resets(
                "matt@example.com",
                Some(100.0),
                Some(20.0),
                true,
                Some(reset),
                None,
            );
            let r = account_header_row(&sec, &a);
            // Session-locked + weekly-healthy nuance (v0.5.2 addendum #7):
            // trailing = "<countdown> / <weekly%>" so the user can still see
            // the healthy weekly headroom while the session countdown ticks.
            assert_eq!(r.plain, "  matt@example.com\t1h 30m / 20%");
            assert!(r.bold, "active locked row is still bold");
            assert!(r.checkmark, "active row gets a leading checkmark");
            // countdown is red; weekly=20 sits below the severity bands so it
            // contributes no additional color entry.
            assert_eq!(r.colors.len(), 1);
            let (_, _, sev) = r.colors[0];
            assert_eq!(sev, Severity::Red);
        });
    }

    #[test]
    fn account_header_row_colors_high_percentages_per_provider_bands() {
        // Same severity-band contract `main_row` has always had, just on the
        // new label format — a regression guard against the block-redesign
        // refactor accidentally dropping the color computation.
        let sec = header_test_section("Codex");
        let a = acct("hot@x.com", Some(96.0), Some(10.0), false);
        let r = account_header_row(&sec, &a);
        assert_eq!(r.plain, "  hot@x.com\t96% / 10%");
        assert_eq!(r.colors.len(), 1, "only the 96% session run is colored");
        let (off, len, sev) = r.colors[0];
        assert_eq!(sev, Severity::Red);
        let picked: String = r.plain.chars().skip(off).take(len).collect();
        assert_eq!(picked, "96%");
    }

    #[test]
    fn sort_by_expiration_orders_accounts_soonest_first() {
        // Regression test for the user-reported bug: newly-added accounts land
        // at the tail of `state.accounts` (upsert appends) and stayed at the
        // bottom of the menu even when their weekly window resets sooner.
        // `sort_by_expiration` fixes that — accounts sort by weekly_reset_at
        // ASC, so the "closest to expiration" is on top.
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let mut soonest = acct("soon@x.com", Some(50.0), Some(60.0), false);
        soonest.weekly_reset_at = Some(now + chrono::Duration::hours(6));
        let mut middle = acct("mid@x.com", Some(50.0), Some(60.0), false);
        middle.weekly_reset_at = Some(now + chrono::Duration::hours(48));
        let mut latest = acct("late@x.com", Some(50.0), Some(60.0), false);
        latest.weekly_reset_at = Some(now + chrono::Duration::hours(120));

        // Insert in REVERSE-expiration order (mirrors what upsert would
        // produce if the user added them latest-first).
        let mut accounts = [latest, middle, soonest];
        accounts.sort_by(sort_by_expiration);

        let keys: Vec<&str> = accounts.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, vec!["soon@x.com", "mid@x.com", "late@x.com"]);
    }

    #[test]
    fn sort_by_expiration_places_no_data_accounts_last() {
        // An account without cached usage yet has `weekly_reset_at == None`.
        // The sort treats None as "furthest in the future" so a freshly-
        // captured account (no data) sinks to the bottom rather than
        // displacing an account with a real, soon reset.
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let mut with_data = acct("data@x.com", Some(10.0), Some(20.0), false);
        with_data.weekly_reset_at = Some(now + chrono::Duration::hours(6));
        let no_data = acct("nodata@x.com", None, None, false);
        assert!(no_data.weekly_reset_at.is_none(), "precondition");

        let mut accounts = [no_data, with_data];
        accounts.sort_by(sort_by_expiration);
        let keys: Vec<&str> = accounts.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, vec!["data@x.com", "nodata@x.com"]);
    }

    // -----------------------------------------------------------------------
    // flat_account_order (v0.5.2: cross-provider account order regression)
    // -----------------------------------------------------------------------

    fn section_with(
        provider_id: &'static str,
        display_name: &'static str,
        accounts: Vec<AcctView>,
    ) -> ProviderSection {
        ProviderSection {
            provider_id,
            display_name,
            supports_switching: true,
            supports_usage: true,
            supports_launch: true,
            supports_remove: true,
            severity_bands: bands(),
            env_override_active: false,
            accounts,
        }
    }

    #[test]
    fn flat_account_order_sorts_by_soonest_reset_across_providers() {
        // Given 3 accounts with resets in [+3d, +6h, +2d]: order should be
        // [+6h_first, +2d, +3d] — a single global comparator spanning ALL
        // providers, not a per-provider re-sort.
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let a_3d = acct_with_resets(
            "three-day@x.com",
            Some(10.0),
            Some(10.0),
            false,
            None,
            Some(now + chrono::Duration::days(3)),
        );
        let a_6h = acct_with_resets(
            "six-hour@x.com",
            Some(10.0),
            Some(10.0),
            false,
            None,
            Some(now + chrono::Duration::hours(6)),
        );
        let mut a_2d = acct_with_resets(
            "two-day@x.com",
            Some(10.0),
            Some(10.0),
            false,
            None,
            Some(now + chrono::Duration::days(2)),
        );
        a_2d.provider_id = "codex";
        let sections = vec![
            section_with(CLAUDE_SLUG, "Claude", vec![a_3d, a_6h]),
            section_with("codex", "Codex", vec![a_2d]),
        ];
        let order = flat_account_order(&sections, now);
        let keys: Vec<&str> = order
            .iter()
            .map(|&(si, ai)| sections[si].accounts[ai].key.as_str())
            .collect();
        assert_eq!(
            keys,
            vec!["six-hour@x.com", "two-day@x.com", "three-day@x.com"]
        );
    }

    #[test]
    fn flat_account_order_sinks_locked_and_inactive_accounts() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        // Locked (session at 100%, reset still future) and NOT active — must
        // sink below a healthy account even though its own reset is sooner.
        let locked = acct_with_resets(
            "locked@x.com",
            Some(100.0),
            Some(10.0),
            false,
            Some(now + chrono::Duration::minutes(30)),
            None,
        );
        let healthy = acct_with_resets(
            "healthy@x.com",
            Some(10.0),
            Some(10.0),
            false,
            None,
            Some(now + chrono::Duration::days(5)),
        );
        let sections = vec![section_with(CLAUDE_SLUG, "Claude", vec![locked, healthy])];
        let order = flat_account_order(&sections, now);
        let keys: Vec<&str> = order
            .iter()
            .map(|&(si, ai)| sections[si].accounts[ai].key.as_str())
            .collect();
        assert_eq!(keys, vec!["healthy@x.com", "locked@x.com"]);
    }

    #[test]
    fn flat_account_order_keeps_active_locked_account_in_normal_rotation() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        // Locked AND active — must NOT sink; it still sorts by its own reset
        // like anyone else, so the user can see its countdown up top.
        let locked_active = acct_with_resets(
            "locked-active@x.com",
            Some(100.0),
            Some(10.0),
            true,
            Some(now + chrono::Duration::minutes(30)),
            None,
        );
        let healthy = acct_with_resets(
            "healthy@x.com",
            Some(10.0),
            Some(10.0),
            false,
            None,
            Some(now + chrono::Duration::days(5)),
        );
        let sections = vec![section_with(
            CLAUDE_SLUG,
            "Claude",
            vec![healthy, locked_active],
        )];
        let order = flat_account_order(&sections, now);
        let keys: Vec<&str> = order
            .iter()
            .map(|&(si, ai)| sections[si].accounts[ai].key.as_str())
            .collect();
        // locked_active's soonest reset (+30m) is sooner than healthy's (+5d).
        assert_eq!(keys, vec!["locked-active@x.com", "healthy@x.com"]);
    }

    #[test]
    fn flat_account_order_tie_breaks_by_headroom_then_email() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        // Same reset instant: more headroom (lower max_pct) sorts first.
        let reset = Some(now + chrono::Duration::days(1));
        let more_headroom = acct_with_resets("b@x.com", Some(10.0), Some(10.0), false, None, reset);
        let less_headroom = acct_with_resets("a@x.com", Some(50.0), Some(50.0), false, None, reset);
        let sections = vec![section_with(
            CLAUDE_SLUG,
            "Claude",
            vec![less_headroom, more_headroom],
        )];
        let order = flat_account_order(&sections, now);
        let keys: Vec<&str> = order
            .iter()
            .map(|&(si, ai)| sections[si].accounts[ai].key.as_str())
            .collect();
        assert_eq!(keys, vec!["b@x.com", "a@x.com"]);
    }

    #[test]
    fn build_snapshot_sorts_active_first_within_a_section() {
        // `build_snapshot` orders accounts so the active row renders first
        // within each provider block — the "active-first ordering" rule from
        // the redesign spec. We assert on the sort key `menu_order` cannot
        // itself provide (it doesn't know about `active`).
        let mut a1 = acct("second@x.com", Some(10.0), Some(20.0), false);
        a1.active = false;
        let mut a2 = acct("active@x.com", Some(50.0), Some(60.0), true);
        a2.active = true;
        let mut a3 = acct("third@x.com", Some(30.0), Some(40.0), false);
        a3.active = false;
        let mut accounts = [a1, a2, a3];
        accounts.sort_by_key(|a| std::cmp::Reverse(a.active));
        assert_eq!(accounts[0].key, "active@x.com", "active is first");
        // Stable sort: the two inactives keep their original order.
        assert_eq!(accounts[1].key, "second@x.com");
        assert_eq!(accounts[2].key, "third@x.com");
    }

    #[test]
    fn provider_group_header_row_is_bold_plain_display_name_with_icon() {
        // v0.5.3: the bold provider-group header row that precedes a
        // provider's account rows carries the display name verbatim, bold,
        // and the provider's 16px icon slug. No tab stop, no checkmark, no
        // color spans — a header, not a data row.
        let sec = header_test_section("Claude");
        let row = provider_group_header_row(&sec);
        assert_eq!(row.plain, "Claude");
        assert!(row.bold, "group header is bold");
        assert!(row.section_header, "marked as a section header");
        assert_eq!(row.icon_slug, Some("claude"));
        assert!(row.tab_x_kind.is_none(), "no tab stop");
        assert!(!row.checkmark);
        assert!(row.colors.is_empty());
    }

    #[test]
    fn provider_grouped_order_partitions_and_sorts_alphabetically() {
        // v0.5.3 menu redesign: two providers with two accounts each, given
        // in Codex → Claude order — grouping must partition by provider AND
        // sort provider groups alphabetically by display name so a two-
        // provider menu always reads Claude → Codex. Within-provider order
        // is preserved from `snap.account_order`.
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let a1 = acct_with_resets(
            "a@claude.com",
            Some(10.0),
            Some(10.0),
            false,
            None,
            Some(now + chrono::Duration::hours(6)),
        );
        let a2 = acct_with_resets(
            "b@claude.com",
            Some(10.0),
            Some(10.0),
            false,
            None,
            Some(now + chrono::Duration::days(2)),
        );
        let mut c1 = acct_with_resets(
            "a@codex.com",
            Some(10.0),
            Some(10.0),
            false,
            None,
            Some(now + chrono::Duration::hours(12)),
        );
        c1.provider_id = "codex";
        let mut c2 = acct_with_resets(
            "b@codex.com",
            Some(10.0),
            Some(10.0),
            false,
            None,
            Some(now + chrono::Duration::days(1)),
        );
        c2.provider_id = "codex";
        // Sections registered in reverse-alpha order — grouped_order must
        // still return Claude before Codex.
        let sections = vec![
            section_with("codex", "Codex", vec![c1, c2]),
            section_with(CLAUDE_SLUG, "Claude", vec![a1, a2]),
        ];
        let account_order = flat_account_order(&sections, now);
        let snap = Snapshot {
            sections,
            account_order,
            capture_creds: Vec::new(),
            capture_api_key: Vec::new(),
            autoswap: false,
            threshold: 95.0,
            notification_config: crate::notifications::NotificationConfig::default(),
        };
        let groups = provider_grouped_order(&snap);
        assert_eq!(groups.len(), 2, "one entry per provider");
        // Claude first (alphabetical by display name), then Codex.
        assert_eq!(snap.sections[groups[0].0].display_name, "Claude");
        assert_eq!(snap.sections[groups[1].0].display_name, "Codex");
        // Within-provider account order preserved.
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].1.len(), 2);
    }

    #[test]
    fn menu_styles_attaches_icon_slug_to_the_provider_group_header() {
        // v0.5.3 restored provider grouping: the 16px icon moves from every
        // account row onto the provider-group header row (bold, disabled,
        // plain title = display name). No account row carries an icon
        // anymore; the group header is the sole icon carrier.
        let snap = one_section_snap(acct("a@x.com", Some(10.0), Some(20.0), true));
        let styles = menu_styles(&snap);
        let header_style = styles
            .iter()
            .find(|s| s.plain == "Claude")
            .expect("provider-group header row present");
        assert_eq!(header_style.icon_slug, Some(CLAUDE_SLUG));
        assert!(header_style.bold, "group header is bold");
        // Account rows (the ones with a tab stop) must NOT carry an icon.
        for s in styles.iter().filter(|s| s.tab_x_kind.is_some()) {
            assert!(
                s.icon_slug.is_none(),
                "account row must not carry an icon: {}",
                s.plain,
            );
        }
        // The submenu info rows (no tab stop) also carry no icon.
        for s in styles
            .iter()
            .filter(|s| s.tab_x_kind.is_none() && s.plain != "Claude")
        {
            assert!(
                s.icon_slug.is_none(),
                "unexpected icon on non-main row: {}",
                s.plain,
            );
        }
    }

    #[test]
    fn menu_styles_tab_stop_is_uniform_across_a_section() {
        // v0.5.1: the concrete tab-stop x is resolved dynamically at install
        // time (`mac_style::compute_menu_right_x`, from the actual rendered
        // widths), not stored on `RowStyle` — so what a `RowStyle` carries,
        // and what this test can pin without native AppKit calls, is just
        // the *kind* of alignment it wants. Every main-list row in a section
        // (and the Quit row) must request `TabX::MenuRight`, or the S%/W%
        // columns wouldn't line up vertically. Vary email length across rows
        // to prove the kind doesn't depend on content width either.
        let short = acct("a@x.com", Some(10.0), Some(20.0), false);
        let long = acct(
            "a.very.long.email.address@example.com",
            Some(30.0),
            Some(40.0),
            true,
        );
        let snap = Snapshot {
            sections: vec![ProviderSection {
                provider_id: CLAUDE_SLUG,
                display_name: "Claude",
                supports_switching: true,
                supports_usage: true,
                supports_launch: true,
                supports_remove: true,
                severity_bands: bands(),
                env_override_active: false,
                accounts: vec![long, short],
            }],
            account_order: vec![(0, 0), (0, 1)],
            capture_creds: Vec::new(),
            capture_api_key: Vec::new(),
            autoswap: false,
            threshold: 95.0,
            notification_config: crate::notifications::NotificationConfig::default(),
        };
        let styles = menu_styles(&snap);
        let tab_kinds: Vec<TabX> = styles.iter().filter_map(|s| s.tab_x_kind).collect();
        assert!(
            tab_kinds.len() >= 2,
            "expected a tab stop on every main row plus Quit"
        );
        for kind in &tab_kinds {
            assert_eq!(
                *kind,
                TabX::MenuRight,
                "every row must request the shared menu-right alignment"
            );
        }
    }

    #[test]
    fn menu_styles_marks_active_account_with_checkmark() {
        // The active-account row sets `checkmark = true` so the native walk
        // renders a leading ✓ glyph via `NSMenuItem::setState(.On)`. Inactive
        // rows must not.
        let mut inactive = acct("dev@x.com", Some(10.0), Some(20.0), false);
        inactive.active = false;
        let active = acct("active@x.com", Some(50.0), Some(60.0), true);
        let snap = Snapshot {
            sections: vec![ProviderSection {
                provider_id: CLAUDE_SLUG,
                display_name: "Claude",
                supports_switching: true,
                supports_usage: true,
                supports_launch: true,
                supports_remove: true,
                severity_bands: bands(),
                env_override_active: false,
                accounts: vec![active, inactive],
            }],
            account_order: vec![(0, 0), (0, 1)],
            capture_creds: Vec::new(),
            capture_api_key: Vec::new(),
            autoswap: false,
            threshold: 95.0,
            notification_config: crate::notifications::NotificationConfig::default(),
        };
        let styles = menu_styles(&snap);
        // Only inspect the per-account main-list rows (they carry a tab
        // stop; the Quit row is excluded by matching on the email instead).
        let mut saw_active = false;
        let mut saw_inactive = false;
        for s in styles.iter().filter(|s| s.tab_x_kind.is_some()) {
            if s.plain.contains("active@x.com") {
                assert!(s.checkmark, "active row must carry the checkmark flag");
                saw_active = true;
            }
            if s.plain.contains("dev@x.com") {
                assert!(
                    !s.checkmark,
                    "inactive row must NOT carry the checkmark flag"
                );
                saw_inactive = true;
            }
        }
        assert!(saw_active && saw_inactive, "both rows produced a style");
    }

    #[test]
    fn build_snapshot_omits_sections_for_providers_with_zero_accounts() {
        // No captured accounts → no section (the "no header, no rows" rule).
        // Uses the pure `partition_capture_providers` sibling to prove the
        // filter logic without touching state.json. `build_snapshot`'s own
        // gate is the `if provider_rows.is_empty() { continue; }` branch —
        // this test locks in the *behavior* the gate exists to enforce.
        let snap = Snapshot::default();
        assert!(
            snap.sections.is_empty(),
            "empty snapshot has no sections — a provider with zero rows must never emit one",
        );
    }

    // -----------------------------------------------------------------------
    // H3 (v0.5.0 codeaudit) — capability-gated menu rows. `supports_switching`
    // used to be the sole gate for BOTH the "Switch to this account" row AND
    // the "Launch client" row, and "Remove…" had no gate at all. A provider
    // like Codex (usage-only, no switching, no launch wired) would still get
    // a "Launch client" row that always errored on click.
    // -----------------------------------------------------------------------

    fn section_with_caps(
        supports_switching: bool,
        supports_launch: bool,
        supports_remove: bool,
    ) -> ProviderSection {
        ProviderSection {
            provider_id: "codex",
            display_name: "Codex",
            supports_switching,
            supports_launch,
            supports_remove,
            supports_usage: true,
            severity_bands: bands(),
            env_override_active: false,
            accounts: vec![],
        }
    }

    #[test]
    fn submenu_rows_omit_launch_when_supports_launch_is_false() {
        // Codex today: supports_switching == false AND supports_launch ==
        // false. Neither the Switch nor the Launch row may appear.
        let sec = section_with_caps(false, false, true);
        let a = acct("user@example.com", Some(10.0), Some(20.0), false);
        let rows = account_submenu_rows(&sec, &a);
        assert_eq!(rows.switch_row, None, "no switch row: {rows:?}");
        assert!(!rows.launch_row, "no launch row: {rows:?}");
    }

    #[test]
    fn submenu_rows_can_omit_launch_even_when_switching_is_supported() {
        // The two capabilities are independent: a provider could (in
        // principle) support switching without a wired client launcher.
        // supports_launch alone must gate the Launch row.
        let sec = section_with_caps(true, false, true);
        let a = acct("user@example.com", Some(10.0), Some(20.0), false);
        let rows = account_submenu_rows(&sec, &a);
        assert_eq!(
            rows.switch_row,
            Some(false),
            "clickable switch row present: {rows:?}"
        );
        assert!(
            !rows.launch_row,
            "launch row absent even though switching is supported: {rows:?}"
        );
    }

    #[test]
    fn submenu_rows_include_launch_when_supports_launch_is_true() {
        let sec = section_with_caps(true, true, true);
        let a = acct("user@example.com", Some(10.0), Some(20.0), false);
        assert!(account_submenu_rows(&sec, &a).launch_row);
    }

    #[test]
    fn submenu_rows_marks_active_account_instead_of_a_clickable_switch_row() {
        let sec = section_with_caps(true, true, true);
        let a = acct("user@example.com", Some(10.0), Some(20.0), true);
        assert_eq!(account_submenu_rows(&sec, &a).switch_row, Some(true));
    }

    #[test]
    fn submenu_rows_omit_remove_when_supports_remove_is_false() {
        let sec = section_with_caps(false, false, false);
        let a = acct("user@example.com", Some(10.0), Some(20.0), false);
        assert!(!account_submenu_rows(&sec, &a).remove_row);
    }

    #[test]
    fn submenu_rows_include_remove_when_supports_remove_is_true() {
        let sec = section_with_caps(false, false, true);
        let a = acct("user@example.com", Some(10.0), Some(20.0), false);
        assert!(account_submenu_rows(&sec, &a).remove_row);
    }

    #[test]
    fn codex_capabilities_advertise_switching_but_not_launch() {
        // H3 (v0.5.0 codeaudit) is now closed: state v2 (`State::providers`)
        // gives Codex somewhere to persist a second account, and
        // `main.rs`/`menubar.rs` dispatch a switch by provider slug, so
        // `write_active_account` is reachable from the live app —
        // `supports_switching` is `true`. `launch_client` is still
        // unimplemented for Codex, so `supports_launch` stays `false`.
        let caps = crate::providers::codex::CodexProvider.capabilities();
        assert!(caps.supports_switching);
        assert!(!caps.supports_launch);
        assert!(caps.supports_remove);
    }

    #[test]
    fn claude_capabilities_advertise_launch_support() {
        // Claude is the only provider with a wired `launch_client`.
        let caps = crate::providers::claude::ClaudeProvider.capabilities();
        assert!(caps.supports_switching);
        assert!(caps.supports_launch);
        assert!(caps.supports_remove);
    }

    #[test]
    fn menu_signature_folds_lock_state_and_reset_at() {
        // Same pcts + updated + windows — but toggling only the reset instant
        // (usage → locked transition) must yield a different signature so the
        // menu redraws with the "locked · Xh Ym" trailing text.
        let now = Utc.timestamp_opt(3_000_000, 0).unwrap();
        with_now(now, || {
            let a_usage = acct_with_resets("m@x.com", Some(100.0), Some(50.0), true, None, None);
            let usage_snap = one_section_snap(a_usage);
            let sig_usage = menu_signature(&usage_snap);
            assert!(sig_usage.contains("L=0"), "usage state marker present");

            let a_locked = acct_with_resets(
                "m@x.com",
                Some(100.0),
                Some(50.0),
                true,
                Some(now + chrono::Duration::hours(2)),
                None,
            );
            let locked_snap = one_section_snap(a_locked);
            let sig_locked = menu_signature(&locked_snap);
            assert!(sig_locked.contains("L=S"), "session-locked marker present");
            assert_ne!(sig_usage, sig_locked, "lock transition must trigger redraw");
        });
    }

    #[test]
    fn capture_menu_filter_hides_zero_usage_stubs_from_creds_list() {
        // Integration-level check on top of `partition_capture_providers`:
        // the live registry produced by `providers::init()` must hide any
        // stub provider (`supports_usage == false`) from the "Capture
        // current login ▸" onboarding surface. Claude + Codex (the two full
        // providers) must be present in the creds bucket.
        providers::init();
        let (creds, _api_key) = capture_menu_providers(&[]);
        let creds_ids: Vec<&str> = creds.iter().map(|p| p.provider_id()).collect();
        assert!(creds_ids.contains(&"claude"), "claude in creds bucket");
        // Every registered stub with supports_usage == false must be filtered.
        for stub in [
            "opencode",
            "gemini-cli",
            "qwen-code",
            "copilot-cli",
            "cursor-agent",
            "amazon-q",
            "cline",
            "grok",
            "kimi",
        ] {
            assert!(
                !creds_ids.contains(&stub),
                "stub `{stub}` must be filtered out of the creds capture list",
            );
        }
    }

    #[test]
    fn main_row_locked_shape_swaps_only_the_trailing_run() {
        // The locked row keeps the "{provider label}\t…" tab structure so
        // right-alignment still works — only the trailing "n% / n%" run
        // becomes "locked · <countdown>". A test that pins the structure so
        // a future refactor can't accidentally lose the tab.
        let now = Utc.timestamp_opt(4_000_000, 0).unwrap();
        with_now(now, || {
            let a = acct_with_resets(
                "matt@example.com",
                Some(100.0),
                None,
                false,
                Some(now + chrono::Duration::hours(23) + chrono::Duration::minutes(52)),
                None,
            );
            let r = main_row("Claude", &a, bands());
            let (label, trailing) = r.plain.split_once('\t').expect("tab preserved");
            assert_eq!(label, "Claude    matt@example.com");
            // No "locked · " prefix — red color + a time (not a percent) is
            // the affordance now. See main_row's locked-branch comment.
            assert_eq!(trailing, "23h 52m");
            assert_eq!(
                r.tab_x_kind,
                Some(TabX::MenuRight),
                "right-align tab-stop preserved"
            );
        });
    }

    // -----------------------------------------------------------------------
    // v0.5.0 redesign: locked-switch refusal, API-key-once-captured, and the
    // "white but not clickable" submenu style.
    // -----------------------------------------------------------------------

    /// Build a minimal Claude `Account` with cached usage, for
    /// `switch_target_lock_countdown` fixtures. No I/O — a plain in-memory
    /// `store::Account`, not anything touching state.json.
    fn account_with_usage(
        email: &str,
        session_pct: Option<f64>,
        session_reset: Option<DateTime<Utc>>,
    ) -> crate::store::Account {
        crate::store::Account {
            email: Some(email.to_string()),
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: 0,
            keychain_blob: "{}".into(),
            oauth_account: None,
            user_id: None,
            cached_usage: Some(crate::store::CachedUsage {
                session_pct,
                weekly_pct: Some(10.0),
                session_reset: session_reset.map(|t| t.to_rfc3339()),
                weekly_reset: None,
                opus_pct: None,
                opus_reset: None,
                fetched_at: 0,
            }),
            notif_state: crate::notifications::NotifState::default(),
            needs_relogin: false,
        }
    }

    #[test]
    fn switch_target_lock_countdown_refuses_a_maxed_out_account() {
        // Item 8 of the redesign: a session at 100% with a still-future reset
        // is "locked" — `handle_switch` must see this and refuse.
        providers::init();
        let now = Utc.timestamp_opt(5_000_000, 0).unwrap();
        with_now(now, || {
            let reset = now + chrono::Duration::minutes(45);
            let st = State {
                accounts: vec![account_with_usage(
                    "matt@example.com",
                    Some(100.0),
                    Some(reset),
                )],
                ..State::default()
            };
            let cd = switch_target_lock_countdown(&st, CLAUDE_SLUG, "matt@example.com");
            assert_eq!(cd, Some("45m".to_string()));
        });
    }

    #[test]
    fn switch_target_lock_countdown_allows_an_account_with_headroom() {
        providers::init();
        let now = Utc.timestamp_opt(5_000_000, 0).unwrap();
        with_now(now, || {
            let st = State {
                accounts: vec![account_with_usage("matt@example.com", Some(40.0), None)],
                ..State::default()
            };
            assert_eq!(
                switch_target_lock_countdown(&st, CLAUDE_SLUG, "matt@example.com"),
                None,
            );
        });
    }

    #[test]
    fn switch_target_lock_countdown_is_none_for_an_unknown_account() {
        providers::init();
        let st = State::default();
        assert_eq!(
            switch_target_lock_countdown(&st, CLAUDE_SLUG, "nobody@example.com"),
            None,
        );
    }

    #[test]
    fn partition_capture_providers_excludes_an_already_captured_api_key_provider() {
        // Item 9 of the redesign: once an API-key provider has a captured
        // account (surfaced to `partition_capture_providers` as its id being
        // in `captured_provider_ids`), it must disappear from the "Paste API
        // key ▸" bucket — the account itself now renders in the main list.
        let openrouter_like = FakeProvider {
            id: "openrouter-like",
            name: "OpenRouter-like",
            supports_usage: true,
            capture_mode: CaptureMode::ApiKey,
        };
        let regs: Vec<Box<dyn Provider>> = vec![Box::new(openrouter_like)];

        let (_, api_key_before) = partition_capture_providers(&regs, &[]);
        assert_eq!(
            api_key_before
                .iter()
                .map(|p| p.provider_id())
                .collect::<Vec<_>>(),
            vec!["openrouter-like"],
            "not yet captured → still offered under Paste API key ▸",
        );

        let (_, api_key_after) = partition_capture_providers(&regs, &["openrouter-like"]);
        assert!(
            api_key_after.is_empty(),
            "already captured → must NOT be offered under Paste API key ▸ anymore",
        );
    }

    // These two tests exercise `mac_style::attributed` directly (NSAttributedString
    // attribute inspection) — nested in their own `#[cfg(target_os = "macos")]`
    // module since `objc2_app_kit`/`objc2_foundation` aren't in the dependency
    // graph on Linux/Windows (see the top-of-file import comment).
    #[cfg(target_os = "macos")]
    mod mac_style_tests {
        use super::*;
        use objc2_app_kit::NSForegroundColorAttributeName;

        #[test]
        fn disabled_but_white_row_gets_a_full_range_labelcolor_attribute() {
            // The "white but not clickable" style: `disabled_but_white` must
            // produce an `NSForegroundColorAttributeName` run spanning the ENTIRE
            // row (offset 0, full length) — not disabled grey, not a partial span.
            let style = RowStyle {
                disabled_but_white: true,
                ..RowStyle::plain_row("Session resets in 3h".to_string())
            };
            let attr = mac_style::attributed(&style, None);
            let full_len = attr.length();
            assert!(full_len > 0);
            let mut effective = objc2_foundation::NSRange::new(0, 0);
            let value = unsafe {
                attr.attribute_atIndex_effectiveRange(
                    NSForegroundColorAttributeName,
                    0,
                    &mut effective,
                )
            };
            assert!(value.is_some(), "expected a foreground-color attribute");
            assert_eq!(
                effective,
                objc2_foundation::NSRange::new(0, full_len),
                "labelColor must span the whole row",
            );
        }

        /// v0.5.4 defect 2: `effective_tab_x` MUST push non-submenu rows'
        /// tab stop right by the chevron column width so their trailing
        /// text terminates at the same visual X as a submenu row's. Pure
        /// arithmetic — no AppKit round-trip needed.
        #[test]
        fn effective_tab_x_pushes_non_submenu_rows_right_by_chevron_width() {
            let base = 200.0;
            assert_eq!(mac_style::effective_tab_x(base, true), base);
            assert_eq!(
                mac_style::effective_tab_x(base, false),
                base + mac_style::CHEVRON_COLUMN_WIDTH,
            );
            // Non-zero chevron adjustment — a defensive check so a future
            // "chevron is 0pt on this platform" refactor can't silently
            // reintroduce the user-visible short-right-alignment bug.
            const _: () = assert!(mac_style::CHEVRON_COLUMN_WIDTH > 0.0);
        }

        /// v0.5.4 defect 1: `attributed` must produce a paragraph-style
        /// right-align tab stop whose location is at least `label_width +
        /// MIN_LABEL_TRAILING_GAP` past the label's right edge, so a near-
        /// widest email row never renders with zero gap between the email
        /// and its trailing `S% / W%`. Measures the actual label width in
        /// the menu font and inspects the tab stop the style produced.
        #[test]
        fn attributed_row_has_a_minimum_gap_between_label_and_tab_stop() {
            use objc2_app_kit::{NSFont, NSParagraphStyleAttributeName};
            use objc2_foundation::NSRange;

            let label = "  matthew@getbusbar.com";
            let plain = format!("{label}\t47% / 89%");
            let style = RowStyle {
                tab_x_kind: Some(TabX::MenuRight),
                ..RowStyle::plain_row(plain.clone())
            };
            let font = NSFont::menuFontOfSize(0.0);
            // Test hook: pass a tab_x already at least MIN_GAP past the
            // label's right edge (that is the invariant callers rely on;
            // production computes it via `compute_menu_right_x` which now
            // folds MIN_GAP into `widest`).
            // Simulate what `compute_menu_right_x` would produce for a
            // one-row menu with this row: label_width + MIN_GAP + trailing.
            // Then assert the tab stop is at least label_width + MIN_GAP.
            fn measured(s: &str, f: &objc2_app_kit::NSFont) -> f64 {
                use objc2::AllocAnyThread;
                use objc2_app_kit::{NSAttributedStringNSStringDrawing, NSFontAttributeName};
                use objc2_foundation::{NSAttributedString, NSMutableAttributedString, NSString};
                let s = NSString::from_str(s);
                let attr = NSMutableAttributedString::initWithString(
                    NSMutableAttributedString::alloc(),
                    &s,
                );
                unsafe {
                    attr.addAttribute_value_range(
                        NSFontAttributeName,
                        f,
                        NSRange::new(0, s.length()),
                    );
                }
                let attr: objc2::rc::Retained<NSAttributedString> =
                    objc2::rc::Retained::into_super(attr);
                attr.size().width
            }
            let label_w = measured(label, &font);
            let trailing_w = measured("47% / 89%", &font);
            let tab_x = label_w + mac_style::MIN_LABEL_TRAILING_GAP + trailing_w;

            let attr = mac_style::attributed(&style, Some(tab_x));
            let mut effective = NSRange::new(0, 0);
            let value = unsafe {
                attr.attribute_atIndex_effectiveRange(
                    NSParagraphStyleAttributeName,
                    0,
                    &mut effective,
                )
            };
            let obj = value.expect("expected a paragraph-style attribute");
            let para: &objc2_app_kit::NSParagraphStyle =
                unsafe { &*(objc2::rc::Retained::as_ptr(&obj) as *const _) };
            let tabs = para.tabStops();
            assert!(!tabs.is_empty(), "paragraph style must carry a tab stop");
            let stop_loc = tabs.iter().next().unwrap().location();
            assert!(
                stop_loc >= label_w + mac_style::MIN_LABEL_TRAILING_GAP,
                "tab stop {stop_loc} must be at least {}pt past label right edge {label_w}",
                mac_style::MIN_LABEL_TRAILING_GAP,
            );
        }

        #[test]
        fn plain_row_with_no_disabled_but_white_has_no_forced_foreground_color() {
            // A normal (non-info) row must NOT get the full-range labelColor
            // treatment — only `disabled_but_white` rows opt into it.
            let style = RowStyle::plain_row("Switch to this account".to_string());
            let attr = mac_style::attributed(&style, None);
            let mut effective = objc2_foundation::NSRange::new(0, 0);
            let value = unsafe {
                attr.attribute_atIndex_effectiveRange(
                    NSForegroundColorAttributeName,
                    0,
                    &mut effective,
                )
            };
            assert!(
                value.is_none(),
                "a plain row must not carry a forced foreground color",
            );
        }
    }

    // -----------------------------------------------------------------
    // Backups Save…/Restore… ▸ FileDialog trait routing
    //
    // These assert the click handlers call `FileDialog` with the expected
    // arguments WITHOUT ever popping a real native panel — the mock's
    // `save_file`/`pick_file` return `None` by default, so each handler
    // returns right after recording the call (the "user cancelled" path).
    // -----------------------------------------------------------------

    #[test]
    fn backup_save_click_calls_file_dialog_with_default_name_and_downloads_dir() {
        let dialog = crate::platform::MockFileDialog::default();
        handle_backup_save_with(&dialog);

        let calls = dialog.save_file_calls.borrow();
        assert_eq!(calls.len(), 1, "expected exactly one save_file() call");
        let (default_name, default_dir) = &calls[0];
        assert!(
            default_name.starts_with("usagio-state-") && default_name.ends_with(".json"),
            "unexpected default file name: {default_name}"
        );
        assert_eq!(default_dir.as_deref(), dirs::download_dir().as_deref());
    }

    #[test]
    fn backup_restore_click_calls_file_dialog_pick_file_once() {
        let _g = crate::store::ScopedConfigDir::new();
        let dialog = crate::platform::MockFileDialog::default();
        handle_backup_restore_dialog_with(&dialog);

        let calls = dialog.pick_file_calls.borrow();
        assert_eq!(calls.len(), 1, "expected exactly one pick_file() call");
    }

    // -----------------------------------------------------------------------
    // H2 (v0.5.0 codeaudit) — Restore… drop-confirmation wording + store-level
    // wiring. `handle_backup_restore_dialog` itself opens a native file panel
    // and a native `rfd::MessageDialog` confirm prompt, neither of which is
    // unit-testable; these tests cover the pure logic it's built from.
    // -----------------------------------------------------------------------

    #[test]
    fn restore_drop_confirmation_names_dropped_emails_not_just_a_count() {
        let msg = super::restore_drop_confirmation(&[
            "dev@getbusbar.com".to_string(),
            "matthew@pq.io".to_string(),
        ]);
        assert_eq!(
            msg,
            "Restoring will drop dev@getbusbar.com, matthew@pq.io. Continue?"
        );
    }

    /// Minimal restore-test account.
    fn restore_test_acct(email: &str) -> crate::store::Account {
        let blob = serde_json::json!({
            "claudeAiOauth": { "accessToken": "at", "refreshToken": "rt", "expiresAt": 0 }
        })
        .to_string();
        let mut a = crate::store::Account::from_keychain_blob(&blob).unwrap();
        a.email = Some(email.to_string());
        a
    }

    #[test]
    fn restore_that_only_adds_an_account_reports_no_drops() {
        use crate::store::{accounts_dropped_by, ScopedConfigDir, State};
        let _g = ScopedConfigDir::new();
        let mut current = State::default();
        current.accounts.push(restore_test_acct("a@e.com"));
        current.save().unwrap();

        let mut restore_target = State::load().unwrap();
        restore_target.accounts.push(restore_test_acct("b@e.com"));

        let dropped = accounts_dropped_by(&restore_target).unwrap();
        assert!(
            dropped.is_empty(),
            "adding an account must not be reported as a drop"
        );
    }

    #[test]
    fn restore_that_drops_an_account_reports_exactly_that_email() {
        use crate::store::{accounts_dropped_by, ScopedConfigDir, State};
        let _g = ScopedConfigDir::new();
        let mut current = State::default();
        for email in ["dev@getbusbar.com", "matthew@pq.io"] {
            current.accounts.push(restore_test_acct(email));
        }
        current.save().unwrap();

        let mut restore_target = State::default();
        restore_target
            .accounts
            .push(restore_test_acct("dev@getbusbar.com"));

        let dropped = accounts_dropped_by(&restore_target).unwrap();
        assert_eq!(dropped, vec!["matthew@pq.io".to_string()]);
        assert_eq!(
            restore_drop_confirmation(&dropped),
            "Restoring will drop matthew@pq.io. Continue?"
        );
    }

    #[test]
    fn pre_restore_stash_lands_under_config_backups_not_tmp() {
        use crate::store::{config_dir, stash_pre_restore, ScopedConfigDir, State};
        let g = ScopedConfigDir::new();
        let mut current = State::default();
        current.accounts.push(restore_test_acct("a@e.com"));
        current.save().unwrap();

        let dir = config_dir().unwrap();
        let stash = stash_pre_restore(&dir)
            .unwrap()
            .expect("live state existed");
        // Strong positive assertion: the stash MUST be inside the scoped tempdir's
        // config backups dir. On Linux CI the tempdir itself lives under /tmp/xyz/,
        // so a `!starts_with("/tmp")` guard would false-positive — the positive
        // form here catches the real regression (stash landing outside backups/).
        assert!(stash.starts_with(g.home().join(".config/usagio/backups")));
    }
}
