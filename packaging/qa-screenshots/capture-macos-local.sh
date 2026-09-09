#!/usr/bin/env bash
# LOCAL macOS capture — produces REAL screenshots of the REAL usagio menu
# bar dropdown on the maintainer's own Mac. There is no cheap on-demand
# macOS cloud option and GitHub-hosted runners have no real GUI session
# (menu-bar dropdowns render black), so macOS capture is done here.
#
# How it actually works (this is the tricky part with NSStatusItem menus):
#   * usagio's dropdown only exists while the menu is open (a modal
#     tracking run-loop). A normal foreground `screencapture` can't fire
#     during that loop, so we SCHEDULE a background `screencapture` with a
#     short `sleep` delay BEFORE opening the menu — it fires while the menu
#     is up.
#   * The menu is opened with the Accessibility API via System Events:
#       perform action "AXPress" of menu bar item 1 of menu bar 1
#     of process "usagio". `set frontmost` / `click` are unreliable for an
#     LSUIElement accessory app; AXPress is the one that keeps the menu open.
#   * Multi-variant data comes from the fixtures in
#     packaging/screenshots/fixtures/. usagio mtime-checks state.json on
#     every ~0.75s menu tick, so we swap the fixture under a SINGLE running
#     instance rather than relaunching.
#
# Isolation / safety: we launch a THROWAWAY usagio pointed at an isolated
# $HOME (usagio resolves config as $HOME/.config/usagio — see
# src/platform/macos.rs::config_dir), so the user's real
# ~/.config/usagio/state.json and their 5 real accounts are NEVER touched.
#
# Privacy: the user's Calendar/Photos desktop widgets sit top-LEFT; usagio's
# menu is top-RIGHT. We hide all other app windows to get a clean wallpaper
# backdrop, then crop right-anchored (postprocess.py) so the top-left
# widgets never make it into frame.
#
# Prereq: usagio installed, Accessibility permission granted to whatever's
# driving osascript (Terminal/iTerm), python3 + Pillow on PATH.
#
# Usage:  capture-macos-local.sh [version]
# Writes: packaging/qa-screenshots/output/<version>/macos-<variant>.png
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

VERSION="${1:-${USAGIO_QA_VERSION:-local}}"
OUT_DIR="${USAGIO_QA_OUT_DIR:-$SCRIPT_DIR/output/$VERSION}"
mkdir -p "$OUT_DIR"

FIXTURES_DIR="$REPO_ROOT/packaging/screenshots/fixtures"
RENDER="$REPO_ROOT/packaging/screenshots/render_fixture.py"
POST="$REPO_ROOT/packaging/screenshots/postprocess.py"

# Isolated scratch HOME so we never read/write the real config.
SCRATCH_HOME="$(mktemp -d "${TMPDIR:-/tmp}/usagio-shots-home.XXXXXX")"
SCRATCH_STATE="$SCRATCH_HOME/.config/usagio/state.json"
mkdir -p "$SCRATCH_HOME/.config/usagio"

RAW_DIR="$(mktemp -d "${TMPDIR:-/tmp}/usagio-shots-raw.XXXXXX")"
USAGIO_PID=""
HIDDEN_APPS=""

# name:fixture   (all use the plain menu-open mode)
VARIANTS=(
  "healthy:healthy"
  "session-locked:session-locked"
  "weekly-locked:weekly-locked"
  "mixed:mixed"
)

# Crop: right-anchored 16:9 that frames the menu bar + open dropdown while
# excluding the top-left desktop widgets.
CROP_ANCHOR_X=0.70
CROP_ANCHOR_Y=0.24
CROP_WIDTH_FRAC=0.55
CROP_BG="40,40,45"

log() { echo "[macos-local] $*"; }

relaunch_real_usagio() {
  # Bring the user's real menu-bar app back via its launch agent.
  local plist="$HOME/Library/LaunchAgents/com.mattjackson.usagio.menubar.plist"
  local uid; uid="$(id -u)"
  if pgrep -f 'usagio.app/Contents/MacOS/usagio menubar' >/dev/null 2>&1; then
    return 0
  fi
  if [[ -f "$plist" ]]; then
    launchctl bootstrap "gui/$uid" "$plist" 2>/dev/null \
      || launchctl kickstart -k "gui/$uid/com.mattjackson.usagio.menubar" 2>/dev/null \
      || true
  fi
}

cleanup() {
  # Kill the throwaway instance.
  if [[ -n "$USAGIO_PID" ]] && kill -0 "$USAGIO_PID" 2>/dev/null; then
    kill "$USAGIO_PID" 2>/dev/null || true
    wait "$USAGIO_PID" 2>/dev/null || true
  fi
  # Un-hide the apps we hid.
  if [[ -n "$HIDDEN_APPS" ]]; then
    while IFS= read -r app; do
      [[ -z "$app" ]] && continue
      osascript -e "tell application \"System Events\" to set visible of (first process whose name is \"$app\") to true" >/dev/null 2>&1 || true
    done <<< "$HIDDEN_APPS"
  fi
  rm -rf "$SCRATCH_HOME" "$RAW_DIR"
  relaunch_real_usagio
}
trap cleanup EXIT

# Locate the usagio binary.
USAGIO_BIN=""
for cand in \
  "/opt/homebrew/opt/usagio/usagio.app/Contents/MacOS/usagio" \
  "/Applications/usagio.app/Contents/MacOS/usagio" \
  "$HOME/Applications/usagio.app/Contents/MacOS/usagio" \
  "$(command -v usagio || true)" \
  "$REPO_ROOT/target/release/usagio" \
  "$REPO_ROOT/target/debug/usagio"; do
  if [[ -n "$cand" && -x "$cand" ]]; then USAGIO_BIN="$cand"; break; fi
done
if [[ -z "$USAGIO_BIN" ]]; then
  echo "error: no usagio binary found" >&2
  exit 1
fi
log "using $USAGIO_BIN"
log "scratch HOME=$SCRATCH_HOME (real ~/.config/usagio untouched)"

# Stop any running usagio so "process usagio" is unambiguous for System
# Events (both the user's real instance and any stray ones). The real one
# is relaunched in cleanup().
pkill -f 'usagio.app/Contents/MacOS/usagio menubar' 2>/dev/null || true
pkill -f 'target/(release|debug)/usagio menubar' 2>/dev/null || true
sleep 1

# Seed the first fixture, launch the throwaway instance.
python3 "$RENDER" "$FIXTURES_DIR/healthy.json" "$SCRATCH_STATE"
HOME="$SCRATCH_HOME" "$USAGIO_BIN" menubar >/dev/null 2>&1 &
USAGIO_PID=$!
sleep 4
if ! kill -0 "$USAGIO_PID" 2>/dev/null; then
  echo "error: throwaway usagio exited immediately" >&2
  exit 1
fi

# Sanity: exactly one usagio process should be visible to System Events.
proc_count="$(osascript -e 'tell application "System Events" to count (processes whose name is "usagio")' 2>/dev/null || echo 0)"
log "usagio processes visible to System Events: $proc_count"

# Record currently-visible apps, then hide them for a clean backdrop.
HIDDEN_APPS="$(osascript -e 'tell application "System Events" to get name of (every process whose visible is true and background only is false and name is not "Finder")' 2>/dev/null | tr ',' '\n' | sed 's/^ *//;s/ *$//')"
osascript -e 'tell application "Finder" to activate' >/dev/null 2>&1 || true
osascript -e 'tell application "System Events" to set visible of (every process whose visible is true and background only is false and name is not "Finder") to false' >/dev/null 2>&1 || true
sleep 1

captured=0
for entry in "${VARIANTS[@]}"; do
  IFS=':' read -r name fixture <<< "$entry"

  python3 "$RENDER" "$FIXTURES_DIR/$fixture.json" "$SCRATCH_STATE"
  sleep 1.8  # let usagio's cached_state() pick up the new mtime

  osascript -e 'tell application "Finder" to activate' >/dev/null 2>&1 || true
  sleep 0.4

  raw="$RAW_DIR/macos-$name-raw.png"
  # Schedule the capture to fire while the menu is open, THEN open the menu.
  ( sleep 0.7 && screencapture -x -T 0 "$raw" ) &
  cap_pid=$!
  osascript -e 'tell application "System Events" to tell process "usagio" to perform action "AXPress" of menu bar item 1 of menu bar 1' >/dev/null 2>&1 || \
    log "warn: AXPress failed for $name (Accessibility perms?)"
  wait "$cap_pid" 2>/dev/null || true
  # Dismiss the menu.
  osascript -e 'tell application "System Events" to key code 53' >/dev/null 2>&1 || true
  sleep 0.3

  if [[ ! -s "$raw" ]]; then
    log "warn: no raw capture for $name; skipping"
    continue
  fi
  python3 "$POST" "$raw" "$OUT_DIR/macos-$name.png" \
    --anchor-x "$CROP_ANCHOR_X" --anchor-y "$CROP_ANCHOR_Y" \
    --crop-width-frac "$CROP_WIDTH_FRAC" --bg "$CROP_BG"
  captured=$((captured + 1))
done

log "wrote $captured screenshots to $OUT_DIR"
