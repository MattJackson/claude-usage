#!/usr/bin/env bash
# Assembles a .deb package for usagio from an already-built release binary.
#
# Linux-only: relies on `dpkg-deb`, which ships on every `ubuntu-latest`
# GitHub Actions runner (part of the base `dpkg` package) -- no extra
# install step needed, unlike the macOS .icns / Linux .png generation which
# need librsvg.
#
# Usage:
#   packaging/linux/build-deb.sh <path-to-usagio-binary> <version> <out-dir>
#
# e.g. packaging/linux/build-deb.sh target/x86_64-unknown-linux-gnu/release/usagio 0.5.0 dist
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_PATH="${1:?usage: build-deb.sh <binary> <version> <out-dir>}"
VERSION="${2:?usage: build-deb.sh <binary> <version> <out-dir>}"
OUT_DIR="${3:?usage: build-deb.sh <binary> <version> <out-dir>}"

if [ ! -f "$BIN_PATH" ]; then
  echo "error: binary not found: $BIN_PATH" >&2
  exit 1
fi

if [ ! -d "$SCRIPT_DIR/icons/hicolor" ]; then
  echo "error: $SCRIPT_DIR/icons/hicolor not found -- run generate-png-icons.sh first" >&2
  exit 1
fi

PKG_ROOT="$(mktemp -d)/usagio_${VERSION}_amd64"
trap 'rm -rf "$(dirname "$PKG_ROOT")"' EXIT

mkdir -p "$PKG_ROOT/DEBIAN" \
  "$PKG_ROOT/usr/bin" \
  "$PKG_ROOT/usr/share/applications" \
  "$PKG_ROOT/usr/share/doc/usagio"

sed "s/{{VERSION}}/${VERSION}/g" "$SCRIPT_DIR/control.template" > "$PKG_ROOT/DEBIAN/control"

install -m 0755 "$BIN_PATH" "$PKG_ROOT/usr/bin/usagio"
install -m 0644 "$SCRIPT_DIR/usagio.desktop" "$PKG_ROOT/usr/share/applications/usagio.desktop"

# hicolor icon theme: one usagio.png per size directory produced by
# generate-png-icons.sh.
for size_dir in "$SCRIPT_DIR"/icons/hicolor/*/apps; do
  size="$(basename "$(dirname "$size_dir")")"
  dest="$PKG_ROOT/usr/share/icons/hicolor/${size}/apps"
  mkdir -p "$dest"
  install -m 0644 "$size_dir/usagio.png" "$dest/usagio.png"
done

# dpkg-deb wants directories, not files, to not be group/other-writable.
find "$PKG_ROOT" -type d -exec chmod 0755 {} +

mkdir -p "$OUT_DIR"
dpkg-deb --build --root-owner-group "$PKG_ROOT" "$OUT_DIR/usagio_${VERSION}_amd64.deb"
