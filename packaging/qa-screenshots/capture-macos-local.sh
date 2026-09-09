#!/usr/bin/env bash
# LOCAL macOS capture — runs on the maintainer's own Mac, not in the
# cloud. There is no cheap on-demand macOS EC2 option (mac1/mac2 instances
# have a 24-hour minimum billing period, ~$25/run just for screenshots),
# and MacStadium/Anka/MacInCloud are monthly subscriptions. So macOS
# capture stays manual until that changes.
#
# Prereq: usagio v<VERSION> installed (Applications/usagio.app or
# equivalent), Accessibility permission granted to whatever's driving
# osascript (Terminal.app / iTerm2 / your shell), python3 on PATH.
#
# Usage:  capture-macos-local.sh [version]
#   version: optional. Used only to name the output folder. Defaults to
#            $USAGIO_QA_VERSION or "local".
#
# Writes: packaging/qa-screenshots/output/<version>/macos-<variant>.png
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

VERSION="${1:-${USAGIO_QA_VERSION:-local}}"
OUT_DIR="${USAGIO_QA_OUT_DIR:-$SCRIPT_DIR/output/$VERSION}"
mkdir -p "$OUT_DIR"

FIXTURES_DIR="$REPO_ROOT/packaging/screenshots/fixtures"
RENDER="$REPO_ROOT/packaging/screenshots/render_fixture.py"
STATE_DIR="$HOME/.config/usagio"
STATE_FILE="$STATE_DIR/state.json"
BACKUP=""
USAGIO_PID=""

FIXTURES=(healthy session-locked weekly-locked mixed)

cleanup() {
  if [[ -n "$USAGIO_PID" ]] && kill -0 "$USAGIO_PID" 2>/dev/null; then
    kill "$USAGIO_PID" 2>/dev/null || true
    wait "$USAGIO_PID" 2>/dev/null || true
  fi
  if [[ -n "$BACKUP" && -f "$BACKUP" ]]; then
    mv "$BACKUP" "$STATE_FILE"
  elif [[ -f "$STATE_FILE" ]]; then
    rm -f "$STATE_FILE"
  fi
}
trap cleanup EXIT

mkdir -p "$STATE_DIR"
if [[ -f "$STATE_FILE" ]]; then
  BACKUP="$(mktemp "${TMPDIR:-/tmp}/usagio-state-backup.XXXXXX")"
  cp "$STATE_FILE" "$BACKUP"
fi

# Find the installed usagio binary. Prefer the installed app bundle, fall
# back to a locally built binary.
USAGIO_BIN=""
for cand in \
  "/Applications/usagio.app/Contents/MacOS/usagio" \
  "$HOME/Applications/usagio.app/Contents/MacOS/usagio" \
  "$(command -v usagio || true)" \
  "$REPO_ROOT/target/release/usagio" \
  "$REPO_ROOT/target/debug/usagio"; do
  if [[ -n "$cand" && -x "$cand" ]]; then USAGIO_BIN="$cand"; break; fi
done
if [[ -z "$USAGIO_BIN" ]]; then
  echo "error: no installed usagio found (checked /Applications, PATH, target/)" >&2
  exit 1
fi
echo "[macos] using $USAGIO_BIN"

cp "$FIXTURES_DIR/healthy.json" "$STATE_FILE"
"$USAGIO_BIN" menubar &
USAGIO_PID=$!
sleep 3

for name in "${FIXTURES[@]}"; do
  python3 "$RENDER" "$FIXTURES_DIR/$name.json" "$STATE_FILE"
  sleep 1.5

  # Open usagio's menu bar dropdown. Needs Accessibility permission on
  # the process driving osascript; if denied you'll see a warning and the
  # capture will show just the menu bar strip.
  set +e
  osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    tell process "usagio"
        set frontmost to true
        click menu bar item 1 of menu bar 2
    end tell
end tell
APPLESCRIPT
  click_rc=$?
  set -e
  [[ $click_rc -ne 0 ]] && echo "[macos] warn: menu open failed for $name (Accessibility perms?)"
  sleep 0.8

  # Full desktop, no cursor, no shadow.
  screencapture -x "$OUT_DIR/macos-$name.png"

  osascript -e 'tell application "System Events" to key code 53' >/dev/null 2>&1 || true
done

echo "[macos] wrote ${#FIXTURES[@]} screenshots to $OUT_DIR"
