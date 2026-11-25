#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

echo "1. Building release binary..."
cargo build --release --manifest-path apps/p2pkm-ui/src-tauri/Cargo.toml

BIN_PATH="apps/p2pkm-ui/src-tauri/target/release/p2pkm-ui"
APP_NAME="Remote P2P.app"
OUT_DIR="apps/p2pkm-ui/src-tauri/target/release/bundle/macos"
APP_DIR="$OUT_DIR/$APP_NAME"

echo "2. Packaging $APP_NAME..."
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"

cp "$BIN_PATH" "$APP_DIR/Contents/MacOS/p2pkm-ui"
chmod +x "$APP_DIR/Contents/MacOS/p2pkm-ui"

cat > "$APP_DIR/Contents/Info.plist" << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>p2pkm-ui</string>
  <key>CFBundleIdentifier</key>
  <string>com.remotep2p.mesh</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>Remote P2P</string>
  <key>CFBundleDisplayName</key>
  <string>Remote P2P</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleVersion</key>
  <string>1</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
EOF

echo "3. Ad-hoc signing $APP_NAME..."
codesign --force --deep --sign - "$APP_DIR" || true

echo ""
echo "Done! App bundle created at:"
echo "  $APP_DIR"
echo ""
echo "To install to /Applications, run:"
echo "  rm -rf '/Applications/Remote P2P.app' && cp -R '$APP_DIR' '/Applications/Remote P2P.app'"
