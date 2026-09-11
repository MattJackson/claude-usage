#!/usr/bin/env bash
# Capture the "hero" screenshot set of usagio's menu on macOS, one per fixture
# in packaging/screenshots/fixtures/, using real usagio UI + mocked account
# data (no hand-drawn mockups).
#
# Headless render (issue #59): earlier revisions launched the live app and
# drove `osascript`/System Events to open the dropdown, then `screencapture`d
# the screen. That needs TCC Accessibility permission and a logged-in GUI
# session — which GitHub Actions runners don't have — so every "menu open"
# variant fell through to a blank tray-only frame and the quality gate
# (MIN_PNG_BYTES) correctly rejected the run. usagio now exposes
# `__render_shot <theme> <out.png> <scale>`, which rasterizes the top-level
# menu for a fixture's state.json straight to a PNG via muri's offscreen
# renderer — no display, no accessibility, deterministic. We render each
# fixture with the macOS theme and frame it onto the uniform hero canvas.
#
# The tray and settings variants are intentionally not produced here: the
# website consumes only the menu variants (+ hero, derived from menu-healthy),
# the tray strip / Settings submenu aren't part of the offscreen top-level
# render, and a live capture of them can't run headlessly.
#
# Usage: capture-macos.sh <output-dir>
set -euo pipefail

OUT_DIR="${1:-.}"
mkdir -p "$OUT_DIR"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES_DIR="$SCRIPT_DIR/fixtures"
STATE_DIR="$HOME/.config/usagio"
STATE_FILE="$STATE_DIR/state.json"
BACKUP_FILE=""
THEME="macos"

# name:fixture — one output PNG per entry, each a distinct fixture so the
# within-OS SHA-uniqueness gate passes.
VARIANTS=(
  "menu-healthy:healthy"
  "menu-locked:weekly-locked"
  "menu-session-locked:session-locked"
  "menu-mixed:mixed"
)

cleanup() {
  if [[ -n "$BACKUP_FILE" && -f "$BACKUP_FILE" ]]; then
    mv "$BACKUP_FILE" "$STATE_FILE"
  elif [[ -f "$STATE_FILE" ]]; then
    rm -f "$STATE_FILE"
  fi
}
trap cleanup EXIT

mkdir -p "$STATE_DIR"
if [[ -f "$STATE_FILE" ]]; then
  BACKUP_FILE="$(mktemp "${TMPDIR:-/tmp}/usagio-state-backup.XXXXXX")"
  cp "$STATE_FILE" "$BACKUP_FILE"
fi

# Prefer the freshly-built binary over any (possibly stale) `usagio` on PATH,
# so we always render with the code under test — a stale PATH build may not
# even know the `__render_shot` subcommand.
USAGIO_BIN=""
for cand in ./target/release/usagio ./target/debug/usagio; do
  if [[ -x "$cand" ]]; then
    USAGIO_BIN="$cand"
    break
  fi
done
if [[ -z "$USAGIO_BIN" ]]; then
  USAGIO_BIN="$(command -v usagio || true)"
fi
if [[ -z "$USAGIO_BIN" ]]; then
  echo "error: could not find a built usagio binary (checked PATH, target/release, target/debug)" >&2
  exit 1
fi

for entry in "${VARIANTS[@]}"; do
  IFS=':' read -r name fixture <<< "$entry"

  # Swap the fixture into state.json; __render_shot reads it via build_snapshot.
  python3 "$SCRIPT_DIR/render_fixture.py" "$FIXTURES_DIR/$fixture.json" "$STATE_FILE"

  raw="$OUT_DIR/macos-$name-raw.png"
  "$USAGIO_BIN" __render_shot "$THEME" "$raw" 2

  # Frame the clean menu render onto the uniform 1600x900 hero canvas.
  python3 "$SCRIPT_DIR/postprocess.py" "$raw" "$OUT_DIR/macos-$name.png" \
    --frame --bg "236,236,238"
  rm -f "$raw"
done

echo "Wrote ${#VARIANTS[@]} macOS hero screenshots to $OUT_DIR"
