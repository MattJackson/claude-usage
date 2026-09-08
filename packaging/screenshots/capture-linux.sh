#!/usr/bin/env bash
# Capture the full set of "hero" screenshot variations of usagio's tray icon
# on Linux, one per fixture in packaging/screenshots/fixtures/, using real
# usagio UI + mocked account data (no hand-drawn mockups).
#
# IMPORTANT / reality check: GitHub's ubuntu-latest runners have no desktop
# session at all — no window manager, no system tray host (no
# StatusNotifierWatcher / XEmbed tray). `tray-icon`/`muda` on Linux need a
# tray host to render into; without one, the icon is created successfully
# at the DBus/XEmbed level but there is nothing on screen to photograph,
# and there is no menu to click open. So this script:
#   1. Starts Xvfb (virtual framebuffer), sized so the final crop has
#      plenty of desktop-wallpaper margin around the panel/tray.
#   2. Starts a minimal tray host (`stalonetray`) so the icon has
#      somewhere to dock — a best-effort approximation of a real desktop
#      panel, not what any actual Linux desktop looks like.
#   3. Per fixture: swaps state.json, best-effort `xdotool` clicks the tray
#      icon (and, for "settings", the Settings item) to open the menu, then
#      captures the full virtual screen via ImageMagick `import`.
#
# For a screenshot that actually looks like a real Linux desktop tray
# (GNOME/KDE/XFCE), run this on your own Linux desktop instead of in CI —
# see README.md.
#
# Requires: Xvfb, xdotool, imagemagick (import), stalonetray, python3+Pillow.
# Usage: capture-linux.sh <output-dir>
set -euo pipefail

OUT_DIR="${1:-.}"
mkdir -p "$OUT_DIR"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES_DIR="$SCRIPT_DIR/fixtures"
STATE_DIR="$HOME/.config/usagio"
STATE_FILE="$STATE_DIR/state.json"
BACKUP_FILE=""
export DISPLAY="${DISPLAY:-:99}"
SCREEN_W=1920
SCREEN_H=1080

XVFB_PID=""
TRAY_PID=""
USAGIO_PID=""
RAW_DIR="$(mktemp -d "${TMPDIR:-/tmp}/usagio-shots.XXXXXX")"

VARIANTS=(
  "tray:healthy:tray"
  "menu-healthy:healthy:menu"
  "menu-locked:weekly-locked:menu"
  "menu-session-locked:session-locked:menu"
  "menu-mixed:mixed:menu"
  "settings:healthy:settings"
)

cleanup() {
  for pid in "$USAGIO_PID" "$TRAY_PID" "$XVFB_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
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

if ! xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; then
  Xvfb "$DISPLAY" -screen 0 "${SCREEN_W}x${SCREEN_H}x24" &
  XVFB_PID=$!
  sleep 2
fi

# Docked top-right, matching the macOS menu bar / Windows tray convention
# so the rotation's menu position feels stable across all three OSes.
if command -v stalonetray >/dev/null 2>&1; then
  stalonetray --geometry "200x24+$((SCREEN_W - 210))+0" --icon-size 20 &
  TRAY_PID=$!
  sleep 1
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

cp "$FIXTURES_DIR/healthy.json" "$STATE_FILE"
"$USAGIO_BIN" menubar &
USAGIO_PID=$!
sleep 3

for entry in "${VARIANTS[@]}"; do
  IFS=':' read -r name fixture mode <<< "$entry"

  python3 "$SCRIPT_DIR/render_fixture.py" "$FIXTURES_DIR/$fixture.json" "$STATE_FILE"
  sleep 1.5

  raw="$RAW_DIR/linux-$name-raw.png"
  clicked=0

  tray_x=$((SCREEN_W - 200))
  if [[ "$mode" == "menu" || "$mode" == "settings" ]] && command -v xdotool >/dev/null 2>&1; then
    if xdotool mousemove "$tray_x" 12 click 1 >/dev/null 2>&1; then
      clicked=1
      sleep 0.8
      if [[ "$mode" == "settings" ]]; then
        # Best-effort: nudge down/left toward where a "Settings" row would
        # be in the opened menu and click. Coordinates are a rough guess —
        # there's no accessibility tree to query in this minimal
        # Xvfb+stalonetray setup, unlike a real desktop's tray
        # implementation.
        xdotool mousemove "$((tray_x - 140))" 200 click 1 >/dev/null 2>&1 || true
        sleep 0.5
      fi
    fi
  fi

  if [[ $clicked -eq 0 && ( "$mode" == "menu" || "$mode" == "settings" ) ]]; then
    echo "warn: [$name] could not open the tray menu via xdotool in this headless session; capturing tray-only instead." >&2
  fi

  if command -v import >/dev/null 2>&1; then
    import -display "$DISPLAY" -window root "$raw"
  elif command -v xwd >/dev/null 2>&1 && command -v convert >/dev/null 2>&1; then
    xwd -display "$DISPLAY" -root -silent | convert xwd:- "$raw"
  else
    echo "error: neither ImageMagick 'import' nor 'xwd'+'convert' is available" >&2
    exit 1
  fi

  # There's no window manager to key-close a menu here, so just move the
  # mouse away before the next iteration's screenshot.
  command -v xdotool >/dev/null 2>&1 && xdotool mousemove "$((SCREEN_W - 5))" "$((SCREEN_H - 5))" >/dev/null 2>&1 || true

  # Tray/panel is docked top-right (matches macOS/Windows convention);
  # anchor there with generous margin.
  anchor_y=0.05
  if [[ "$mode" != "tray" ]]; then
    anchor_y=0.22
  fi
  python3 "$SCRIPT_DIR/postprocess.py" "$raw" "$OUT_DIR/linux-$name.png" \
    --anchor-x 0.78 --anchor-y "$anchor_y" --crop-width-frac 0.62 --bg "45,45,45"
done

echo "Wrote ${#VARIANTS[@]} Linux hero screenshots to $OUT_DIR"
