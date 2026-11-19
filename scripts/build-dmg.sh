#!/usr/bin/env bash
# build-dmg.sh — build "LGX Server.app" and wrap it in a drag-to-Applications DMG.
#
#   ./scripts/build-dmg.sh [OUT.dmg]          # default packaging/LGX-Server-0.2.dmg
#   LGX_SIGN_ID="LGX Dev Signing 2" ./scripts/build-dmg.sh
#
# Run it from a Terminal in the logged-in GUI session (codesign needs the login
# keychain unlocked), or from a one-shot LaunchAgent in that session. The app
# inside the image is signed with the same stable identity as
# a direct install, so TCC grants survive dragging the new copy over the old one.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/packaging/LGX-Server-0.2.dmg}"
IDENT="${LGX_SIGN_ID:-LGX Dev Signing 2}"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

APP="$STAGE/LGX Server.app"
echo "==> app"
LGX_SIGN_ID="$IDENT" "$ROOT/scripts/build-macos-app.sh" "$APP"

# The classic install affordance: the window shows the app next to the
# Applications folder, so the user just drags it in.
ln -s /Applications "$STAGE/Applications"

mkdir -p "$(dirname "$OUT")"
rm -f "$OUT"
echo "==> dmg $OUT"
hdiutil create -quiet -volname "LGX Server" -srcfolder "$STAGE" -ov -format UDZO "$OUT"
hdiutil verify "$OUT" >/dev/null

# Signing the image itself is optional, but it keeps Gatekeeper quiet about the
# container after the app copy has already been verified.
if codesign --force --sign "$IDENT" --timestamp=none "$OUT" >/dev/null 2>&1; then
    echo "==> signed: $OUT"
else
    echo "==> warning: could not sign the image (the app inside is signed)"
fi

echo "==> ok: $OUT"
