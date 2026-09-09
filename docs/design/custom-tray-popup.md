# Design: custom tray-anchored popup to replace the native menu

Status: **proposal / for maintainer review**. Target: a later release. This
document is design + architecture only — no code is changed by it.

## 1. Problem and goal

usagio renders its UI as a **native menu** today:

- macOS: `NSMenu` via `tray-icon` + `muda`, post-styled with
  `NSAttributedString` (`src/menubar.rs`, `mod mac_style`).
- Linux/Windows: `muda` menus via `platform::MenuBackend` (`mod
  cross_platform`).

Native menus constrain us to the OS menu layout: fixed row heights, OS-drawn
submenu chevrons with a reserved right margin (the origin of the gap between
our right-aligned `session% / weekly%` and the `>`), no custom widgets
(progress bars, sparklines, avatars), and limited typography. We've already
spent real effort fighting this — `mac_style` measures every row to compute a
shared right-align tab stop (`compute_menu_right_x`), then adds fudge constants
for the chevron column (`CHEVRON_COLUMN_WIDTH = 14.0`) and label/trailing gap
(`MIN_LABEL_TRAILING_GAP`) just to approximate a two-column layout the menu
system doesn't natively support. That is the "fitting what we want into a
normal menu system" the maintainer wants to escape.

**Goal:** full control over drawing (custom rows, widgets, spacing, type)
while staying robust and cross-platform, and — critically — while keeping the
existing snapshot→rows data model so only the *render* layer changes.

## 2. What we keep no matter which approach wins

The most important architectural fact: **usagio's data model is already fully
decoupled from rendering.** `build_snapshot()` (`src/menubar.rs`) is a pure,
network-free function that reads `State` and produces a `Snapshot`:

```
Snapshot
├── sections: Vec<ProviderSection>      // one per provider with captured accounts
│   └── accounts: Vec<AcctView>          // email/display, active flag, has_data
│       └── windows: Vec<WindowView>     // session/weekly/opus: pct + reset text
├── account_order: Vec<(usize, usize)>  // global soonest-expiry/priority sort
├── capture_creds / capture_api_key     // "Capture current login" rows
├── autoswap, threshold, notification_config
```

On top of that, the macOS renderer derives `Vec<RowStyle>` (`menu_styles`),
which already encodes *everything a custom renderer needs*: the plain text, a
`bold` flag for the active account, `colors: Vec<(offset, len, Severity)>`
spans, `section_header`, `icon_slug`, `checkmark`, right-align intent
(`TabX`), and the grey trailing "usagio vX.Y.Z". `Severity` (Amber/Red) and the
per-provider `SeverityBands` already turn a percentage into a colour band.

So the redesign is **not** a data-model change. It is: add a new *renderer*
that consumes `Snapshot` (and a cleaned-up, toolkit-neutral evolution of
`RowStyle`), leaving `build_snapshot`, the provider/account aggregation, the
`flat_account_order` / `provider_grouped_order` sorts, the countdown/locked
logic, and `handle_click`'s id-routing untouched. Click ids
(`switch:claude:<key>`, `capture:…`, `noop`, `quit`, …) stay exactly as they
are; the popup emits the same id strings the menu items carry today.

## 3. Approach comparison

### (a) Fork `muda`/`tray-icon`, replace only the "drawing layer" — REJECTED

The maintainer floated "fork from an OSS menu, use its core, change the menu
drawing to our drawing." Researched against muda's source and docs, **this
rests on a false premise: muda has no drawing layer to replace.**

muda is a thin cross-platform *data-model synchroniser* over each OS's native,
OS-drawn menu objects:

- macOS → AppKit `NSMenu`/`NSMenuItem`
- Linux → GTK 3/4 `gtk::MenuItem`
- Windows → Win32 `HMENU`

muda's entire job is to keep its `Menu`/`MenuItem`/`Submenu` structs in sync
with those native objects and funnel native click callbacks into one
`MenuEvent` channel. **The pixels are drawn by AppKit / GTK / Win32 USER.**
There is no muda-owned renderer sitting between the model and the screen. To
get custom drawing you would not be "swapping muda's renderer" — you would be
*bypassing native menus entirely* and building your own popup + renderer,
keeping at most muda's plain data structs (which we already have a better,
domain-specific version of in `Snapshot`).

Verdict: forking muda buys us nothing a custom popup doesn't, and saddles us
with an upstream fork to maintain. Reject. (Sources:
github.com/tauri-apps/muda, docs.rs/muda.)

Note we do **keep** `tray-icon` — but for the *tray icon* (the status-bar
glyph, the title, `set_icon`/`set_title`), not for menus. `tray-icon` also
gives us the anchor rect we need (§4).

### (b) Custom borderless popup WINDOW anchored to the tray icon — RECOMMENDED (see §5)

Draw our own popup and place it under the tray icon. This is how well-designed
menu-bar apps (Ice, Bartender, iStat Menus, etc.) achieve a non-menu look.
Sub-choices — the *shell* (window + anchoring + dismiss) and the *content
renderer* (what draws the rows) are separable decisions:

**Content renderer candidates:**

| Toolkit | Binary | First-frame latency | Dark/HiDPI | Borderless popup | A11y | Maturity |
|---|---|---|---|---|---|---|
| egui/eframe | ~5–15 MB (glow backend smaller) | wgpu ~500 ms GPU init; glow faster | good / strong | `with_decorations(false)`+always-on-top | weak (immediate-mode, no native a11y tree) | very mature, pre-1.0 |
| Slint | 3–4 MB sw (+23 MB Skia) | software renderer skips GPU init | strong / first-class | `no-frame`+always-on-top | partial | vendor-backed; **tri-license GPLv3/paid — disqualifying for us** |
| iced | mid-MB (tiny_skia backend avoids GPU) | CPU backend instant-ish | good / yes | decorations off + level | weak | active, pre-1.0 |
| gpui (Zed) | — | GPU init | strong | via its APIs | — | **macOS+Linux only, Windows experimental; effectively Zed-internal — reject** |
| Tauri (webview) | few MB (OS webview) | **worst: webview cold start, WebView2 first-launch slow** | full (CSS) / full | config flags | full (DOM) | mature; wrong tool for an instant tiny popup |
| winit + wgpu | several MB | full ~500 ms GPU init | manual | native winit attrs | manual | foundational |
| **winit + softbuffer/tiny-skia (CPU raster)** | **smallest** | **best: no GPU stack → effectively instant** | manual (draw every pixel) | native winit attrs | manual | small, stable, permissive |
| **native AppKit views (macOS only)** | ~0 (already linked) | instant | free / free | n/a (native) | **free (NSAccessibility)** | we already use objc2-app-kit |

The decisive constraint is **startup latency**: a tray popup must appear the
instant you click. Any wgpu/GPU-backed toolkit pays a one-time ~500 ms+ GPU
init on first device/surface creation (measured ~576 ms release; cold drivers
worse — gfx-rs/wgpu#6155). Mitigation is always the same: **pre-create a hidden
window at launch and only show/hide on click**, or use a **CPU rasteriser**
that skips GPU init entirely. Tauri is the worst fit (webview cold start).

**Shell mechanics** (winit ≥ 0.31): borderless via `with_decorations(false)`;
top via `WindowLevel::AlwaysOnTop`; no dock icon via
`ActivationPolicy::Accessory`. The hard part is a **non-activating** popup that
doesn't steal focus / bounce the app forward:
- macOS needs a native non-activating `NSPanel` — winit ≥ 0.31 exposes
  `WindowAttributesExtMacOS::with_panel(true)` (PR #4035); must be set at
  create time. A never-key panel **cannot host text fields or keyboard
  shortcuts**, so any text entry (e.g. paste-API-key) stays in a separate
  ordinary window.
- Windows: `WS_EX_NOACTIVATE` (+ `WS_EX_TOOLWINDOW` to skip the taskbar) via
  raw win32; not absolute but good enough.
- Linux: `with_active(false)` is **unsupported on X11/Wayland**; see §4.

### (c) Native-per-OS custom views (NSView + GTK + Win32) — REJECTED as the primary plan

Full control everywhere, but 3× the drawing/layout/hit-testing/dismiss code,
in three languages of API (AppKit, GTK, Win32), each with its own text and
theming model. usagio is a small tool; the maintenance cost is not justified
when one platform (macOS) carries the "look good" mandate and the other two are
already functional with native menus. **However**, note the recommendation in
§5 deliberately uses native AppKit *for the macOS content* — that is not
approach (c) (which means doing all three natively); it is choosing the
right-for-one-platform renderer inside approach (b)'s popup model, while
Linux/Windows keep their existing native menus.

## 4. The hard cross-OS problems

### Anchoring the popup to the tray icon

- **macOS: solved, natively.** `NSStatusItem.button` is both the click target
  and the anchor. The idiomatic path is `NSPopover.show(relativeTo:of:
  preferredEdge:)` — **AppKit computes the icon's on-screen position for us**,
  including which display the menu bar is on. `tray-icon` also exposes
  `ns_status_item()` and reports the icon `rect` if we place a window
  ourselves.
- **Windows: solved.** `tray-icon`'s `TrayIconEvent`/`TrayIcon::rect()` returns
  the `Shell_NotifyIcon` rect (`Rect { position, size }` in physical px). Feed
  that to `set_outer_position`. (`tauri-plugin-positioner`'s `TrayBottomCenter`
  does exactly this on Win/mac.)
- **Linux: NOT SOLVABLE through this stack — the known-hard case.** The Linux
  tray is `StatusNotifierItem`/AppIndicator over D-Bus: the *host* (GNOME
  extension, KDE plasmoid, xembed shim) owns and draws the icon in its own
  process and renders the menu via `com.canonical.dbusmenu`. The application is
  **never told where the icon is on screen and never receives the click**.
  `tray-icon` documents this explicitly: `TrayIconEvent` — "Linux: Unsupported.
  The event is not emitted"; `rect()` — "Linux: Unsupported." There is no icon
  rectangle to query and no click coordinate delivered. **Conclusion: a
  tray-anchored popup is architecturally impossible on Linux.** Linux options
  are (1) keep the native SNI/dbusmenu menu — recommended; (2) a
  non-anchored popup at a fixed/screen-edge position; or (3) a
  `wlr-layer-shell` client on wlroots/KWin (not GNOME, and winit doesn't wrap
  it). We choose (1).

### Dismiss-on-click-outside / focus loss

- macOS `NSPopover` with `behavior = .transient` auto-dismisses for free; for a
  hand-placed panel, add a global `NSEvent` monitor
  (`addGlobalMonitorForEvents`) since a non-key panel won't emit normal
  focus-loss.
- winit's `WindowEvent::Focused(false)` is a reasonable hook for *activating*
  popups on Win/mac, but **unreliable for genuine non-activating panels** —
  back it with an OS global outside-click monitor (macOS global monitor,
  Windows mouse hook). Linux/Wayland focus events are the least reliable.

### Multi-monitor / which-display-the-menu-bar-is-on

- macOS `NSPopover`/`NSStatusItem` handle this for us. If we hand-place, derive
  from the status-item screen. Windows: the tray rect already encodes the
  correct monitor. Linux: N/A (no anchoring).

### Dark mode / system accent

- Native AppKit content follows appearance and `NSColor` semantic colours
  (`labelColor`, `secondaryLabelColor`, `systemRed/Orange`) automatically —
  we already use these in `mac_style`. A CPU-raster/toolkit path must query the
  OS theme (winit `Theme`, macOS `effectiveAppearance`) and repaint on change
  (`WindowEvent::ThemeChanged`).

### Keyboard nav + screen-reader accessibility

- **This is the biggest thing native menus give us for free** and a custom
  popup risks losing. Native `NSMenu` exposes a full accessibility tree and
  keyboard navigation. Mitigations, best → worst:
  - **Native AppKit views inside the popover** keep `NSAccessibility` and key
    handling largely for free (each `NSControl`/`NSView` row is accessible).
    This is a strong reason to prefer native content on macOS.
  - A GUI toolkit relies on **AccessKit** (egui/others integrate it) — usable
    but an extra moving part and less complete than native.
  - A raw tiny-skia pixmap is a **black box to screen readers** unless we wire
    AccessKit ourselves. This is the main strike against the pure-raster path.
- We must implement Esc-to-dismiss and arrow/Enter navigation ourselves in any
  custom path.

### Wayland vs X11

- **X11:** a client can set absolute global coordinates — anchoring would work
  *if we had the rect*, which on Linux we don't (see above).
- **Wayland:** a client **cannot set its own toplevel's global screen
  position, by design** (winit `set_outer_position` is a no-op on Wayland), and
  can't set always-on-top or non-activation. Menus are done via
  `xdg_positioner` popups *relative to your own surface* (not a panel-owned
  tray icon), or `wlr-layer-shell` (compositor-permitting, not GNOME).
- Net: Linux/Wayland reinforces "keep the native SNI menu on Linux."

## 5. Recommended starting point

**Approach (b), macOS-first, as an `NSPopover` anchored to the
`NSStatusItem` button, with content built from a small tree of native AppKit
views driven by the existing `Snapshot`. Keep the native `muda` menu on
Linux/Windows as the shipping fallback. Extend to a `tiny-skia`/winit anchored
popup on Windows in a later phase; treat Linux as native-menu-only because the
tray geometry does not exist.**

Why this and not a single cross-platform toolkit:

1. **It targets the platform that actually carries the "look good" mandate.**
   The hero screenshot is macOS; that's where the design pressure is.
2. **It reuses the team's existing investment.** We already link
   `objc2`/`objc2-app-kit` and already drive `NSMenu`, `NSAttributedString`,
   `NSColor`, `NSFont`, and status-item plumbing in `mac_style`. Custom
   `NSView` rows are the same toolbox, not a new dependency tree.
3. **It's the most robust option**, precisely because the hard cross-OS
   problems (§4) — anchoring, transient dismiss, multi-monitor, dark mode,
   accessibility, keyboard nav — are all **solved for free by AppkKit** on
   macOS via `NSPopover`/`NSStatusItem`, instead of being re-implemented per
   OS. Zero GPU-init latency; the popover is instant.
4. **It directly kills the current pain**: no chevron column, no reserved
   submenu margin, no tab-stop fudge constants — we draw a real two-column
   layout, progress bars, per-account detail cards, whatever we want.
5. **Linux honesty:** since Linux can't anchor a popup to the tray at all, a
   "one unified custom renderer" plan would still need a Linux special-case.
   The unified toolkit doesn't actually buy a unified Linux result.

The alternative — **if the maintainer insists on one shared custom renderer
across all three OSes** — is `winit ≥ 0.31 + softbuffer/tiny-skia` (CPU raster,
smallest binary, no GPU-init latency), pre-warmed hidden window, with a shared
"scene" drawer. Accept: hand-rolled theming/HiDPI/text, AccessKit for
accessibility, and Linux falling back to a fixed/centered (non-anchored) window
or the native menu anyway. This is more code and weaker a11y than the native
macOS path, for a marginal drawing-code-sharing win; recommended only if
cross-platform *visual identity* becomes a hard product requirement.

### Module structure

Introduce `src/ui/` and split the render layer out of `menubar.rs` without
touching the data layer:

```
src/menubar.rs          // KEEP: build_snapshot(), Snapshot/ProviderSection/
                        //   AcctView/WindowView, ordering, countdown/locked,
                        //   handle_click(id), poller thread, run() dispatch.
                        //   `mod mac_style` (NSMenu) stays as the fallback path.
                        //   `mod cross_platform` (muda) stays as-is.
src/ui/
├── mod.rs              // NEW: toolkit-neutral view-model. `fn view_from_
│                       //   snapshot(&Snapshot) -> PopoverModel`. PopoverModel
│                       //   is the clean evolution of RowStyle: groups →
│                       //   account rows (display, active, checkmark, two
│                       //   percentages each with a Severity band, locked
│                       //   countdown, detail card fields) → action rows,
│                       //   each carrying the SAME click-id string
│                       //   handle_click already understands.
├── mac_popover.rs      // NEW (cfg macos): owns NSStatusItem + NSPopover;
│                       //   builds NSView rows from PopoverModel; on click
│                       //   sends the id into the existing handle_click();
│                       //   refreshes on the same 0.75s snapshot tick.
├── scene.rs            // OPTIONAL (later, for the raster path): tiny-skia
│                       //   drawing of PopoverModel, shared by win/linux.
└── win_popup.rs        // NEW (later, cfg windows): borderless WS_EX_NOACTIVATE
                        //   window anchored via tray-icon rect(); draws scene.rs.
```

`view_from_snapshot` is the seam. It replaces `menu_styles`'s macOS-specific
`RowStyle` list with a render-target-neutral model, but derives from the exact
same `Snapshot` and reuses `Severity`/`SeverityBands`, `trailing_for_account`,
`locked_countdown_for`, `provider_grouped_order`, etc. **No provider,
aggregation, ordering, or click-routing code moves.**

### Coexistence with today's split

`menubar::run()` already branches macOS vs non-macOS. On macOS, gate the popup
behind a flag (§6): flag off → today's `mac_style::install_menu` NSMenu path;
flag on → `ui::mac_popover`. Both consume the same `build_snapshot()` output
and the same `handle_click`, so they are drop-in interchangeable and can live
side by side through the whole migration.

## 6. Migration path

Ship incrementally, never breaking the working native menu:

1. **Phase 0 — seam only (no behaviour change).** Add `src/ui/mod.rs` with
   `view_from_snapshot` + `PopoverModel`, plus tests that it reproduces the
   current rows/colours/active-marking from a fixture `Snapshot`. Nothing wired
   to a window yet. Native menu still the only UI.
2. **Phase 1 — macOS popover behind a Cargo feature `custom-popup` (default
   off).** Implement `ui::mac_popover`. `run()` picks popover vs NSMenu on the
   feature. Dogfood via `cargo run --features custom-popup`. Native NSMenu
   remains the shipped default and the fallback.
3. **Phase 2 — flip macOS default** to the popover once it reaches parity
   (all rows, subm*card* content, switch/launch/remove/capture/settings/quit,
   dark mode, keyboard + VoiceOver). Keep the NSMenu path compiled behind
   `--no-default-features`/an env kill-switch for one release as a safety net.
4. **Phase 3 (optional, later) — Windows** anchored `tiny-skia` popup via
   `ui::win_popup` + `ui::scene`, same feature gate. Linux stays on the native
   muda menu indefinitely (anchoring impossible); revisit only if a
   layer-shell story matures.

macOS first because it's where the win is, where we have the tooling, and where
the platform does the hard parts for us. Native menu stays as fallback at every
step.

## 7. Robustness risks and effort estimate

**Risks**

- **Linux can't be anchored (highest-impact constraint, not a bug we can
  fix).** Any messaging of "custom UI everywhere" must carve out Linux. Plan
  keeps Linux native — accept the visual divergence.
- **macOS activation flakiness.** `NSApp.activate(ignoringOtherApps:)` is
  deprecated and unreliable on recent macOS (~85%); a non-key `NSPanel` won't
  emit normal focus-loss. Mitigate with `NSPopover.transient` + a global event
  monitor; prefer `NSPopover` over a hand-placed panel specifically to avoid
  this.
- **Accessibility regression.** Native menus give VoiceOver + keyboard nav
  free. Native AppKit content preserves most of it; a raster/toolkit path needs
  AccessKit. Gate the Phase-2 default flip on a VoiceOver pass.
- **Maintenance:** custom drawing = we now own layout, hit-testing, dismiss,
  theme-change repaint, and HiDPI that the OS handled. Contained on macOS by
  using native views.
- **GPU/startup latency** if a wgpu toolkit is ever chosen — avoided by the
  native (macOS) and CPU-raster (Windows) choices.

**Effort (rough t-shirt)**

- macOS `NSPopover` + native rows (Phases 0–2): **M** (the interesting work is
  the detail *card* layout and pixel-polish, not plumbing — anchoring/dismiss
  are near-free).
- Windows anchored raster popup (Phase 3): **M–L** (window + `WS_EX_NOACTIVATE`
  + tray-rect anchor + tiny-skia scene + theme + dismiss, all hand-rolled).
- Linux custom popup: **L and not worth it** — keep native menu (**S**).

## 8. Summary recommendation

Build a custom tray-anchored **popup**, not a muda fork (muda has no drawable
layer to fork). Start **macOS-first** with an **`NSPopover` anchored to the
`NSStatusItem` button, drawn with native AppKit views** fed by the existing
`Snapshot` through a new `src/ui/` view-model seam; keep the native menu on
Linux/Windows as the shipped fallback and behind a feature flag on macOS until
parity. The single biggest risk is **Linux: tray-icon geometry and click
events do not exist there (StatusNotifierItem by design), so a tray-anchored
popup is impossible on Linux — it must stay on the native menu.**
