#!/usr/bin/env bash
# Sign an unsigned Rudder.app and its Sparkle components for Developer ID distribution.
set -euo pipefail

MAC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ $# -eq 3 ]] || { echo 'usage: sign-app.sh <Rudder.app> <identity> <team>' >&2; exit 1; }
APP="$1"
IDENTITY="$2"
TEAM="$3"
SPARKLE="$APP/Contents/Frameworks/Sparkle.framework"
[[ -d "$SPARKLE" ]] || { echo "sign-app.sh: $APP has no Sparkle.framework" >&2; exit 1; }

sign() {
  codesign --force --sign "$IDENTITY" --options runtime --timestamp "$@"
}

# Sign inside out. Downloader.xpc keeps Sparkle's entitlements.
INSTALLER="$SPARKLE/Versions/B/XPCServices/Installer.xpc"
DOWNLOADER="$SPARKLE/Versions/B/XPCServices/Downloader.xpc"
AUTOUPDATE="$SPARKLE/Versions/B/Autoupdate"
UPDATER="$SPARKLE/Versions/B/Updater.app"
sign "$INSTALLER"
sign --preserve-metadata=entitlements "$DOWNLOADER"
sign "$AUTOUPDATE"
sign "$UPDATER"
sign "$SPARKLE"
sign --entitlements "$MAC_DIR/App/Rudder.entitlements" "$APP"

codesign --verify --deep --strict --verbose=2 "$APP"
for code in "$INSTALLER" "$DOWNLOADER" "$AUTOUPDATE" "$UPDATER" "$SPARKLE" "$APP"; do
  signature="$(codesign -dv --verbose=2 "$code" 2>&1)"
  grep -Fxq "TeamIdentifier=$TEAM" <<<"$signature" || { echo "sign-app.sh: $code is not signed by team $TEAM" >&2; exit 1; }
  grep -q 'flags=.*runtime' <<<"$signature" || { echo "sign-app.sh: $code is missing the hardened runtime" >&2; exit 1; }
  grep -q '^Authority=Developer ID Application:' <<<"$signature" || { echo "sign-app.sh: $code is missing a Developer ID signature" >&2; exit 1; }
done
