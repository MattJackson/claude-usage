#!/usr/bin/env bash
# Capture a "hero" screenshot of usagio's tray icon on Linux, using a fixed
# mocked demo-state.json fixture (see demo-state.json in this dir).
#
# IMPORTANT / reality check: GitHub's ubuntu-latest runners have no desktop
# session at all — no window manager, no system tray host (no
# StatusNotifierWatcher / XEmbed tray). `tray-icon`/`muda` on Linux need a
# tray host to render into; without one, the icon is created successfully
# at the DBus/XEmbed level but there is nothing on screen to photograph,
# and there is no "menu bar" to click open. So this script:
#
#   1. Starts Xvfb (virtual framebuffer) so there's an X server at all.
#   2. Starts a minimal tray host (`stalonetray`) so the icon has
#      somewhere to dock — this is a best-effort approximation of a real
#      desktop panel, not what any actual Linux desktop looks like.
#   3. Launches usagio, waits for tray init, then uses `xdotool` to try to
#      click the tray icon and `import` (ImageMagick) to capture the root
#      window.
#   4. If any of the tray-hosting/click steps fail (very possible — tray
#      protocols vary a lot across DBus/XEmbed implementations and CI
#      environments), falls back to a plain screenshot of the virtual
#      desktop with usagio running in the background, clearly documented
#      as "process running" rather than "menu open".
#
# For a screenshot that actually looks like a real Linux desktop tray
# (GNOME/KDE/XFCE), run this on your own Linux desktop instead of in CI —
# see README.md.
#
# Requires: Xvfb, xdotool, imagemagick (import/xwd), stalonetray.
# Usage: capture-linux.sh <output-dir>
set -euo pipefail

OUT_DIR="${1:-.}"
mkdir -p "$OUT_DIR"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE_DIR="$HOME/.config/usagio"
STATE_FILE="$STATE_DIR/state.json"
BACKUP_FILE=""
export DISPLAY="${DISPLAY:-:99}"

XVFB_PID=""
TRAY_PID=""
USAGIO_PID=""

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
}
trap cleanup EXIT

mkdir -p "$STATE_DIR"
if [[ -f "$STATE_FILE" ]]; then
  BACKUP_FILE="$(mktemp "${TMPDIR:-/tmp}/usagio-state-backup.XXXXXX")"
  cp "$STATE_FILE" "$BACKUP_FILE"
fi
cp "$SCRIPT_DIR/demo-state.json" "$STATE_FILE"

if ! xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; then
  Xvfb "$DISPLAY" -screen 0 1600x900x24 &
  XVFB_PID=$!
  sleep 2
fi

# Minimal tray host so tray-icon/muda has somewhere to dock the icon.
if command -v stalonetray >/dev/null 2>&1; then
  stalonetray --geometry 200x24+0+0 --icon-size 20 &
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

"$USAGIO_BIN" menubar &
USAGIO_PID=$!
sleep 3

# Best-effort: try to click the tray icon area to open the menu, then
# capture the whole virtual screen (there's no window manager to isolate
# just the menu, so this is the full root window).
CLICK_OK=0
if command -v xdotool >/dev/null 2>&1; then
  if xdotool mousemove 10 12 click 1 >/dev/null 2>&1; then
    CLICK_OK=1
    sleep 1
  fi
fi

if command -v import >/dev/null 2>&1; then
  import -display "$DISPLAY" -window root "$OUT_DIR/hero-linux.png"
elif command -v xwd >/dev/null 2>&1 && command -v convert >/dev/null 2>&1; then
  xwd -display "$DISPLAY" -root -silent | convert xwd:- "$OUT_DIR/hero-linux.png"
else
  echo "error: neither ImageMagick 'import' nor 'xwd'+'convert' is available" >&2
  exit 1
fi

if [[ "$CLICK_OK" -eq 0 ]]; then
  echo "warn: tray click via xdotool did not run cleanly; hero-linux.png shows the tray icon only, not an open menu." >&2
fi

echo "Wrote $OUT_DIR/hero-linux.png"
