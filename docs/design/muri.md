# Design: `muri` — a cross-platform, fully-styleable tray-icon + popup-menu crate

Status: **finalized design / maintainer-approved decisions.** Design + architecture
+ public API only. The crate lives in its **own repository**
(<https://github.com/MattJackson/muri>); this document is the authoritative design
that usagio consumes. No usagio source is changed by it.

> **This document supersedes `docs/design/styled-menu-crate.md`** (the prior
> "flyout" proposal). That earlier doc did the research and recommended a name
> (`flyout`) and a **native-macOS / portable-elsewhere hybrid** rendering model.
> The maintainer overrode two of its recommendations, and this doc reconciles to
> them:
>
> 1. **Name is `muri`** (Menu Utilities for Rust Interfaces), not `flyout`.
> 2. **One consistent custom-drawn appearance on every OS** — *not* the
>    native-macOS hybrid. Even on macOS the menu is drawn by muri's own raster
>    surface; AppKit is used only to *anchor and dismiss* that surface.
>
> `styled-menu-crate.md` has been **deleted** in favor of this file. Its
> platform findings (especially the Linux verdict) are carried forward here
> verbatim in substance.

---

## 0. Decisions (fixed — not up for relitigation)

| Decision | Value |
|---|---|
| **Name** | `muri` — Menu Utilities for Rust Interfaces |
| **Repo** | <https://github.com/MattJackson/muri> (standalone, not a usagio workspace member) |
| **License** | MIT |
| **Look & feel** | **One** consistent custom-drawn appearance across all OSes (not native-per-OS chrome) |
| **Scope** | muri owns the **tray icon + styled popup + anchoring** — a full `muda` + `tray-icon` replacement |
| **Rendering stack** | `winit` (windowing) + `softbuffer`/`tiny-skia` (CPU raster) + `cosmic-text` (text/fonts) |
| **Submenus** | flyout panels beside the row (styled, nestable) |
| **Accessibility** | **first-class, v1 target** — native a11y over the custom tree (NSAccessibility / UIA / AT-SPI) |
| **Theming** | follows OS dark/light + accent by default; full theme API for overrides |
| **v1 OS order** | macOS → Windows → Linux |

The consuming app (usagio) drives muri with a small builder:

```rust
let tray = muri::Tray::new(icon)
    .tooltip("usagio")
    .menu(menu)
    .on_click(|id| handle_click(id.as_str()));
tray.run()?;  // installs the tray icon and runs the anchoring/render loop
```

---

## 1. Scope & non-goals

### What muri does

- Owns and installs the **tray / status-bar icon** (the glyph itself), replacing
  `tray-icon`.
- Draws **styled, custom popup menus** anchored to that icon, or to an arbitrary
  screen point (a true context menu), replacing `muda`.
- Gives the consumer **full presentation control**: per-segment alignment
  (left / center / right and multi-column `label … value` rows), literal or
  semantic colors, fonts (family / size / weight), leading & trailing
  icons/logos (PNG or SVG bytes), enabled/checked state, separators, section
  headers, and **nested submenus** as flyout panels.
- Exposes a **declarative builder API** plus a click model (by id string and/or
  closure), independent of the backend drawing pixels.
- **Follows system dark/light + accent by default**, is **HiDPI-crisp**, and
  opens with **no perceptible delay**.
- Ships **one consistent look on macOS and Windows**; on **Linux it honestly
  degrades** (see §4).

### What muri does NOT do

- It is **not a general GUI framework** — no arbitrary widget trees, forms, text
  inputs, tabs, or scroll-views-of-anything. It draws *menus* (lists of rows,
  possibly nested), full stop.
- It is **not a replacement for application windows / dialogs**. Text entry
  (paste-an-API-key) belongs in a normal window the host app owns — a
  non-activating popup deliberately can't host a first-responder text field.
- It is **not a fork of `muda`**. `muda` is a thin data-model sync over native
  OS-drawn menu objects (`NSMenu` / GTK `MenuItem` / Win32 `HMENU`); there is no
  drawable layer to fork. To get custom drawing you must bypass native menus
  entirely — which is exactly what muri does.

---

## 2. Rendering architecture

> **One custom-drawn raster backend on every OS.** `winit` for the window,
> `softbuffer` to get a CPU framebuffer, `tiny-skia` to rasterize 2D shapes,
> `cosmic-text` to shape and lay out text. The *same* scene drawer renders the
> menu identically on macOS, Windows, and Linux. Per-OS code is confined to the
> thin **anchoring / dismiss / a11y** shims around that shared drawer.

### Why this stack (and why not the alternatives)

A tray-menu crate has an unusual constraint set: it must be **small**, **open
instantly** (no GPU warm-up stutter when the icon is clicked), be **HiDPI-crisp**,
follow **system dark/light + accent**, and be **accessible**.

| Approach | Binary | First-open latency | Styling control | A11y | Notes |
|---|---|---|---|---|---|
| GPU toolkit (egui/iced/winit+wgpu) | medium–large | GPU device/surface init on first show (~0.5s+, cold drivers worse) | good | AccessKit | pays a latency tax a popup can't afford |
| Native per-OS (AppKit + Win32 + GTK) | smallest | instant | native only — **can't** restyle | free | 3× the code; defeats the "one custom look" goal |
| **CPU raster (`tiny-skia`) everywhere** | **small** | **instant** (no GPU init) | **full pixel control** | AccessKit + native shims | the chosen path |

- **GPU toolkits** pay a one-time device/surface initialization the first time a
  window is shown (cf. `gfx-rs/wgpu#6155`); a popup that must appear the instant
  the icon is clicked can't absorb that. CPU raster has no such cost.
- **Native per-OS** is the smallest binary and free a11y, but it cannot deliver
  the **one consistent custom look** the maintainer requires — native menus draw
  their own pixels. It is also 3× the drawing/layout/hit-test/dismiss code.
- **`tiny-skia`** is purpose-built as a minimal CPU-only 2D library optimized for
  binary size and quality (`linebender/tiny-skia`); it is the same software
  fallback `iced_tiny_skia` ships. `cosmic-text` provides system-font lookup +
  shaping feeding `tiny-skia` glyph rasterization. All permissive-licensed.
- **Slint** was additionally disqualified by its GPLv3/paid licensing —
  incompatible with muri's MIT.

> **Note on macOS:** even though macOS draws through the same raster surface, a
> borderless `winit`/`softbuffer` toplevel is *not enough on its own* for tray
> anchoring and transient dismiss. The surface is hosted in / positioned by a
> native **non-activating AppKit panel** anchored to the `NSStatusItem` button,
> and a global `NSEvent` monitor handles click-outside dismiss (see §3). AppKit
> is the *anchor*, not the *renderer*.

### The shape that falls out

```
                 ┌─────────────────────────────────────────┐
   consumer ───► │  muri::Tray / muri::Menu (declarative)   │   builder API, §5
                 │  + MenuEvent / on_click                  │
                 └───────────────────┬─────────────────────┘
                                     │
                 ┌───────────────────▼─────────────────────┐
                 │  Shared scene drawer (ONE implementation)│
                 │  layout + tiny-skia raster + cosmic-text │
                 └───────────────────┬─────────────────────┘
                                     │  Platform shim: anchor(rect/point),
                                     │  dismiss, theme query, a11y tree
              ┌──────────────────────┼────────────────────────┐
              ▼                      ▼                         ▼
      macOS shim               Windows shim             Linux shim
  NSStatusItem anchor +    Shell_NotifyIconGetRect +   NO tray anchor (§4):
  non-activating NSPanel   WS_EX_NOACTIVATE layered    native-menu fallback
  + global event monitor   window + mouse-hook dismiss  OR pointer ContextMenu
  + NSAccessibility        + UIA                        + AT-SPI (AccessKit)
```

One API, one renderer, three thin platform shims, and a documented Linux
carve-out. This is the key departure from the prior doc, which kept two
renderers (native AppKit + raster); muri keeps **one renderer** and only
per-OS *shims*.

---

## 3. Tray anchoring, per OS — head-on

Anchoring — *where on screen the popup appears* — is the genuinely hard,
genuinely per-OS problem, and it is **the same problem regardless of who draws
the pixels**. That's the whole reason a single cross-platform toolkit wouldn't
have saved work.

### macOS — solved

`NSStatusItem.button` is both the click target and the anchor rect. muri creates
the status item, reads the button's screen frame, and positions its
non-activating panel relative to it (AppKit also tells us *which display the menu
bar is on*). `tray-icon` exposes the status item and its rect for reference; muri
owns this directly. Transient dismiss uses a global `NSEvent` monitor (a
non-key panel does not emit normal focus-loss events).

### Windows — solved

`Shell_NotifyIconGetRect` returns the screen-coordinate bounding rectangle of a
notification icon given a `NOTIFYICONIDENTIFIER` (`shellapi.h`). muri feeds that
rect to the popup window's position (`SetWindowPos`), choosing the edge so the
popup opens toward screen center; the rect already encodes the correct monitor,
so multi-monitor is handled. The window is `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`
(no focus steal, no taskbar button). Outside-click dismiss is backed by a
`WH_MOUSE_LL` hook + `WM_ACTIVATEAPP` (bare `WindowEvent::Focused(false)` is
unreliable for genuine non-activating windows).

### Linux — NOT solvable through this stack. The honest verdict.

This is the crux, and the answer is the uncomfortable one, carried forward
unchanged from the prior design:

The modern Linux tray is **StatusNotifierItem / AppIndicator over D-Bus**. The
*host* (a GNOME extension, KDE plasmoid, or XEmbed shim) owns and draws the icon
**in its own process**, and renders the menu from a `com.canonical.dbusmenu`
description the app exports. The application is **never told where its icon is on
screen and never receives the click coordinate**. `tray-icon` documents this
flatly: `TrayIconEvent` — *"Linux: Unsupported. The event is not emitted"*;
`rect()` — *"Linux: Unsupported."* **There is no icon rectangle to query and no
click coordinate delivered**, so a tray-anchored popup is **architecturally
impossible** on the SNI/AppIndicator path.

Compounding it, **Wayland forbids a client from positioning its own toplevel**:
`set_outer_position` is a documented no-op on Wayland by protocol design — the
*compositor* owns placement (winit docs; `winit#2920`). Client-positioned popups
exist only as `xdg_positioner` popups *relative to your own surface* (you have no
surface near the panel's tray icon) or via `wlr-layer-shell` (wlroots/KWin only,
**not GNOME**, not wrapped by winit — `winit#2582`).

**What muri will and won't promise on Linux:**

| Option | Verdict |
|---|---|
| **1. Native SNI/dbusmenu menu** | **Recommended default.** Loses the custom look, but is the only thing that works everywhere a Linux tray works. muri offers a `LinuxFallback` that renders the same `Menu` spec into a native `muda`/dbusmenu tree so the consumer keeps one menu definition. |
| **2. Pointer-anchored `ContextMenu`** | The styled raster surface **is** available wherever a pointer coordinate exists (a right-click menu), via `ContextMenu::open_at(point, edge)` — just not anchored to the tray icon. |
| **3. X11-only `_NET_SYSTEM_TRAY` geometry** | Possible only on pure-X11 XEmbed trays; brittle, excludes Wayland and SNI-on-X11. Out of scope for v1; documented future opt-in. |
| **4. `wlr-layer-shell` surface** | Real on wlroots/KWin, impossible on GNOME, not in winit. Future community backend at best. |

**muri will NOT promise a styled tray-anchored popup on Linux.** `Tray::run`
returns `Err(Error::Unsupported(Unsupported::TrayAnchor))` there. This is stated
plainly in the README so no consumer is surprised.

### Cross-cutting: dismiss, multi-monitor, focus

- **Dismiss-on-click-outside.** macOS: global `NSEvent` monitor. Windows:
  `WH_MOUSE_LL` hook + `WM_ACTIVATEAPP`. muri owns this so consumers don't
  re-implement it.
- **Non-activating / no focus-steal.** macOS: non-activating `NSPanel`
  (`winit ≥ 0.31` exposes `WindowAttributesExtMacOS::with_panel(true)`); a
  never-key panel cannot host text fields (hence the non-goal). Windows:
  `WS_EX_NOACTIVATE`. Linux: `with_active(false)` is unsupported on X11/Wayland —
  another reason Linux stays on native menus.
- **Multi-monitor.** macOS `NSStatusItem` and the Windows tray rect both name the
  correct monitor; muri clamps the popup to that monitor's work area so it never
  spills off-screen.

---

## 4. Rendering pipeline (the shared drawer)

The scene drawer is written **once** and is the heart of the crate:

1. **Theme resolution.** At open (and on OS theme-change events) muri resolves
   every semantic `Color` against the active `Theme` — `Color::Label` →
   `theme.label`, `Color::Accent` → OS accent, etc. Literal `Color::Rgba` passes
   through. OS theme/accent come from winit `Theme` + `WindowEvent::ThemeChanged`
   on Windows (`DwmGetColorizationColor` / `UISettings::GetColorValue`) and from
   `effectiveAppearance` / `NSColor` on macOS.
2. **Layout.** Each `Row` lays its `Segment`s out left→right. Segments with
   `Flex::Grow` absorb leftover width; `Flex::Fixed` take intrinsic width. A
   `Grow` label followed by an `Align::Right` value yields a **truly flush-right
   value with no reserved chevron column** — the core reason muri exists. Leading
   (icon/check) and trailing columns are laid out around the segment band; row
   height is `max(intrinsic, theme.row_height)`. Text is measured with
   `cosmic-text` at the target DPI.
3. **Raster.** `softbuffer` hands muri a CPU framebuffer for the window;
   `tiny-skia` fills the rounded-rect background, row highlight, separators, and
   icons; `cosmic-text` glyphs are blitted with per-`StyleRun` colors/weights.
   SVG icons are rasterized per-DPI; PNG icons are decoded once and scaled.
4. **Hit-testing & events.** Pointer position maps to a row; a click on an
   enabled row emits `MenuEvent { id }` through the registered `on_click` handler
   and closes the popup. Submenu rows open a nested flyout panel beside the row on
   hover/click (a second `winit` surface positioned at the row's right edge,
   clamped to the monitor).
5. **Keyboard nav.** Arrows move the selection, Enter activates, Esc closes,
   Right/Left open/close submenus, type-ahead jumps — implemented by muri
   directly, independent of any a11y backend.

All of the above is platform-agnostic; only steps that touch OS theme APIs and
window creation go through the per-OS shim.

---

## 5. Public API

The API is the product: backend-agnostic, declarative, builder-shaped. It lives
in `muri/src/lib.rs` (shipped today as a compiling skeleton with `todo!()`
renderer bodies). Illustrative signatures — see the crate for the authoritative,
fully-documented types.

```rust
// ---- Tray (owns icon + popup + anchoring) ----------------------------------
pub struct Tray { /* opaque */ }
impl Tray {
    pub fn new(icon: Icon) -> Self;
    pub fn menu(self, menu: Menu) -> Self;
    pub fn tooltip(self, text: impl Into<String>) -> Self;
    pub fn options(self, options: MenuOptions) -> Self;
    pub fn theme(self, theme: ThemeSource) -> Self;
    pub fn on_click(self, handler: impl Fn(&MenuId) + Send + 'static) -> Self;
    pub fn set_menu(&mut self, menu: Menu);           // swap content at runtime (usagio's ~0.75s tick)
    pub fn open(&self) -> Result<(), Error>;
    pub fn close(&self);
    pub fn run(self) -> Result<(), Error>;            // install icon + event loop; Err(Unsupported::TrayAnchor) on Linux
}

// ---- ContextMenu (styled menu at an explicit point; works on Linux too) ----
pub struct ContextMenu { /* opaque */ }
impl ContextMenu {
    pub fn new(menu: Menu) -> Self;
    pub fn options(self, options: MenuOptions) -> Self;
    pub fn on_click(self, handler: impl Fn(&MenuId) + Send + 'static) -> Self;
    pub fn open_at(&self, point: LogicalPoint, edge: Edge) -> Result<(), Error>;
}

// ---- Declarative menu tree -------------------------------------------------
pub struct Menu { pub items: Vec<Item> }
impl Menu {
    pub fn new() -> Self;
    pub fn row(self, row: Row) -> Self;
    pub fn separator(self) -> Self;
    pub fn section_header(self, row: Row) -> Self;
    pub fn submenu(self, label: Row, menu: Menu) -> Self;
    pub fn item(self, item: Item) -> Self;
}

pub enum Item {
    Row(Row),
    Separator,
    SectionHeader(Row),                    // styled, non-interactive group heading
    Submenu { label: Row, menu: Menu },    // flyout panel beside the row
}

pub struct Row {
    pub id: MenuId,                        // click id, e.g. "switch:claude:me@x.com"; MenuId::none() = inert
    pub segments: Vec<Segment>,            // laid out left→right across columns
    pub leading: Option<Icon>,             // logo / avatar / checkmark column
    pub trailing: Option<Icon>,
    pub enabled: bool,                     // default true
    pub checked: Option<bool>,             // Some → check column; None → no column
    pub background: Option<Color>,
    pub min_height: Option<f32>,
}
// builders: Row::new(id), Row::info(), .label(), .segment(), .segments(),
//           .leading(), .trailing(), .enabled(), .checked(), .background()

pub struct Segment {
    pub text: String,
    pub runs: Vec<StyleRun>,               // per-substring color/weight spans
    pub align: Align,                      // Left | Center | Right (within its width)
    pub flex: Flex,                        // Fixed | Grow  — Grow right-flushes the next segment
    pub font: Option<Font>,
    pub color: Option<Color>,
}
// builders: Segment::new(text), .align(), .flex(), .runs(), .font(), .color()

pub struct StyleRun { pub start: usize, pub len: usize, pub color: Color, pub weight: Option<Weight> }
// offsets are UTF-16 code units — matches NSRange and usagio's existing spans.

pub enum Align { Left, Center, Right }     // default Left
pub enum Flex  { Fixed, Grow }             // default Fixed

pub enum Color {                           // literal OR semantic (theme/NSColor-resolved)
    Rgba(u8, u8, u8, u8),
    Label, SecondaryLabel, Accent, Separator,
    SystemRed, SystemOrange, SystemGreen, SystemYellow,
}

pub struct Font { pub family: FontFamily, pub size: f32, pub weight: Weight }
pub enum FontFamily { System, SystemMono, Named(String) }
pub enum Weight { Regular, Medium, Semibold, Bold }

pub enum Icon {                            // format auto-detected; SVG rasterized per-DPI
    Png(Arc<[u8]>), Svg(Arc<[u8]>), Checkmark, Symbol(&'static str),
}

// ---- Theming & options -----------------------------------------------------
pub enum ThemeSource { FollowSystem, Light, Dark, Custom(Theme) }
pub struct Theme {                         // full override surface
    pub background, pub label, pub secondary_label, pub accent, pub separator, pub row_highlight: Color,
    pub row_font, pub header_font: Font,
    pub row_height, pub corner_radius, pub column_gap: f32,
    pub padding: Insets,
}
pub struct MenuOptions { pub min_width: Option<f32>, pub max_width: Option<f32>, pub theme: ThemeSource }

// ---- Errors ----------------------------------------------------------------
pub enum Unsupported { TrayAnchor, ClientPositioning }
pub enum Error { Unsupported(Unsupported), BadIcon(String), Platform(String) }
```

### Worked example: rebuilding usagio's exact menu

`examples/usagio_menu.rs` in the muri repo builds the real usagio menu and passes
`cargo build`/`clippy`/`fmt`. It reproduces:

- **Provider groups** (Claude, then Codex — usagio's alphabetical group order)
  as bold, logo'd, non-interactive `Item::SectionHeader`s
  (usagio's `provider_group_header_row`: `section_header: true` + `icon_slug`).
- **Accounts** as `Item::Submenu`: the label row carries
  `segments: vec![label.flex(Grow), value.align(Right).runs(severity)]`, so the
  `"S% / W%"` value (or a locked countdown like `"3h 12m"`) sits **flush-right
  with no chevron gap**, with `Color::SystemRed`/`SystemOrange` spans from the
  severity bands. The active account gets `Row::leading(Icon::Checkmark)` +
  `checked(true)` and a **bold** label font.
- **Per-account submenus**: reset-window info rows (`"Session resets in …"`,
  `"Weekly resets in …"`), an `"updated …"` line (`Color::SecondaryLabel`), a
  separator, then `"✓ Active"` (inert) or `"Switch to this account"`
  (`switch:<slug>:<key>`), `"Launch client"` (`launch:…`, gated on
  `supports_launch`), `"Remove…"` (`remove:…`, gated on `supports_remove`).
- **Bottom actions**: `"Capture current login"` and `"Settings"` submenus, then a
  **Quit** row: `segments: vec![Segment::new("Quit").flex(Grow),
  Segment::new("usagio vX").align(Right).color(SecondaryLabel)]` — the greyed
  version tail flush-right.

The click-id strings pass straight through as `MenuId`s and muri reports them
back verbatim to usagio's `handle_click`. The full id grammar usagio uses
(`switch:<slug>:<key>`, `launch:…`, `remove:…`, `capture:<slug>`,
`apikey:<slug>`, `autoswap:<pct>`/`off`/`now`, `notifications:<trigger>`,
`backup:save`/`restore`, `refresh:now`, `quit`, `noop`) is entirely the
consumer's business — muri only sees opaque strings.

### How usagio's `RowStyle` maps onto muri

| usagio `RowStyle` field (`menubar.rs`) | muri equivalent |
|---|---|
| `plain` (title used to match the native item) | **gone** — rows carry their own `id`, no string-matching |
| `bold` | `Segment.font.weight = Bold` |
| `section_header` | `Item::SectionHeader` |
| `colors: Vec<(off, len, Severity)>` | `Segment.runs: Vec<StyleRun>` with `Color::SystemRed/Orange` |
| `tab_x_kind: Some(MenuRight)` + `compute_menu_right_x` measuring hack | **deleted** — `Flex::Grow` + `Align::Right` is true layout; no tab-stop fudge, no chevron-column constants |
| `icon_slug` | `Row.leading = Icon::Png(bytes)` |
| `checkmark` | `Row.checked = Some(true)` / `Icon::Checkmark` |
| `grey_tail_from` | trailing `Segment` with `Color::SecondaryLabel` |
| `disabled_but_white` (info rows) | `Row::info()` (id = `none`) with `Color::Label` — no "disabled-but-recolor" workaround |

---

## 6. Theming & accessibility

### Theming

- Semantic colors (`Label`, `SecondaryLabel`, `Accent`, `Separator`,
  `SystemRed/Orange/Green/Yellow`) resolve against the active `Theme`; on macOS
  they map to the matching `NSColor` so dark/light + accent are automatic.
- `ThemeSource::FollowSystem` (default) queries the OS theme/accent at open and
  repaints on change events; `ThemeSource::Custom(Theme)` gives a fixed look.
- The full `Theme` exposes colors, `row_font`/`header_font`, `row_height`,
  `corner_radius`, `padding`, and `column_gap` — a consumer can restyle every
  pixel.

### Accessibility — first-class, and honest about the cost

This is the one thing native menus give for free and a custom-drawn popup risks
losing, so muri treats it as a **v1 requirement, not an afterthought** — but it
is genuinely the hardest part of the project.

**The core difficulty:** a `tiny-skia` pixmap is an opaque rectangle to a screen
reader. There are no `NSView`/`HWND`/widget objects for the AT to walk. muri must
therefore **publish a parallel accessibility tree** that mirrors the menu's
logical structure, and keep it in sync with what's drawn and focused.

**How the widget tree is structured to make this tractable:** the declarative
`Menu`/`Item`/`Row` tree *is already* an accessibility tree in disguise. muri
maps it directly:

- the popup → an `AXMenu` / UIA `Menu` / AT-SPI `menu` container;
- each `Item::Row` → an `AXMenuItem` / `menuitem`, with its accessible **name**
  from the concatenated segment text, **checked** state from `Row.checked`,
  **enabled** from `Row.enabled`, and an **activate/`AXPress`** action that fires
  the same `MenuEvent` a click would;
- `Item::SectionHeader` → a non-focusable group label;
- `Item::Submenu` → a `menuitem` with `haspopup` + a child submenu container.

**The integration path, per OS:**

- **macOS & Windows:** muri publishes this tree through **AccessKit**
  (`accesskit` + `accesskit_macos` / `accesskit_windows`), which bridges to
  **NSAccessibility** and **UIA** respectively — the same mechanism egui/eframe
  use, mature and enabled-by-default there. muri owns keyboard navigation
  (arrows/Enter/Esc/type-ahead, §4) regardless of AccessKit, and drives
  AccessKit's focus node from the same selection state.
- **Linux:** the styled surface isn't tray-anchored anyway (§3), so on the
  **native-menu fallback** path a11y is free (the native `dbusmenu`/GTK menu is
  accessible via AT-SPI); on the **pointer `ContextMenu`** path, AccessKit's
  `accesskit_unix` bridges to **AT-SPI**.

**Honest limitations stated in the README:** on the custom-drawn path,
screen-reader fidelity depends on AccessKit's per-platform coverage; muri
*guarantees* keyboard navigation and focus handling but does not claim byte-for-
byte parity with native menu a11y on every OS/AT combination. The v1 gate is a
real VoiceOver pass on macOS and an NVDA/Narrator pass on Windows.

This is the **second-biggest risk** after Linux (see §8).

---

## 7. Packaging

- **Layout:** a **standalone repository** (<https://github.com/MattJackson/muri>),
  *not* a usagio workspace member. The maintainer chose an early repo split so
  muri is publishable and reusable from day one; usagio depends on it by git/path
  during development and by version once published.
- **Crate split:** single `muri` crate; the shared drawer plus per-OS shims live
  behind `cfg(target_os = …)` in one crate. Split into `muri-macos`/`muri-windows`
  sub-crates only if compile times later demand it.
- **Public surface:** `Tray`, `ContextMenu`, `Menu`, `Item`, `Row`, `Segment`,
  `StyleRun`, `Align`, `Flex`, `Color`, `Font`/`FontFamily`/`Weight`, `Icon`,
  `Theme`/`ThemeSource`, `MenuOptions`, `Insets`, `Edge`, `LogicalPoint`,
  `MenuId`, `MenuEvent`, `Error`/`Unsupported`. Nothing usagio-specific leaks in
  (no `Snapshot`, no `switch:`/`capture:` id grammar, no 0.75s tick) — muri knows
  only `Menu` and opaque `MenuId` strings.
- **Dependencies:** all permissive — `winit`, `softbuffer`, `tiny-skia`,
  `cosmic-text`, `accesskit` (+ platform bridges), `objc2`/`objc2-app-kit`/
  `objc2-foundation` (macOS), `windows`/`windows-sys` (Windows). The skeleton
  shipped today has **zero** runtime deps so the API compiles instantly.
- **License:** **MIT** (copyright Matthew Jackson).
- **CI:** GitHub Actions runs `cargo fmt --check`, `cargo clippy -- -D warnings`,
  and `cargo build` on macOS (Windows/Linux jobs added as those backends land).
- **crates.io:** publish `0.1` once the macOS backend + API stabilize; keep it
  pre-1.0 until the Windows backend proves the API is truly backend-neutral.

---

## 8. Phasing & roadmap

| Phase | Scope | Size | Milestone |
|---|---|---|---|
| **0 — API skeleton + design** *(done)* | Standalone repo; `Tray`/`Menu`/`Item`/`Row`/`Segment`/`Align`/`Flex`/`Color`/`Theme`/`Icon` as real compiling types with `todo!()` renderer bodies; `examples/usagio_menu.rs`; README with the Linux carve-out; CI. | **S** | `cargo build`/`fmt`/`clippy` green; example reproduces usagio's menu tree. |
| **1 — macOS backend** | Shared scene drawer (`winit`+`softbuffer`+`tiny-skia`+`cosmic-text`); non-activating `NSPanel` anchored to `NSStatusItem`; `Flex::Grow` flush layout (kills the chevron gap); semantic colors → `NSColor`; transient dismiss via global event monitor; flyout submenus. | **M** | macOS tray shows the styled popup with flush-right percentages + working submenus. |
| **2 — macOS parity + a11y** | usagio's full menu (detail cards, capture/settings trees, every action), dark mode, **NSAccessibility via AccessKit + keyboard nav (VoiceOver pass)**. usagio can flip to muri on macOS, keeping native `muda` one release as a kill-switch. | **M** | macOS ships the styled popup as default; VoiceOver usable. |
| **3 — Windows backend** | `WS_EX_NOACTIVATE` layered window anchored via `Shell_NotifyIconGetRect`; theme/accent follow; `WH_MOUSE_LL` outside-click dismiss; **UIA via AccessKit (NVDA/Narrator pass)**. Same scene drawer, new shim only. | **M–L** | Windows tray shows the identical styled menu from the same `Menu` spec. |
| **4 — publish** | `muri 0.1` on crates.io; usagio depends on the published crate. | **S–M** | crates.io release. |
| **5 — Linux** | `LinuxFallback` → native `muda`/dbusmenu tree from the same `Menu`; pointer-anchored `ContextMenu` with AT-SPI a11y. **No** tray-anchored styled popup. | **M** | Linux tray works via native menu; styled context menus available at a point. |
| *(future)* Linux layer-shell | Community opt-in `wlr-layer-shell` anchored backend (wlroots/KWin only). | **L, not promised** | — |

### The biggest risks

1. **Linux (protocol reality, not a bug).** The SNI/AppIndicator tray gives no
   icon geometry and delivers no click, and Wayland forbids client
   self-positioning. No stack — muri's or anyone's — can anchor a styled popup to
   a Linux tray icon. muri's credibility depends on being **honest** about this
   in the README and API (an explicit `Unsupported::TrayAnchor` + a
   `LinuxFallback`) rather than shipping a half-working path that surprises
   consumers.
2. **Accessibility on the custom-drawn surface.** A raster pixmap has no native
   widget objects, so muri must publish an AccessKit tree and keep focus/state in
   sync — more moving parts than native menus, with fidelity bounded by
   AccessKit's per-platform coverage. Mitigated by mapping the declarative
   `Menu` tree straight onto menu roles and owning keyboard nav directly;
   gated by real VoiceOver/NVDA passes.
3. **macOS non-activating anchoring of a `winit`/`softbuffer` surface.** Hosting
   the CPU-raster surface inside a non-activating `NSPanel` anchored to the
   status item, with correct transient dismiss and no focus steal, is the
   trickiest part of Phase 1 (the prior doc leaned on `NSPopover` to get this for
   free; muri forgoes that to keep one consistent custom look, and must
   re-implement anchoring/dismiss over the panel). If `winit`'s macOS panel
   support proves insufficient, the fallback is a hand-rolled `objc2` `NSPanel`
   hosting the `softbuffer` layer.
```
