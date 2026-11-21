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

# Sparkle owns the update dialog. Set Rudder's English copy before sealing its
# resources with the framework signature.
python3 - "$SPARKLE/Resources/Base.lproj/Sparkle.strings" <<'PY'
import pathlib
import plistlib
import subprocess
import sys

path = pathlib.Path(sys.argv[1])
strings = plistlib.loads(subprocess.check_output([
    "/usr/bin/plutil", "-convert", "binary1", "-o", "-", "--", str(path)
]))
copy = {
    "A new version of %@ is available!": "%@ update available",
    "%@ %@ is now available—you have %@. Would you like to download it now?":
        "Update to %1$@ %2$@ from %3$@.",
    "A new version of %@ is ready to install!": "%@ update ready to install",
    "%1$@ %2$@ has been downloaded and is ready to use! Would you like to install it and relaunch %1$@ now?":
        "%1$@ %2$@ is ready. Install it now to restart %1$@ with the update.",
    "Automatically download and install updates in the future":
        "Install future updates automatically",
}
missing = copy.keys() - strings.keys()
if missing:
    sys.exit(f"Sparkle update strings have changed: {sorted(missing)}")
strings.update(copy)
path.write_bytes(plistlib.dumps(strings, fmt=plistlib.FMT_BINARY))
PY

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
