#!/usr/bin/env bash
# sign-and-install.sh — rebuild "LGX Server.app" and sign it with a stable
# identity, so TCC grants (Screen Recording, Accessibility, System Audio
# Recording) survive app updates.
#
# Without a stable certificate macOS signs ad-hoc and *every* rebuild resets
# the permissions: TCC binds them to the content hash.
#
#   ./scripts/sign-and-install.sh ["LGX Dev Signing 2"]
#
# The certificate is created once: Keychain Access -> Certificate Assistant ->
# Create a Certificate (Self Signed Root, Type: Code Signing).
#
# Run this from a Terminal in the logged-in GUI session: codesign needs the
# login keychain unlocked, which an SSH session does not provide.
set -euo pipefail

IDENT="${1:-${LGX_SIGN_ID:-LGX Dev Signing 2}}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="/Applications/LGX Server.app"

# The identity may be untrusted (self-signed); codesign can still use it and
# TCC keys on the certificate, so only "-v" (valid/trusted only) must not be
# used here.
if ! security find-identity -p codesigning | grep -q "$IDENT"; then
    echo "error: identity '$IDENT' is not in the keychain" >&2
    echo "create it once: Keychain Access -> Certificate Assistant -> Create a Certificate" >&2
    echo "  Name: $IDENT, Identity Type: Self Signed Root, Type: Code Signing" >&2
    exit 1
fi

LGX_SIGN_ID="$IDENT" "$ROOT/scripts/build-macos-app.sh"

# codesign rewrites the bundle, so it has to belong to the current user. The
# build above installs it that way; a root-owned bundle means a stale install
# from an older workflow.
if [ ! -w "$APP" ]; then
    echo "error: $APP is not writable; remove it (sudo rm -rf) and run this again" >&2
    exit 1
fi

codesign --verify --deep --strict "$APP"
echo "signed:"
codesign -dv "$APP" 2>&1 | grep -E "Identifier|Authority|Signature" | head -3

# The LaunchAgent must run the bundle's main executable: TCC then attaches the
# grants to the app, and the nested helpers inherit them.
PLIST="$HOME/Library/LaunchAgents/dev.b612s.lgx.server.plist"
if [ -f "$PLIST" ]; then
    launchctl bootout "gui/$(id -u)/dev.b612s.lgx.server" 2>/dev/null || true
    sleep 1
    /usr/libexec/PlistBuddy -c "Set :ProgramArguments:0 $APP/Contents/MacOS/lgx-launcher" "$PLIST" 2>/dev/null || true
    # launchd needs a moment after bootout; a failed bootstrap leaves the whole
    # server stopped, so retry instead of hiding the error.
    if ! launchctl bootstrap "gui/$(id -u)" "$PLIST" 2>/dev/null; then
        sleep 2
        launchctl bootstrap "gui/$(id -u)" "$PLIST"
    fi
    echo "lgx-server restarted via lgx-launcher"
fi
