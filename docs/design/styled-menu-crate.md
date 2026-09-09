# Design: `flyout` — a standalone, fully-styleable cross-platform tray/context-menu crate

Status: **proposal / for maintainer review.** Design + architecture + API only —
no code is changed by it.

This document **supersedes and generalizes** `docs/design/custom-tray-popup.md`.
That earlier doc solved the problem *inside usagio* (a macOS `NSPopover` wired to
`build_snapshot`); it carries hard-won, still-correct platform findings which are
cited throughout here. This doc lifts that work into a **reusable, publishable
crate** — provisionally named **`flyout`** (see §9) — that usagio consumes like
any other dependency, and that anyone else can drop into their own tray app.

---

## 1. Scope & non-goals

### What the crate does

- Renders **styled, custom-drawn popup menus** anchored to a tray/status-bar
  icon, or to an arbitrary screen point (for true context menus).
- Gives the consumer **full control of presentation**: per-item alignment
  (left / right / center and multi-column `label ......… value` rows),
  foreground/background colors (literal or semantic/system), fonts
  (family / size / weight), leading & trailing icons/logos (from PNG or SVG
  bytes), enabled/checked state, separators, section headers, and **nested
  submenus**.
- Exposes a **clean declarative builder API** plus a click model (by id string
  and/or closure), independent of how any one backend draws pixels.
- Follows **system dark/light appearance and accent color**, is **HiDPI-crisp**,
  and appears with **no perceptible delay** on open.
- Works on **macOS and Windows** with a styled custom surface; on **Linux** it
  honestly degrades (see §4 — the crux).

### What the crate explicitly does NOT do

- It is **not a general GUI framework.** No layout engine for arbitrary widget
  trees, no forms, no text input fields, no tabs/scroll-views-of-anything. It
  draws *menus* (lists of rows, possibly nested), full stop.
- It is **not a replacement for application windows**, dialogs, or preference
  panes. Text entry (e.g. paste-an-API-key) belongs in a normal window the host
  app owns — a non-activating popup deliberately can't host a first-responder
  text field (see §4, macOS).
- It does **not own the tray icon glyph** itself. The consumer keeps using
  `tray-icon` (or the OS API) for the status-bar image/title; `flyout` only
  needs the *anchor rectangle* and the *open* trigger from it.
- It is **not** a fork of `muda`. `muda` has no drawable layer to fork — it is a
  thin data-model synchronizer over native OS-drawn menu objects
  (`NSMenu` / GTK `MenuItem` / Win32 `HMENU`); the pixels are drawn by AppKit /
  GTK / Win32 USER. To get custom drawing you must bypass native menus entirely,
  which is what this crate does. (Confirmed against the muda source and the
  prior-doc analysis in `custom-tray-popup.md §3a`.)

---

## 2. Rendering architecture — recommendation

> **RECOMMENDED: Approach (c), the hybrid.** One public API over two backends:
> a **native AppKit backend on macOS** (a generalization of usagio's in-flight
> `NSPopover` prototype) and a **portable CPU-raster backend — `winit` +
> `softbuffer` + `tiny-skia` — on Windows** (and, where geometry allows, Linux).
> Native where native clearly wins; one portable drawer everywhere else.

### The decisive constraints, and how each option scores

A tray-menu crate has an unusual constraint set: it must be **small**, **start
instantly** (the popup must appear the moment the icon is clicked — no GPU warm-up
stutter), be **HiDPI-crisp**, follow **system dark/light + accent**, and be
**accessible**. Those rank differently than for a normal app.

| Option | Binary | First-open latency | Text/theming control | A11y | Code cost |
|---|---|---|---|---|---|
| **(a) one cross-platform toolkit** (egui / iced / slint / winit+wgpu) | medium–large | GPU backends pay a one-time device/surface init on first show (~0.5s+, cold drivers worse — `gfx-rs/wgpu#6155`); CPU/`tiny_skia` backends avoid it | good, but you re-draw everything and re-derive theme | egui/iced lean on **AccessKit** (egui integrates it; native APIs on Win/macOS, enabled by default in `eframe`); weaker than native | 1× |
| **(b) native per-OS** (AppKit + GTK + Win32, all three) | smallest | instant everywhere | full native fidelity, free theming | **free** (native a11y trees) | **3×** — three API models, three text/layout/hit-test/dismiss impls |
| **(c) hybrid: native macOS + one portable backend elsewhere** | small | **instant** (AppKit = no GPU init; `tiny-skia` = CPU raster, no GPU init) | native fidelity on mac; full manual control on the raster path | free on mac; **AccessKit** on the raster path | ~1.6× |

Why **not (a) alone:** the single-toolkit dream doesn't actually unify the hard
part. The hard part isn't *drawing rows* — it's **anchoring, non-activating
focus behavior, transient dismiss, multi-monitor, and accessibility**, and those
are per-OS regardless of which toolkit draws the pixels. A cross-platform toolkit
still needs a macOS special-case (non-activating `NSPanel`), a Windows
special-case (`WS_EX_NOACTIVATE`), and — critically — a **Linux special-case that
no toolkit can paper over** (§4). You pay the toolkit's binary/latency tax and
*still* write the per-OS plumbing. And on macOS specifically you'd be *throwing
away* native `NSPopover`, which solves anchoring + transient-dismiss +
multi-monitor + dark-mode + a11y **for free**.

- **Slint** is additionally disqualified by licensing: tri-license GPLv3 / paid —
  incompatible with usagio's MIT and with a permissive open-source crate.
- **Tauri/webview** is the worst latency fit (webview cold start / WebView2
  first-launch) for a popup that must feel instant.
- **winit + wgpu** carries GPU-init latency; mitigable by pre-warming a hidden
  window, but that's a workaround for a cost the CPU raster path simply doesn't
  have.

Why **not (b) alone:** full native on all three is 3× the drawing / layout /
hit-testing / dismiss / theming code in three different API dialects. usagio is a
small tool; an open-source crate wants a small, reviewable surface. Unjustified
when only **one** platform (macOS) carries the "must look beautiful" mandate and
Linux can't be anchored at all.

Why **(c):** it spends the native-code budget exactly where it pays off.
- **macOS** gets a native AppKit backend: the in-flight `NSPopover` prototype
  (`src/ui/popover.rs`) **is** this backend in embryo. AppKit gives anchoring,
  `.transient` dismiss, multi-monitor, dark/light + accent, HiDPI, and
  `NSAccessibility` essentially for free; zero GPU init → instant.
- **Windows** gets the portable `winit + softbuffer + tiny-skia` backend: a
  borderless `WS_EX_NOACTIVATE` layered window anchored via the tray rect,
  drawing a shared "scene" on the CPU. Smallest binary, no GPU init,
  full pixel control. `tiny-skia` is explicitly designed as a minimal CPU-only
  2D library optimized for binary size and quality (linebender/tiny-skia), and
  is the same software fallback `iced_tiny_skia` ships — mature and permissive.
- The **scene/drawer is shared** between the raster backends (Windows now, Linux
  if/when it's viable), so we write the custom row-drawing once for everything
  that isn't macOS.

### The shape that falls out

```
                 ┌───────────────────────────────┐
   consumer ───► │  flyout::Menu (declarative)    │   builder API, §5
                 │  + MenuEvent / on_click        │
                 └───────────────┬───────────────┘
                                 │  Backend trait: open(anchor), close(),
                                 │  set_theme(), emit(MenuEvent)
              ┌──────────────────┼────────────────────┐
              ▼                  ▼                     ▼
   macos::AppKitBackend   raster::RasterBackend   (linux: see §4)
   (NSPopover + NSViews)  (winit+softbuffer+         honest fallback:
    — prototype today      tiny-skia; shared         native menu via the
                           scene drawer)             consumer's own muda,
                                                     OR non-anchored raster
```

One API, two real renderers, a documented Linux carve-out. Text rendering on the
raster path uses a font stack (`cosmic-text` / `fontdue` — system-font lookup +
shaping) feeding `tiny-skia` glyph rasterization; AppKit uses `NSFont` /
`NSAttributedString` as the prototype already does.

---

## 3. Tray-anchoring, per OS — head-on

Anchoring — *where on screen does the popup appear* — is the genuinely hard,
genuinely per-OS problem. The earlier doc's verdict holds and is reconfirmed:

### macOS — solved, natively

`NSStatusItem.button` is both the click target and the anchor. The idiomatic path
is `NSPopover.showRelativeToRect_ofView_preferredEdge(...)` against the status
button — **AppKit computes the on-screen position for us**, including *which
display the menu bar is on*. The prototype does exactly this
(`src/ui/popover.rs`, `PopoverHost::show` → `showRelativeToRect_ofView_preferredEdge`
with `NSRectEdge::MinY`). `tray-icon` also exposes the status item and its rect if
we ever hand-place. Nothing to solve.

### Windows — solved

`Shell_NotifyIconGetRect` returns the screen-coordinate bounding rectangle of a
notification icon, given a `NOTIFYICONIDENTIFIER` (Microsoft Learn,
`shellapi.h`). `tray-icon` surfaces this as `TrayIcon::rect()` /
`TrayIconEvent` position (physical px). Feed it to the popup window's
`set_outer_position` (or raw `SetWindowPos`), choosing the edge so the popup
opens toward screen center. The tray rect already encodes the correct monitor, so
multi-monitor is handled. (`tauri-plugin-positioner`'s `TrayBottomCenter` does
precisely this on Win/mac.)

### Linux — NOT solvable through this stack. The honest verdict.

This is the crux, and the answer is the uncomfortable one. The modern Linux tray
is **StatusNotifierItem / AppIndicator over D-Bus**: the *host* (a GNOME
extension, KDE plasmoid, or xembed shim) owns and draws the icon **in its own
process**, and renders the menu from a `com.canonical.dbusmenu` description the
app exports. The application is **never told where its icon is on screen and
never receives the click coordinate.** `tray-icon` documents this flatly:
`TrayIconEvent` — *"Linux: Unsupported. The event is not emitted"*; `rect()` —
*"Linux: Unsupported."* **There is no icon rectangle to query and no click
coordinate delivered**, so a tray-anchored popup is **architecturally impossible**
on the SNI/AppIndicator path.

Compounding it, **Wayland forbids a client from positioning its own toplevel**:
`set_outer_position` is a documented no-op on Wayland, by protocol design — the
*compositor* owns placement (winit docs; `winit#2920`). Client-positioned popups
exist only as `xdg_positioner` popups *relative to your own surface* (you have no
surface near the panel's tray icon) or via `wlr-layer-shell` (wlroots/KWin only,
**not GNOME**, and not wrapped by winit — `winit#2582`).

**What the crate will and won't promise on Linux:**

| Option | Verdict |
|---|---|
| **1. Native SNI/dbusmenu menu** (consumer keeps `muda`) | **Recommended default.** Loses custom styling, but it's the only thing that actually works everywhere a Linux tray works. `flyout` ships a `LinuxFallback` that hands the same `Menu` spec to the consumer's native-menu path. |
| **2. X11-only `_NET_SYSTEM_TRAY` geometry** | Possible *only* on pure-X11 XEmbed trays; brittle, excludes Wayland and SNI-on-X11. Out of scope for v1; documented as a future opt-in. |
| **3. Position at pointer on click** | Needs the click coordinate, which SNI doesn't deliver. Viable only for **non-tray context menus** (where the consumer *does* have a pointer event) — supported via the `anchor_at_point` API (§5), not for tray anchoring. |
| **4. `wlr-layer-shell` surface** | Real on wlroots/KWin, impossible on GNOME, not in winit. Future community backend at best. |

**The crate will NOT promise a styled tray-anchored popup on Linux.** It promises:
(a) a correct **native-menu fallback** on Linux tray, and (b) a **styled
context menu at an explicit point** (usable by non-tray callers and by X11 apps
that can supply geometry). This must be stated plainly in the README so no
consumer is surprised.

### Cross-cutting: dismiss, multi-monitor, focus

- **Dismiss-on-click-outside / focus loss.** macOS: `NSPopover` `.transient`
  dismisses for free (prototype uses it); a hand-placed `NSPanel` needs a global
  `NSEvent` monitor since a non-key panel won't emit normal focus-loss. Windows:
  `WindowEvent::Focused(false)` is the first hook but is unreliable for genuine
  non-activating windows — back it with a `WH_MOUSE_LL` / outside-click hook and
  `WM_ACTIVATEAPP`. The crate owns this so consumers don't re-implement it.
- **Non-activating / no focus-steal.** macOS needs a native non-activating
  `NSPanel` *only if not using `NSPopover`* — `winit ≥ 0.31` exposes
  `WindowAttributesExtMacOS::with_panel(true)`; a never-key panel **cannot host
  text fields** (hence the non-goal). Windows: `WS_EX_NOACTIVATE`
  (+ `WS_EX_TOOLWINDOW` to skip the taskbar). Linux: `with_active(false)` is
  unsupported on X11/Wayland — another reason Linux stays on native menus.
- **Multi-monitor / which display.** macOS `NSPopover`/`NSStatusItem` handle it.
  Windows: the tray rect already names the right monitor; the crate clamps the
  popup to that monitor's work area so it never spills off-screen. Linux: N/A.

---

## 4. Public API design

The API is the product. It is **backend-agnostic, declarative, and
builder-shaped**, and it must express everything usagio's `RowStyle` needs and
more (alignment, multi-column rows, colors incl. semantic, fonts, leading &
trailing icons, enabled/checked, separators, section headers, submenus).

### Core types (illustrative signatures — not a full impl)

```rust
// ---- Handle & lifecycle ----------------------------------------------------

/// A live tray-anchored menu. Created once; its content is swapped with
/// `set_menu` as the app's state changes (usagio rebuilds every ~0.75s tick).
pub struct TrayMenu { /* opaque: owns the backend */ }

impl TrayMenu {
    /// Anchor to a platform tray handle. On macOS pass the `NSStatusItem`
    /// button (from `tray-icon`'s `ns_status_item()`); on Windows the crate
    /// reads the icon rect via `Shell_NotifyIconGetRect`. On Linux this returns
    /// `Err(Unsupported::TrayAnchor)` and the caller must use the native-menu
    /// fallback (see `LinuxFallback`).
    pub fn new(anchor: TrayAnchor, opts: MenuOptions) -> Result<Self, Error>;

    /// Replace the content (cheap; re-rendered on next open, or live if shown).
    pub fn set_menu(&self, menu: Menu);

    /// Show/hide/toggle anchored to the tray icon.
    pub fn open(&self);
    pub fn close(&self);
    pub fn toggle(&self);

    /// Receive clicks by id (channel), mirroring `muda`'s MenuEvent ergonomics.
    pub fn events(&self) -> &MenuEventReceiver; // yields MenuEvent { id: MenuId }
}

/// A free-standing styled context menu shown at a screen point (works anywhere
/// you have a pointer coordinate — including Linux/Wayland via xdg_positioner
/// relative to the caller's own surface).
pub struct ContextMenu { /* … */ }
impl ContextMenu {
    pub fn new(menu: Menu, opts: MenuOptions) -> Self;
    pub fn open_at(&self, point: LogicalPoint, anchor_edge: Edge);
}

pub struct MenuOptions {
    pub min_width: Option<f32>,
    pub max_width: Option<f32>,
    pub theme: ThemeSource,      // FollowSystem | Light | Dark | Custom(Theme)
    pub corner_radius: f32,
    pub padding: Insets,
    pub default_font: Font,
    pub on_click: Option<Box<dyn Fn(&MenuId) + Send>>, // closure OR events()
}

// ---- Declarative menu tree -------------------------------------------------

#[derive(Clone, Default)]
pub struct Menu { pub items: Vec<Item> }

#[derive(Clone)]
pub enum Item {
    Row(Row),
    Separator,
    SectionHeader(Row),          // styled, non-interactive group heading
    Submenu { label: Row, menu: Menu },
}

/// One row. Built from left→right segments so multi-column layouts
/// (`label ......… value`) are first-class, not a tab-stop hack.
#[derive(Clone)]
pub struct Row {
    pub id: MenuId,              // click id string, e.g. "switch:claude:me@x.com"
    pub segments: Vec<Segment>,  // laid out across columns (see Align)
    pub leading: Option<Icon>,   // logo/avatar/checkmark column
    pub trailing: Option<Icon>,
    pub enabled: bool,
    pub checked: Option<bool>,   // Some → show check column; None → no column
    pub row_style: RowStyle,     // bg, min height, hover behavior
}

#[derive(Clone)]
pub struct Segment {
    pub text: String,
    pub runs: Vec<StyleRun>,     // per-substring color/weight spans
    pub align: Align,            // Left | Center | Right (within its column)
    pub flex: Flex,              // Fixed | Grow — Grow eats leftover width,
                                 // so `label <grow> value` right-flushes `value`
                                 // with NO reserved chevron gap (the whole point)
    pub font: Option<Font>,      // per-segment font override
}

#[derive(Clone)]
pub struct StyleRun { pub start: usize, pub len: usize, pub color: Color, pub weight: Option<Weight> }
// offsets are UTF-16 code units — matches NSRange and usagio's existing spans.

#[derive(Clone, Copy)]
pub enum Align { Left, Center, Right }
#[derive(Clone, Copy)]
pub enum Flex  { Fixed, Grow }

/// Literal OR semantic. Semantic colors map to NSColor system colors on macOS
/// (labelColor, secondaryLabelColor, controlAccentColor, systemOrange/Red…) and
/// to theme-derived values on the raster backend, so dark/light is automatic.
#[derive(Clone, Copy)]
pub enum Color {
    Rgba(u8, u8, u8, u8),
    Label, SecondaryLabel, Accent, Separator,
    SystemRed, SystemOrange, SystemGreen, SystemYellow,
}

#[derive(Clone)]
pub struct Font { pub family: FontFamily, pub size: f32, pub weight: Weight }
pub enum FontFamily { System, SystemMono, Named(String) }
pub enum Weight { Regular, Medium, Semibold, Bold }

/// Icons/logos from raw bytes — format auto-detected; SVG rasterized per-DPI.
#[derive(Clone)]
pub enum Icon {
    Png(Arc<[u8]>),
    Svg(Arc<[u8]>),
    Checkmark,                   // themed check glyph in the check column
    Symbol(&'static str),        // SF Symbol name on macOS; bundled fallback elsewhere
}
```

### A worked example: rendering usagio's exact menu

This builds the real usagio menu — provider groups (Claude / Codex), the active
account bold + checkmarked, **flush-right colored percentages with no chevron
gap**, and a per-account submenu — proving the API is sufficient.

```rust
use flyout::*;

fn build(snapshot: &Snapshot) -> Menu {
    let mut menu = Menu::default();

    for group in snapshot.provider_grouped() {           // Claude, then Codex
        // Bold, icon'd, non-interactive group header (maps to
        // provider_group_header_row: bold + section_header + icon_slug).
        menu.items.push(Item::SectionHeader(Row {
            id: MenuId::NONE,
            leading: Some(Icon::Png(group.icon_png.clone())),   // 16px provider logo
            segments: vec![Segment::label(&group.display_name)
                .font(Font::system(13.0, Weight::Bold))],
            ..Row::default()
        }));

        for acct in group.accounts {                     // soonest-reset order preserved
            // Trailing "S% / W%" (or locked countdown) with severity spans,
            // RIGHT-aligned and flush — the label Grows to eat the gap, so the
            // percentages sit at the true right edge with no reserved chevron
            // column. This is the exact pain custom-tray-popup.md set out to kill.
            let label = Segment::new(&acct.display)
                .flex(Flex::Grow)                        // pushes value flush-right
                .font(if acct.active { Font::system(13.0, Weight::Bold) }
                      else           { Font::system(13.0, Weight::Regular) });

            let value = Segment::new(&acct.trailing)     // "47% / 89%" or "3h 12m"
                .align(Align::Right)
                .runs(acct.severity_runs());             // StyleRun{Color::SystemRed/Orange}

            menu.items.push(Item::Submenu {
                label: Row {
                    id: MenuId::from(format!("switch:{}:{}", acct.provider, acct.key)),
                    leading: if acct.active { Some(Icon::Checkmark) } else { None },
                    checked: Some(acct.active),
                    segments: vec![label, value],
                    enabled: true,
                    ..Row::default()
                },
                // The per-account detail submenu (reset windows, "updated …",
                // Switch / Launch / Remove) — maps to submenu_info_rows + action rows.
                menu: account_submenu(&acct),
            });
        }
        menu.items.push(Item::Separator);
    }

    // Bottom actions. "Quit   usagio vX.Y.Z" with a grey, right-flushed version
    // tail — label Grows, version segment is SecondaryLabel + Align::Right.
    menu.items.push(Item::Submenu { label: capture_row(), menu: capture_submenu(snapshot) });
    menu.items.push(Item::Submenu { label: settings_row(), menu: settings_submenu(snapshot) });
    menu.items.push(Item::Row(Row {
        id: MenuId::from("quit"),
        segments: vec![
            Segment::new("Quit").flex(Flex::Grow),
            Segment::new(&format!("usagio v{}", env!("CARGO_PKG_VERSION")))
                .align(Align::Right)
                .runs(vec![StyleRun::all(Color::SecondaryLabel)]),
        ],
        ..Row::default()
    }));

    menu
}
```

### How usagio's `RowStyle` maps onto it

usagio's `RowStyle` (`src/menubar.rs`) fields map one-to-one onto richer,
backend-neutral concepts — every one is expressible, and several hacks disappear:

| `RowStyle` field | `flyout` equivalent |
|---|---|
| `plain` (title, used to match native item) | gone — rows carry their own `id`, no string-matching needed |
| `bold` | `Segment.font.weight = Bold` |
| `section_header` | `Item::SectionHeader` |
| `colors: Vec<(off, len, Severity)>` | `Segment.runs: Vec<StyleRun>` with `Color::SystemRed/Orange` |
| `tab_x_kind: Some(MenuRight)` + `compute_menu_right_x` measuring hack | **deleted** — `Flex::Grow` + `Align::Right` is true layout, no tab-stop fudge, no `CHEVRON_COLUMN_WIDTH`/`MIN_LABEL_TRAILING_GAP` constants |
| `icon_slug` | `Row.leading = Icon::Png(bytes)` |
| `checkmark` | `Row.checked = Some(true)` / `Icon::Checkmark` |
| `grey_tail_from` | a trailing `Segment` with `Color::SecondaryLabel` |
| `disabled_but_white` (info rows that must read normal, route to `noop`) | `Row { enabled: true, id: "noop", … }` with `Color::Label` — no "disabled-but-recolor" workaround needed |

The click-id strings (`switch:claude:<key>`, `capture:…`, `launch:…`,
`remove:…`, `refresh:now`, `autoswap:<pct>`, `notifications:<trigger>`, `quit`,
`noop`) pass straight through as `MenuId`s; `flyout` reports them back verbatim to
`handle_click` — confirmed against the routing in `handle_click` (`menubar.rs`).

---

## 5. Theming & accessibility

### Theming (dark/light + accent)

- **macOS backend:** already free. Every color is a semantic `NSColor`
  (`labelColor`, `secondaryLabelColor`, `controlAccentColor`, `separatorColor`,
  `systemOrange/RedColor`) and every size is in points — so the popover follows
  `effectiveAppearance` and the user's accent automatically, exactly as the
  prototype does (`src/ui/popover.rs` comments confirm this is the design).
- **Raster backend:** the crate queries the OS theme at open and subscribes to
  change events (winit `Theme` + `WindowEvent::ThemeChanged`; Windows accent via
  `DwmGetColorizationColor` / `UISettings::GetColorValue`), resolves every
  `Color::Semantic` against a `Theme` struct, and repaints on change. Consumers
  that want a fixed look pass `ThemeSource::Custom(Theme)`.

### Accessibility

This is the one real thing native menus give for free and a custom popup risks
losing. The crate's position, stated honestly:

- **macOS (native backend): mostly free.** Each row is a real `NSView` /
  `NSControl`; wiring `NSAccessibility` role/label/`AXPress` per row and
  arrow/Enter/Esc key handling gets VoiceOver + keyboard nav close to a native
  menu. The Phase-2 "make it the default" gate is a VoiceOver pass.
- **Raster backend (Windows/Linux): via AccessKit.** A raw `tiny-skia` pixmap is
  a black box to screen readers; the crate integrates **AccessKit** (the same
  path egui uses — native APIs on Windows/macOS, enabled by default in `eframe`)
  to publish an accessibility tree of the rows. This is an extra moving part and
  **less complete than native** — so it ships as a **documented, scoped** feature
  (`a11y` cargo feature), with keyboard navigation (arrows/Enter/Esc/type-ahead)
  implemented by the crate directly regardless of AccessKit.
- **Known limitation, stated in the README:** on the raster backend, screen-reader
  fidelity depends on AccessKit coverage for the platform; the crate guarantees
  keyboard navigation and focus handling but does not claim parity with native
  menu a11y on every Windows/Linux configuration.

---

## 6. How usagio migrates onto it

The migration is a **render-layer swap only** — the seam already exists. usagio's
data model is fully decoupled from rendering: `build_snapshot()` is pure and
network-free, and the in-flight `src/ui/mod.rs` already defines a
toolkit-neutral `PopoverModel` / `PopoverRow` view-model with click-ids baked in.

**The seam:**

1. usagio keeps `build_snapshot()` → `Snapshot`, the ordering
   (`provider_grouped_order`), countdown/locked logic, and **`handle_click(id)`
   unchanged**.
2. A thin mapping function `flyout_menu_from_snapshot(&Snapshot) -> flyout::Menu`
   replaces `menu_styles()`/`PopoverModel`. It is the evolution of the existing
   `src/ui/mod.rs` view-model — same inputs, same click-id strings, richer output
   type.
3. usagio creates one `flyout::TrayMenu` at launch (anchored via `tray-icon`'s
   status item), calls `set_menu(...)` on each ~0.75s tick when the signature
   changes (the prototype's structure), and drains `events()` into
   `handle_click`. The `NSPopover` prototype in `src/ui/popover.rs` **becomes the
   crate's macOS backend** — it is not discarded; it moves behind the
   `Backend` trait and gains the generic `Menu` as its input instead of the
   usagio-specific `PopoverModel`.

**Feature-flag / fallback strategy during rollout** (mirrors the in-flight
`custom-popup` feature):

- usagio keeps the native `muda`/`NSMenu` path compiled as the shipping default.
- `--features custom-popup` routes through `flyout`. macOS first (parity:
  all rows, detail cards, every action, dark mode, VoiceOver), then flip the
  macOS default, keeping `muda` behind `--no-default-features` for one release.
- **Windows:** adopt the `flyout` raster backend once it reaches parity; `muda`
  stays the fallback.
- **Linux:** usagio **stays on the native `muda` menu indefinitely** —
  `flyout::TrayMenu::new` returns `Unsupported::TrayAnchor` there, and usagio's
  existing `cross_platform::run` handles it. `flyout` provides `LinuxFallback`
  helpers to render the same `Menu` spec as a native `muda` tree so usagio keeps
  one menu definition.

---

## 7. Packaging as a standalone crate

- **Layout:** start as a **new workspace member `crates/flyout/`** inside the
  usagio repo (add a `[workspace]` with `members = ["crates/flyout", "."]` or the
  inverse). This lets the crate co-evolve with its first real consumer and get
  dogfooded every build, without a premature repo split. **When it stabilizes,
  move it to a sibling repo** (`flyout/`) and depend on the published version.
- **Crate split:** `flyout` (the public API + backend dispatch), with backend
  code behind `cfg`/features in the same crate initially; split into
  `flyout-appkit` / `flyout-raster` sub-crates only if compile times demand it.
- **What to expose:** `Menu`/`Item`/`Row`/`Segment`/`Align`/`Flex`/`Color`/
  `Font`/`Icon`/`TrayMenu`/`ContextMenu`/`MenuOptions`/`MenuEvent`/`Theme` and
  the `Backend` trait (for community Linux backends, e.g. layer-shell). Nothing
  else.
- **What must NOT leak in:** anything usagio-specific — `Snapshot`,
  `ProviderSection`, `AcctView`, `Severity`/`SeverityBands`, the `switch:`/
  `capture:` id grammar, the 0.75s tick, provider registry, countdown/locked
  logic. The crate knows only `Menu` and `MenuId` *strings*; the id grammar is
  the consumer's business.
- **Licensing:** **MIT** (or MIT OR Apache-2.0, the Rust-ecosystem norm) to match
  usagio and stay maximally reusable. This is a reason Slint was rejected (§2).
- **crates.io path:** depends only on permissive crates (`winit`, `softbuffer`,
  `tiny-skia`, `cosmic-text`/`fontdue`, `accesskit`, `objc2`/`objc2-app-kit`,
  `windows`/`windows-sys`, optionally `tray-icon` re-export for anchors). Ship
  with a runnable `examples/` (a tray app + a context menu), doc'd README with
  the **Linux honesty section front-and-center**, CI building all three targets.
  Publish `0.1` after the macOS backend + API stabilize; keep it pre-1.0 until
  the Windows backend proves the API is backend-neutral.

---

## 8. Phasing & effort

| Phase | Scope | Size | Demoable milestone |
|---|---|---|---|
| **0 — API + view-model seam** | Finalize `Menu`/`Row`/`Segment`/etc.; `flyout_menu_from_snapshot`; fixture tests reproducing today's rows/colors/active-marking. No window yet. | **S** | `cargo test` proves the spec reproduces the current menu content. |
| **1 — macOS AppKit backend** | Generalize the in-flight `NSPopover` prototype into the crate's `AppKitBackend` behind the `Backend` trait; multi-column `Flex::Grow` layout (kills the chevron gap), submenus, semantic colors, transient dismiss. Behind usagio's `custom-popup` feature. | **M** | `cargo run --features custom-popup` on macOS shows the styled popover with flush-right percentages and working submenus. |
| **2 — macOS parity + default flip** | Detail cards, capture/settings trees, every action, dark mode, VoiceOver + keyboard nav. Flip macOS default; keep `muda` one release as kill-switch. | **M** | macOS ships the styled popover as default; native menu removable. |
| **3 — Windows raster backend** | `winit + softbuffer + tiny-skia` scene drawer; `WS_EX_NOACTIVATE` layered window anchored via `Shell_NotifyIconGetRect`; theme follow; outside-click dismiss; AccessKit. | **M–L** | Windows tray shows the same styled menu from the same `Menu` spec. |
| **4 — extract + publish** | Move to a sibling repo, MIT, examples, README (Linux carve-out), CI, `0.1` on crates.io. Linux ships the `LinuxFallback` native menu. | **S–M** | `flyout 0.1` on crates.io; usagio depends on the published crate. |
| *(future)* Linux layer-shell backend | Community/opt-in `wlr-layer-shell` backend for wlroots/KWin only. | **L, not promised** | — |

**The single biggest risk: Linux.** Not a bug we can fix — a protocol reality.
The SNI/AppIndicator tray gives no icon geometry and delivers no click
(`tray-icon` documents both as Unsupported), and Wayland forbids client
self-positioning by design. Any messaging of "styled menus everywhere" must
carve out Linux up front. The crate's credibility depends on being **honest about
this in the README and the API** (an explicit `Unsupported::TrayAnchor` error and
a `LinuxFallback`) rather than shipping a half-working Linux path that surprises
consumers. Second-order risk: raster-backend **accessibility** — mitigated by
AccessKit + crate-owned keyboard nav, scoped as a documented limitation.

---

## 9. Naming

Requirements: available-ish on crates.io, evokes a styled/custom cross-platform
tray-or-context popup menu, not tied to usagio, short/lowercase/hyphen-free, not
confusable with `muda` / `tray-icon` / `menu` / `egui`.

**Recommendation: `flyout`.** A "flyout" is the exact UI primitive this crate
draws — a panel that flies out from an anchor (tray icon or point). It's short,
lowercase, hyphen-free, memorable, generic (no usagio coupling), and unconfusable
with the existing crates. **Checked `https://crates.io/api/v1/crates/flyout` →
HTTP 404 (free).**

Alternatives (all crates.io-checked):

- **`menuet`** — "menu" + a light, elegant connotation (the dance); styled-menu
  read is obvious. `crates.io/api/v1/crates/menuet` → **404 (free)**. Slightly
  cute; risks being read as a typo of "menu".
- **`poplet`** — a small *pop*up; diminutive and friendly.
  `crates.io/api/v1/crates/poplet` → **404 (free)**. Less self-explanatory than
  `flyout`.
- **`trayflyout`** — maximally descriptive, but longer and narrows the crate to
  *tray* when it also does point-anchored context menus.
  `crates.io/api/v1/crates/trayflyout` → **404 (free)**.

Checked-and-taken (avoid): `perch` (Mastodon/Bluesky client, GPL), `plume`
(text-editor spawner), `morsel` (Morse library), `nook` (niche types), `vellum`
(yanked wiki app).

Pick **`flyout`** unless a clash surfaces at publish time, in which case
`menuet` is the fallback.
