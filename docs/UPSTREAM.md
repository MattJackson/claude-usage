# Upstream PRs

Two fixes are filed upstream. Until they merge **and ship in a published crate**,
this repo ships its own native workaround, so releases never depend on an
unmerged fork.

| PR | What it adds | Our workaround today |
|---|---|---|
| [tauri-apps/tao#1325](https://github.com/tauri-apps/tao/pull/1325) (issue [#1324](https://github.com/tauri-apps/tao/issues/1324)) | Stop registering the run-loop observer/timer in `NSEventTrackingRunLoopMode` (open status-menu was auto-dismissed) | We dropped `tao` and drive a native `NSApplication` loop with a default-mode `NSTimer` (`src/menubar.rs`). Likely permanent even if merged. |
| [tauri-apps/muda#399](https://github.com/tauri-apps/muda/pull/399) | `MenuItem::set_attributed_title` for custom `NSAttributedString` labels | We build the muda menu, then walk the native `NSMenu` via `ns_menu()` and set attributed titles with objc2 (`apply_menu_styles` in `src/menubar.rs`). |

When #399 lands in a released `muda`, the ~80-line objc2 helper in
`apply_menu_styles` can be replaced with a couple of `set_attributed_title`
calls (and the extra `objc2-app-kit` features dropped).
