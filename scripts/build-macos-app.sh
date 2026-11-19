#!/usr/bin/env bash
# build-macos-app.sh — assemble the self-contained "LGX Server.app".
#
# The bundle contains the lgx-protocol server (Rust) and its helpers:
#   lgx-vtenc        ScreenCaptureKit -> VideoToolbox HEVC
#   lgx-cginput      CGEvent input injection
#   lgx-macos-audio  Core Audio process tap (system audio, muted locally)
#
# Drag the app into /Applications and grant Screen Recording, Accessibility
# and System Audio Recording.
#
#   ./scripts/build-macos-app.sh              # into /Applications
#   ./scripts/build-macos-app.sh /tmp/x.app   # somewhere else
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="${1:-/Applications/LGX Server.app}"
IDENT="dev.b612s.lgx.server"

echo "==> rust server"
cargo build --release -p lgx-server-macos --manifest-path "$ROOT/Cargo.toml"

echo "==> swift helpers"
mkdir -p "$ROOT/target/macos-helper"
swiftc -O -o "$ROOT/target/macos-helper/lgx-vtenc" "$ROOT/helper/lgx-vtenc.swift" \
  -framework ScreenCaptureKit -framework VideoToolbox -framework CoreMedia \
  -framework CoreVideo -framework Foundation
# Input injection is C with small AppKit shims for the frontmost PID and the
# NX_SYSDEFINED media events; the Swift variant trapped (SIGTRAP) before main.
clang -O2 -o "$ROOT/target/macos-helper/lgx-cginput" \
  "$ROOT/helper/lgx-cginput.c" "$ROOT/helper/lgx-frontmost.m" \
  "$ROOT/helper/lgx-media.m" \
  -framework ApplicationServices -framework CoreGraphics -framework AppKit
swiftc -O -o "$ROOT/target/macos-helper/lgx-launcher" "$ROOT/helper/lgx-launcher.swift" \
  -framework AppKit -framework ApplicationServices -framework ScreenCaptureKit
# Audio capture is Swift: CATapDescription is an Objective-C class, so the C
# runtime would be a lot of boilerplate for no gain.
swiftc -O -o "$ROOT/target/macos-helper/lgx-macos-audio" "$ROOT/helper/lgx-macos-audio.swift" \
  -framework CoreAudio -framework AudioToolbox -framework Foundation
# Clipboard bridge: NSPasteboard has no bindings worth writing in Rust.
swiftc -O -o "$ROOT/target/macos-helper/lgx-pb" "$ROOT/helper/lgx-pb.swift" \
  -framework AppKit -framework Foundation

echo "==> bundle $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# The launcher is the bundle's main executable, so TCC attaches permissions to
# the app itself; the Rust server runs as its child under the name *-bin.
cp "$ROOT/target/release/lgx-server-macos" "$APP/Contents/MacOS/lgx-server-bin"
cp "$ROOT/target/macos-helper/lgx-launcher" "$APP/Contents/MacOS/lgx-launcher"
cp "$ROOT/target/macos-helper/lgx-vtenc"     "$APP/Contents/MacOS/"
cp "$ROOT/target/macos-helper/lgx-cginput"   "$APP/Contents/MacOS/"
cp "$ROOT/target/macos-helper/lgx-macos-audio" "$APP/Contents/MacOS/"
cp "$ROOT/target/macos-helper/lgx-pb"        "$APP/Contents/MacOS/"
[ -f "$ROOT/assets/lgx.icns" ] && cp "$ROOT/assets/lgx.icns" "$APP/Contents/Resources/lgx.icns"
chmod 755 "$APP/Contents/MacOS/"*

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>LGX Server</string>
    <key>CFBundleDisplayName</key><string>LGX Server</string>
    <key>CFBundleIdentifier</key><string>$IDENT</string>
    <key>CFBundleExecutable</key><string>lgx-launcher</string>
    <key>CFBundleIconFile</key><string>lgx.icns</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.2</string>
    <key>CFBundleVersion</key><string>2</string>
    <key>LSMinimumSystemVersion</key><string>14.2</string>
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSAudioCaptureUsageDescription</key>
    <string>LGX Server captures the system audio mix to stream it to the connected client. Local playback is muted while streaming.</string>
</dict>
</plist>
PLIST

# Sign with the stable identity when one is given: TCC binds permissions to
# the code requirement, and an ad-hoc signature changes on every build, which
# resets Screen Recording / System Audio / Accessibility. Every executable in
# the bundle is signed as well, because launchd may run them directly.
SIGN_ID="${LGX_SIGN_ID:-}"
if [ -n "$SIGN_ID" ]; then
    for bin in "$APP/Contents/MacOS/"*; do
        codesign --force --sign "$SIGN_ID" --timestamp=none "$bin"
    done
    codesign --force --sign "$SIGN_ID" --timestamp=none "$APP"
else
    codesign --force --sign - --timestamp=none "$APP"
fi
echo "==> ok: $APP"
codesign -dvv "$APP" 2>&1 | grep -E "Identifier|Authority|Signature" | head -3
