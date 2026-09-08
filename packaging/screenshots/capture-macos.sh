#!/usr/bin/env bash
# Capture a "hero" screenshot of usagio's menu bar icon on macOS, using a
# fixed mocked demo-state.json fixture (see demo-state.json in this dir).
#
# IMPORTANT / reality check: on a headless GitHub Actions macOS runner,
# clicking the menu bar item to *open the dropdown* requires TCC
# Accessibility permission for the process driving `osascript`/System
# Events, which CI runners do not grant by default and which cannot be
# granted non-interactively without a logged-in GUI session accepting a
# permission prompt. So this script:
#
#   1. Always captures the menu bar strip (icon + percentage title) —
#      this works headlessly because it's just `screencapture` of a
#      region, no synthetic click required.
#   2. BEST-EFFORT attempts to open the dropdown via System Events and
#      capture the full menu. This step is allowed to fail (TCC denial,
#      no bounds, etc.) without failing the whole script — if it fails,
#      only the menu-bar-only screenshot is produced. Run this script on
#      your own Mac (logged into a real Aqua session with Accessibility
#      access granted to Terminal/osascript) to get the full dropdown
#      screenshot.
#
# Usage: capture-macos.sh <output-dir>
set -euo pipefail

OUT_DIR="${1:-.}"
mkdir -p "$OUT_DIR"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE_DIR="$HOME/.config/usagio"
STATE_FILE="$STATE_DIR/state.json"
BACKUP_FILE=""

cleanup() {
  # Kill usagio if still running.
  if [[ -n "${USAGIO_PID:-}" ]] && kill -0 "$USAGIO_PID" 2>/dev/null; then
    kill "$USAGIO_PID" 2>/dev/null || true
    wait "$USAGIO_PID" 2>/dev/null || true
  fi
  # Restore whatever state.json existed before we ran (or remove the demo
  # one if there was nothing there originally).
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
cp "$SCRIPT_DIR/demo-state.json" "$STATE_FILE"

# Locate the built binary. Prefer release, fall back to debug.
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

"$USAGIO_BIN" menubar &
USAGIO_PID=$!

# Give the tray icon time to register with the system status bar.
sleep 3

# 1) Menu-bar-only capture: the top-right strip of the primary display,
#    where a status-bar item lands. Adjust the region if your display
#    resolution differs; on GitHub's macOS runners this covers the status
#    bar comfortably.
screencapture -x -R0,0,1600,24 "$OUT_DIR/hero-macos-menubar.png"

# 2) Best-effort dropdown capture. Requires Accessibility permission for
#    the process running this script (System Settings > Privacy & Security
#    > Accessibility). Will silently no-op on CI.
set +e
osascript <<'APPLESCRIPT'
tell application "System Events"
    tell process "usagio"
        set frontmost to true
        click menu bar item 1 of menu bar 2
    end tell
end tell
APPLESCRIPT
CLICK_STATUS=$?
set -e

if [[ $CLICK_STATUS -eq 0 ]]; then
  sleep 1
  screencapture -x -R0,0,1600,700 "$OUT_DIR/hero-macos.png"
  # Dismiss the open menu so we don't leave the UI in a weird state.
  osascript -e 'tell application "System Events" to key code 53' >/dev/null 2>&1 || true
else
  echo "warn: could not open the menu bar dropdown (likely missing Accessibility permission in this session)." >&2
  echo "warn: falling back to menu-bar-only screenshot as hero-macos.png." >&2
  cp "$OUT_DIR/hero-macos-menubar.png" "$OUT_DIR/hero-macos.png"
fi

echo "Wrote $OUT_DIR/hero-macos.png (and hero-macos-menubar.png)"
