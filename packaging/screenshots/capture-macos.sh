#!/usr/bin/env bash
# Capture the full set of "hero" screenshot variations of usagio's menu bar
# on macOS, one per fixture in packaging/screenshots/fixtures/, using real
# usagio UI + mocked account data (no hand-drawn mockups).
#
# IMPORTANT / reality check: on a headless GitHub Actions macOS runner,
# clicking the menu bar item to *open the dropdown* requires TCC
# Accessibility permission for the process driving `osascript`/System
# Events, which CI runners do not grant by default and which cannot be
# granted non-interactively without a logged-in GUI session accepting a
# permission prompt. So for every "menu open" variant this script:
#   1. Always captures a full-screen shot first (works headlessly).
#   2. BEST-EFFORT attempts to open the dropdown (and, for the "settings"
#      variant, drill into the Settings submenu) via System Events before
#      that full-screen capture. If the click is denied, the capture still
#      succeeds — it just shows the tray icon rather than an open menu, and
#      a warning is logged. Run this script locally on a real Mac with
#      Accessibility access granted for guaranteed open-menu shots.
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
USAGIO_PID=""
RAW_DIR="$(mktemp -d "${TMPDIR:-/tmp}/usagio-shots.XXXXXX")"

# name:fixture:mode — mode is one of tray | menu | settings.
VARIANTS=(
  "tray:healthy:tray"
  "menu-healthy:healthy:menu"
  "menu-locked:weekly-locked:menu"
  "menu-session-locked:session-locked:menu"
  "menu-mixed:mixed:menu"
  "settings:healthy:settings"
)

cleanup() {
  if [[ -n "$USAGIO_PID" ]] && kill -0 "$USAGIO_PID" 2>/dev/null; then
    kill "$USAGIO_PID" 2>/dev/null || true
    wait "$USAGIO_PID" 2>/dev/null || true
  fi
  if [[ -n "$BACKUP_FILE" && -f "$BACKUP_FILE" ]]; then
    mv "$BACKUP_FILE" "$STATE_FILE"
  elif [[ -f "$STATE_FILE" ]]; then
    rm -f "$STATE_FILE"
  fi
  rm -rf "$RAW_DIR"
}
trap cleanup EXIT

mkdir -p "$STATE_DIR"
if [[ -f "$STATE_FILE" ]]; then
  BACKUP_FILE="$(mktemp "${TMPDIR:-/tmp}/usagio-state-backup.XXXXXX")"
  cp "$STATE_FILE" "$BACKUP_FILE"
fi

USAGIO_BIN="$(command -v usagio || true)"
if [[ -z "$USAGIO_BIN" ]]; then
  for cand in ./target/release/usagio ./target/debug/usagio; do
    if [[ -x "$cand" ]]; then
      USAGIO_BIN="$cand"
      break
    fi
  done
fi
if [[ -z "$USAGIO_BIN" ]]; then
  echo "error: could not find a built usagio binary (checked PATH, target/release, target/debug)" >&2
  exit 1
fi

# Launch usagio ONCE and swap state.json under it for each variant — usagio
# mtime-checks state.json on every ~0.75s menu tick (see
# src/menubar.rs::cached_state), so a fresh menu open after the mtime
# changes always reflects the newest fixture without a relaunch.
cp "$FIXTURES_DIR/healthy.json" "$STATE_FILE"
"$USAGIO_BIN" menubar &
USAGIO_PID=$!
sleep 3

for entry in "${VARIANTS[@]}"; do
  IFS=':' read -r name fixture mode <<< "$entry"

  python3 "$SCRIPT_DIR/render_fixture.py" "$FIXTURES_DIR/$fixture.json" "$STATE_FILE"
  sleep 1.5 # let cached_state() pick up the new mtime before we open the menu

  raw="$RAW_DIR/macos-$name-raw.png"
  clicked=0

  if [[ "$mode" == "menu" || "$mode" == "settings" ]]; then
    set +e
    osascript <<'APPLESCRIPT'
tell application "System Events"
    tell process "usagio"
        set frontmost to true
        click menu bar item 1 of menu bar 2
    end tell
end tell
APPLESCRIPT
    click_status=$?
    set -e
    if [[ $click_status -eq 0 ]]; then
      clicked=1
      if [[ "$mode" == "settings" ]]; then
        sleep 0.5
        set +e
        osascript -e 'tell application "System Events" to tell process "usagio" to click menu item "Settings" of menu 1 of menu bar item 1 of menu bar 2' >/dev/null 2>&1
        set -e
      fi
      sleep 0.8
    else
      echo "warn: [$name] could not open the menu (likely missing Accessibility permission in this session); capturing tray-only instead." >&2
    fi
  fi

  # Full-screen capture — postprocess.py crops down to the framed,
  # OS-consistent 1600x900 hero frame. See "Capture quality standards" in
  # README.md: wide desktop context, no tight crop, uniform menu position.
  screencapture -x "$raw"

  if [[ $clicked -eq 1 ]]; then
    # Dismiss whatever's open so the next variant starts from a clean state.
    osascript -e 'tell application "System Events" to key code 53' >/dev/null 2>&1 || true
    osascript -e 'tell application "System Events" to key code 53' >/dev/null 2>&1 || true
  fi

  # macOS menu bar sits top-right. The tray-only variant anchors right at
  # the strip (small margin above); menu/settings variants anchor lower so
  # the open dropdown, which hangs well below the menu bar, stays in frame.
  anchor_y=0.28
  if [[ "$mode" == "tray" ]]; then
    anchor_y=0.05
  fi
  python3 "$SCRIPT_DIR/postprocess.py" "$raw" "$OUT_DIR/macos-$name.png" \
    --anchor-x 0.78 --anchor-y "$anchor_y" --crop-width-frac 0.62 --bg "246,246,246"
done

echo "Wrote ${#VARIANTS[@]} macOS hero screenshots to $OUT_DIR"
