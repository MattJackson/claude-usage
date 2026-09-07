#!/usr/bin/env bash
# Generates packaging/macos/AppIcon.icns from a large square source PNG.
#
# Runs once per release, invoked from the "Bump Homebrew tap formula" /
# packaging step of .github/workflows/release.yml right after the universal
# (lipo'd) binary is built. macOS-only: relies on `sips` and `iconutil`,
# both stock Xcode command-line tools present on every `macos-latest`
# GitHub Actions runner — no extra dependency to install.
#
# The output AppIcon.icns is git-ignored (see .gitignore) — it's a build
# artifact, not source; regenerate it any time the source PNG changes.
#
# Usage:
#   packaging/macos/generate-icns.sh [source.png]
#
# If no source is given, defaults to rasterizing web/public/logo.svg (the
# usagio brand mark — a 512x512 vector, so it upscales losslessly to the
# 1024x1024 the largest .iconset slot needs) via rsvg-convert. Pass an
# explicit PNG path to use something else instead.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
OUT_ICNS="$SCRIPT_DIR/AppIcon.icns"
ICONSET_DIR="$(mktemp -d)/AppIcon.iconset"
SOURCE_PNG="${1:-}"

cleanup() { rm -rf "$(dirname "$ICONSET_DIR")"; }
trap cleanup EXIT

if [ -z "$SOURCE_PNG" ]; then
  SOURCE_SVG="$REPO_ROOT/web/public/logo.svg"
  if [ ! -f "$SOURCE_SVG" ]; then
    echo "error: no source PNG given and $SOURCE_SVG not found" >&2
    exit 1
  fi
  if ! command -v rsvg-convert >/dev/null 2>&1; then
    echo "error: rsvg-convert not found (brew install librsvg) and no source PNG given" >&2
    exit 1
  fi
  SOURCE_PNG="$(mktemp -d)/logo-1024.png"
  rsvg-convert -w 1024 -h 1024 "$SOURCE_SVG" -o "$SOURCE_PNG"
fi

if [ ! -f "$SOURCE_PNG" ]; then
  echo "error: source PNG not found: $SOURCE_PNG" >&2
  exit 1
fi

width="$(sips -g pixelWidth "$SOURCE_PNG" | awk '/pixelWidth/{print $2}')"
height="$(sips -g pixelHeight "$SOURCE_PNG" | awk '/pixelHeight/{print $2}')"
if [ "$width" -lt 1024 ] || [ "$height" -lt 1024 ]; then
  echo "error: source PNG is ${width}x${height}, need at least 1024x1024" >&2
  exit 1
fi

mkdir -p "$ICONSET_DIR"

# iconutil requires exactly these ten filenames/sizes in a .iconset.
declare -a SIZES=(
  "16 icon_16x16.png"
  "32 icon_16x16@2x.png"
  "32 icon_32x32.png"
  "64 icon_32x32@2x.png"
  "128 icon_128x128.png"
  "256 icon_128x128@2x.png"
  "256 icon_256x256.png"
  "512 icon_256x256@2x.png"
  "512 icon_512x512.png"
  "1024 icon_512x512@2x.png"
)

for entry in "${SIZES[@]}"; do
  size="${entry%% *}"
  name="${entry#* }"
  sips -z "$size" "$size" "$SOURCE_PNG" --out "$ICONSET_DIR/$name" >/dev/null
done

iconutil -c icns "$ICONSET_DIR" -o "$OUT_ICNS"
echo "wrote $OUT_ICNS"
