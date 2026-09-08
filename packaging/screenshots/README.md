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

This is a fully automatic, end-to-end pipeline: **app updates -> push to
`qa` or a release tag -> CI captures fresh screenshots -> website
updated** — no manual download/copy step anywhere in the chain.

`.github/workflows/screenshots.yml` has two job stages:

1. `capture` — runs on every push to `qa` and on every `vX.Y.Z` release
   tag (plus manual `workflow_dispatch` for one-off test runs), builds
   `usagio` in release mode on a `[macos-latest, ubuntu-latest,
   windows-latest]` matrix, runs the matching capture script, and uploads
   the result as a workflow artifact (`hero-macos`, `hero-linux`,
   `hero-windows`).
2. `commit-hero-images` — runs once all three `capture` legs finish, for
   real push triggers only (a `qa` push or a release tag — not
   `workflow_dispatch`, so poking the button in the Actions tab can't
   accidentally redeploy the live site). Downloads all three artifacts,
   copies the primary `hero-<os>.png` from each into
   `web/public/hero-<os>.png`, and commits + pushes straight to **`main`**
   as `github-actions[bot]`. `main` — not `qa` — because that's the branch
   `.github/workflows/pages.yml`'s `deploy` job actually redeploys the
   live site from; pushing there is what makes "website updated" the
   final, automatic step. If the captured PNGs are byte-identical to what
   `main` already has, the job skips the commit/push (logs "no image
   change; skipping commit") instead of creating no-op noise commits.

**One-time repo setup required:** the `commit-hero-images` job needs
`contents: write` to push, which only works if this repo's **Settings >
Actions > General > Workflow permissions** is set to **"Read and write
permissions"** (GitHub defaults new repos to read-only). This is a
one-time dashboard setting, not something the workflow file itself can
grant — if it's still on the read-only default, `commit-hero-images` will
fail on `git push` with a permission error and you'll need to flip that
setting once.

Because not every OS reliably produces a menu-open screenshot in headless
CI (see "Reality check" below), an automatic run can legitimately refresh
`web/public/hero-linux.png` with a tray-icon-only fallback image rather
than an open dropdown. If you want a hand-curated, guaranteed-good hero
image instead, capture it locally (see "Running locally" below) and
commit it directly to `web/public/` on `main` yourself — a manual commit
after the bot's auto-commit simply wins since it lands later in `main`'s
history.

**Deliberately no `[skip ci]` marker on the bot's commit.** GitHub's
native skip-ci handling suppresses *every* push-triggered workflow run
for a commit carrying that marker — including pages.yml's own
push-to-`main` deploy trigger, which would silently break the "website
updated" step this whole pipeline exists for. There's no loop to guard
against by omitting it: this job only ever pushes to `main`, and neither
this workflow nor pages.yml re-triggers `screenshots.yml` (which only
listens for `qa` pushes and `vX.Y.Z` tags).

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
