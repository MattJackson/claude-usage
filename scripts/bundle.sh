#!/usr/bin/env bash
# Build "Claude Usage.app" — a menu-bar (LSUIElement) wrapper around the
# release binary. The app launches `claude-usage menubar`; the same binary is
# still a full CLI when invoked directly.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
APP="target/Claude Usage.app"
MACOS="$APP/Contents/MacOS"

cargo build --release

rm -rf "$APP"
mkdir -p "$MACOS" "$APP/Contents/Resources"
cp "target/release/claude-usage" "$MACOS/claude-usage"

# Launcher: the bundle's executable, execs the binary in menu-bar mode.
cat > "$MACOS/launcher" <<'SH'
#!/bin/bash
DIR="$(cd "$(dirname "$0")" && pwd)"
exec "$DIR/claude-usage" menubar
SH
chmod +x "$MACOS/launcher" "$MACOS/claude-usage"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Claude Usage</string>
  <key>CFBundleDisplayName</key><string>Claude Usage</string>
  <key>CFBundleIdentifier</key><string>com.claude-usage</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>launcher</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict>
</plist>
PLIST

echo "Built $APP"
echo "Run it:  open \"$APP\""
