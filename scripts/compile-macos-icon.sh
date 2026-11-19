#!/usr/bin/env bash
# Compile the Icon Composer source into the assets used by the app bundle.
#
# The .icon format is compiled by Apple's actool, which ships with Xcode. A
# clean-room implementation is available at https://github.com/viraptor/actool
# (build it with cargo and set ACTOOL=/path/to/actool); it produces the same
# Assets.car + .icns pair for the macOS target.
#
#   ./scripts/compile-macos-icon.sh
#   ACTOOL=/tmp/actool/target/release/actool ./scripts/compile-macos-icon.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ACTOOL="${ACTOOL:-}"
if [ -z "$ACTOOL" ]; then
    ACTOOL="$(xcrun --find actool 2>/dev/null || true)"
fi
if [ -z "$ACTOOL" ] || [ ! -x "$ACTOOL" ]; then
    echo "compile-macos-icon: actool not found; set ACTOOL=/path/to/actool" >&2
    exit 1
fi

OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

"$ACTOOL" \
    --compile "$OUT" \
    --platform macosx \
    --minimum-deployment-target 14.2 \
    --app-icon AppIcon \
    --output-partial-info-plist "$OUT/AppIcon.plist" \
    "$ROOT/assets/AppIcon.icon"

cp "$OUT/Assets.car" "$ROOT/assets/Assets.car"
cp "$OUT/AppIcon.icns" "$ROOT/assets/lgx.icns"
echo "wrote assets/Assets.car and assets/lgx.icns"
