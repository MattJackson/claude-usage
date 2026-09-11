# Screenshots

Automation for capturing real usagio menu screenshots — on macOS, Linux, and
Windows — against fixed mocked account fixtures, so the images are reproducible
regardless of the developer's real account state. **No hand-drawn mockups are
used anywhere in this pipeline**: every image is a real render of usagio's menu
with account data swapped in via `state.json`.

This is a fully automatic, end-to-end pipeline: **app updates -> push to `qa`
or a release tag -> CI renders fresh screenshots -> website updated** — no
manual download/copy step anywhere in the chain.

## How it works: headless offscreen render

Capture is deterministic and headless. Each OS leg runs

```
usagio __render_shot <theme> <out.png> <scale>
```

which rasterizes the **top-level menu** for whatever is in `state.json`
straight to a PNG via muri's offscreen renderer (muri issue #59) — no live
tray, no accessibility permission, no display or window manager. `<theme>` is
`macos` / `gnome` / `windows`, so each OS leg renders its own native look. The
render already includes the themed background, rounded corners, per-severity
value colors, and the bold active row; `postprocess.py --frame` just centers it
on the uniform website canvas.

This replaced an earlier approach that launched the live `usagio menubar` and
tried to open the dropdown with `screencapture`/`osascript` (macOS),
Xvfb+`xdotool`+`stalonetray` (Linux), and `SendKeys`+`CopyFromScreen`
(Windows). None of that works on a headless CI runner — there is no TCC
Accessibility grant, no real display, and no tray focus — so every menu variant
fell through to a blank or duplicate frame that the quality gate correctly
rejected. The offscreen renderer removes the whole class of problem.

## Files

- `fixtures/*.json` — one fixture per account-state variation (healthy,
  weekly-locked, session-locked, mixed). See `fixtures/README.md` for what each
  demonstrates and which website surface consumes it.
- `render_fixture.py` — resolves a fixture's `NOW+<duration>` countdown tokens
  (e.g. `NOW+2d14h`) to an absolute timestamp right before rendering, then
  writes the result to `state.json`. Needed because usagio's `countdown.rs`
  only shows the locked-account countdown UI when the reset time is still in the
  future — a static date baked into a fixture would go stale.
- `postprocess.py` — `--frame` mode centers a clean menu render (RGBA,
  compositing through its own alpha mask so the rounded corners stay clean) onto
  the uniform 1600x900 website canvas with a neutral background. (The legacy
  crop mode — `--anchor-x/--anchor-y/--crop-width-frac` — is retained for
  cropping down a full-desktop capture, but the pipeline no longer uses it.)
- `capture-macos.sh`, `capture-linux.sh`, `capture-windows.ps1` — per-OS
  scripts. Each loops the menu fixtures: writes the fixture into `state.json`
  (via `render_fixture.py`), runs `usagio __render_shot <theme>`, frames the
  result, and cleans up. Each backs up and restores whatever `state.json`
  existed beforehand. They prefer the freshly-built `target/release` binary over
  any `usagio` already on `PATH`, so a stale install can't render the wrong
  code.

## Variants

Four per OS (`<os>` is `macos` / `linux` / `windows`), 12 screenshots total per
run:

1. `<os>-menu-healthy.png` — all accounts healthy.
2. `<os>-menu-locked.png` — one account weekly-locked (red countdown, e.g.
   `"2d 14h"`).
3. `<os>-menu-session-locked.png` — one account session-locked with weekly
   still healthy (red countdown, e.g. `"3h 15m"`).
4. `<os>-menu-mixed.png` — one healthy + one session-locked + one weekly-locked
   + one needing re-login, all at once (also shows the amber/red per-window
   value banding).

`hero-<os>.png` is refreshed from each OS's `menu-healthy` variant.

The former `tray` and `settings` variants were dropped: the website consumes
only the menu variants (plus the derived hero), and neither the tray strip nor
the Settings submenu is part of the offscreen top-level render.

## Framing standard

`postprocess.py --frame` produces a **consistent 1600x900** image for every
OS/variant: the menu render is centered on a neutral canvas with a uniform
margin, scaled down with Lanczos resampling only if it would otherwise exceed
the margin box (never upscaled past 1:1). Uniform dimensions keep the site's
3-OS hero rotation from jumping in size between slides, and the ~50-60KB PNGs
comfortably clear the pipeline's `MIN_PNG_BYTES` (20000) gate.

## CI flow

`.github/workflows/screenshots.yml` has two job stages:

1. `capture` — runs on every push to `qa` and on every `vX.Y.Z` release tag
   (plus manual `workflow_dispatch` for one-off test runs), builds `usagio` in
   release mode on a `[macos-latest, ubuntu-latest, windows-latest]` matrix,
   runs the matching capture script (4 menu variants), and uploads the 4
   resulting PNGs as a workflow artifact (`screenshots-macos`,
   `screenshots-linux`, `screenshots-windows`).
2. `commit-screenshots` — runs once all three `capture` legs finish, for real
   push triggers only (a `qa` push or a release tag — not `workflow_dispatch`,
   so poking the button in the Actions tab can't accidentally redeploy the live
   site). Downloads all three artifacts (12 files), runs the two quality gates
   (see below), copies each into `web/public/screenshots/<os>-<variant>.png`,
   refreshes `hero-<os>.png` from `menu-healthy`, and commits + pushes straight
   to **`main`** as `github-actions[bot]`. `main` — not `qa` — because that's
   the branch `.github/workflows/pages.yml`'s `deploy` job actually redeploys
   the live site from. If the captured PNGs are byte-identical to what `main`
   already has, the job skips the commit/push instead of creating no-op noise.

### Quality gates

Two gates stand between a bad capture leg and `main` (better a red CI run than a
green run committing garbage the site then serves as its hero):

- **`MIN_PNG_BYTES` (20000)** — rejects a blank/solid-color frame. Any real menu
  render is a >20KB PNG.
- **Within-OS SHA256 uniqueness** — rejects the "same frame emitted under N
  names" class of bug. The 4 menu variants use 4 distinct fixtures, so their
  renders must differ.

**One-time repo setup required:** the `commit-screenshots` job needs
`contents: write` to push, which only works if this repo's **Settings > Actions
> General > Workflow permissions** is set to **"Read and write permissions"**
(GitHub defaults new repos to read-only). If it's still read-only,
`commit-screenshots` fails on `git push` and you'll need to flip that setting
once.

**Deliberately no `[skip ci]` marker on the bot's commit.** GitHub's native
skip-ci handling suppresses *every* push-triggered workflow for a commit
carrying that marker — including pages.yml's own push-to-`main` deploy trigger,
which is the "website updated" step this whole pipeline exists for. There's no
loop to guard against: this job only pushes to `main`, and neither this workflow
nor pages.yml re-triggers `screenshots.yml` (which only listens for `qa` pushes
and `vX.Y.Z` tags).

## Running locally

```sh
cargo build --release --all-features
pip install pillow   # needed by postprocess.py

# macOS
chmod +x packaging/screenshots/capture-macos.sh
packaging/screenshots/capture-macos.sh ./out

# Linux
chmod +x packaging/screenshots/capture-linux.sh
packaging/screenshots/capture-linux.sh ./out

# Windows (PowerShell)
./packaging/screenshots/capture-windows.ps1 -OutDir .\out
```

The render is headless, so any OS's look can be produced from any host (the
scripts just default each to its own native theme). Each script backs up and
restores your real `~/.config/usagio/state.json`
(`%APPDATA%\usagio\state.json` on Windows) around the run.
