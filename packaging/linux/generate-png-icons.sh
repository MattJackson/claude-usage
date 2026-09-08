#!/usr/bin/env bash
# Generates packaging/linux/icons/hicolor/<size>x<size>/apps/usagio.png at
# every size the freedesktop hicolor icon theme spec expects, from the same
# vector source (web/public/logo.svg) that packaging/macos/generate-icns.sh
# rasterizes for the macOS .icns.
#
# Linux-only: relies on `rsvg-convert` (from librsvg2-bin), installed
# alongside the rest of the apt system deps in .github/workflows/release.yml
# / rc-release.yml right before this script runs.
#
# The output PNGs are git-ignored (see .gitignore) -- build artifacts, not
# source; regenerate any time the source SVG changes.
#
# Usage:
#   packaging/linux/generate-png-icons.sh [source.svg]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
OUT_DIR="$SCRIPT_DIR/icons/hicolor"
SOURCE_SVG="${1:-$REPO_ROOT/web/public/logo.svg}"

if [ ! -f "$SOURCE_SVG" ]; then
  echo "error: source SVG not found: $SOURCE_SVG" >&2
  exit 1
fi

if ! command -v rsvg-convert >/dev/null 2>&1; then
  echo "error: rsvg-convert not found (apt install librsvg2-bin)" >&2
  exit 1
fi

# Standard hicolor icon theme sizes for application launcher icons.
SIZES=(16 22 24 32 48 64 128 256 512)

rm -rf "$OUT_DIR"
for size in "${SIZES[@]}"; do
  dir="$OUT_DIR/${size}x${size}/apps"
  mkdir -p "$dir"
  rsvg-convert -w "$size" -h "$size" "$SOURCE_SVG" -o "$dir/usagio.png"
done

echo "wrote ${#SIZES[@]} sizes under $OUT_DIR"
