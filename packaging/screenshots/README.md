# Hero screenshots

Automation for rendering "hero" screenshots of usagio's menu bar / tray
icon on macOS, Linux, and Windows, using a fixed mocked account fixture so
the images look identical regardless of the developer's real account
state.

## Files

- `demo-state.json` — a fixed `state.json` fixture: 2 Claude accounts
  (`demo1@example.com` at 47% session / 89% weekly, `dev@example.com` at
  2% session / 98% weekly) and 3 Codex accounts (`alice@codex.example`,
  `bob@codex.example`, and `carol.locked@codex.example`, the last with
  `needs_relogin: true` to show the re-login row). All emails, tokens
  (`demo-*`), and UUIDs are obviously fake placeholders — this file is not
  a secret and is safe to commit.
- `capture-macos.sh`, `capture-linux.sh`, `capture-windows.ps1` — per-OS
  capture scripts. Each: backs up any existing `state.json`, drops in
  `demo-state.json`, launches `usagio menubar`, waits ~3s for tray init,
  attempts to open the menu and screenshot it, then kills usagio and
  restores the original `state.json`.

## CI flow

`.github/workflows/screenshots.yml` runs on every push to `qa` (plus
manual `workflow_dispatch`), builds `usagio` in release mode on a
`[macos-latest, ubuntu-latest, windows-latest]` matrix, runs the matching
capture script, and uploads the result as a workflow artifact
(`hero-macos`, `hero-linux`, `hero-windows`).

**This workflow is artifact-only — it does not commit anything back to
`qa` or into `web/public/`.** That's a deliberate choice, not a shortcut
we forgot to finish: committing images into the website tree is a
separate, visible change to a different concern, and (per the "reality
check" below) not every OS reliably produces a menu-open screenshot in
headless CI, so an unattended auto-commit risks silently overwriting a
good hero image with a degraded fallback. To publish a screenshot:

1. Go to the workflow run in the Actions tab.
2. Download the `hero-<os>` artifact you want.
3. Inspect it — confirm it shows the open dropdown, not just the icon.
4. Copy it into `web/public/hero-<os>.png` and commit it yourself (the
   website is out of scope for this automation).

## Reality check: what CI can and can't capture

Capturing an *open* dropdown menu in headless CI is genuinely hard, and
the difficulty is different per OS:

- **macOS**: `screencapture` can only grab screen regions, not the
  dropdown specifically — and opening it via `osascript`/System Events
  requires the process to hold Accessibility permission, which GitHub's
  macOS runners do not grant by default and which can't be granted
  non-interactively. `capture-macos.sh` always produces a menu-bar-strip
  screenshot (works headlessly), and *attempts* a full dropdown capture
  as a best-effort step — if the click is denied, it falls back to
  copying the menu-bar-only image to `hero-macos.png` and logs a warning.
- **Linux**: GitHub's `ubuntu-latest` runners have no desktop session,
  window manager, or tray host at all. `tray-icon`/`muda` need something
  implementing the StatusNotifierItem/XEmbed tray protocol to dock into;
  without one there's nothing on screen to photograph. `capture-linux.sh`
  starts Xvfb plus a minimal tray host (`stalonetray`) as a best-effort
  approximation — it is not what any real Linux desktop looks like, and
  the click-to-open step is the least reliable of the three OSes.
- **Windows**: `windows-latest` runners do run a real interactive desktop
  session, so there's an actual taskbar and tray to capture. But Windows
  auto-collapses inactive tray icons into the overflow flyout, and
  synthesizing a click via `SendKeys`/`mouse_event` at the right screen
  coordinates is flaky. `capture-windows.ps1` always captures the
  taskbar strip and attempts a best-effort flyout capture, falling back
  to the taskbar-only image as `hero-windows.png` if the click didn't
  clearly work.

**Bottom line:** treat the CI-produced screenshots as "menu bar icon is
visible and usagio is running with realistic demo data" proof, not as a
guaranteed polished dropdown shot. For a genuinely good hero image showing
the open dropdown, run the capture script locally on a real desktop
session (see below) where Accessibility/tray permissions already exist
and there's no headless-runner tray-host problem.

## Running locally

```sh
cargo build --release --all-features

# macOS
chmod +x packaging/screenshots/capture-macos.sh
packaging/screenshots/capture-macos.sh ./out

# Linux (needs xvfb, xdotool, imagemagick, stalonetray if you want to
# run it "CI-style"; on a real desktop session you can skip Xvfb/stalonetray
# entirely and just run against your live X11/Wayland session)
chmod +x packaging/screenshots/capture-linux.sh
packaging/screenshots/capture-linux.sh ./out

# Windows (PowerShell)
./packaging/screenshots/capture-windows.ps1 -OutDir .\out
```

Each script backs up and restores your real `~/.config/usagio/state.json`
(`%APPDATA%\usagio\state.json` on Windows) around the capture, but it's
still a good idea to close a running usagio menubar instance first and to
not have anything sensitive in an open account switcher menu when you run
it, since the swap is not perfectly instantaneous.
