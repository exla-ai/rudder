#!/usr/bin/env bash
# Signs, notarizes and packages a Release build of Rudder.app: Developer ID
# signatures with the hardened runtime and a secure timestamp on Sparkle's
# helpers, Sparkle.framework and the app, notarized and stapled; then
# Rudder.dmg (laid out by make-dmg.sh), signed, notarized and stapled, and
# release.env (VERSION, BUILD, MINIMUM_SYSTEM) for scripts/publish.sh. Both
# land next to the app.
#
#   mac/scripts/package.sh <path/to/Rudder.app>
#
# build.sh runs it on the app it archives. An unsigned Release archive built
# on another Mac (an Offstage Mac when Xcode is unusable here) works the same:
# copy its Rudder.app over with `ditto -c -k --keepParent` and run this on it.
#
# Environment:
#   RUDDER_TEAM_ID        Apple team (default: project.yml's DEVELOPMENT_TEAM, 3C4383262W)
#   RUDDER_SIGN_IDENTITY  default "Developer ID Application" (must be in the login keychain)
#   Notary credentials, one of:
#     RUDDER_NOTARY_PROFILE                 a `xcrun notarytool store-credentials` profile
#     RUDDER_NOTARY_KEY, RUDDER_NOTARY_KEY_ID, RUDDER_NOTARY_ISSUER   App Store Connect API key (.p8 path)
#     RUDDER_NOTARY_APPLE_ID, RUDDER_NOTARY_PASSWORD (app-specific)  plus RUDDER_TEAM_ID
#   RUDDER_SKIP_NOTARIZE=1   signed but not notarized, for local testing
set -euo pipefail

MAC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEAM="${RUDDER_TEAM_ID:-3C4383262W}"
IDENTITY="${RUDDER_SIGN_IDENTITY:-Developer ID Application}"

say() { printf '\n== %s\n' "$*"; }
die() { printf 'package.sh: %s\n' "$*" >&2; exit 1; }

[[ "$(uname)" == "Darwin" ]] || die "run this on a Mac"
[[ $# -eq 1 ]] || die "usage: package.sh <path/to/Rudder.app>"
[[ -d "$1" ]] || die "$1 is not an app bundle"
APP="$(cd "$1" && pwd)"
OUT="$(dirname "$APP")"
DMG="$OUT/Rudder.dmg"
SPARKLE="$APP/Contents/Frameworks/Sparkle.framework"
[[ -d "$SPARKLE" ]] || die "$APP has no Sparkle.framework"

notarize() {
  local file="$1"
  if [[ -n "${RUDDER_SKIP_NOTARIZE:-}" ]]; then
    echo "skipping notarization of $(basename "$file") (RUDDER_SKIP_NOTARIZE)"
    return 0
  fi
  local args=()
  if [[ -n "${RUDDER_NOTARY_PROFILE:-}" ]]; then
    args=(--keychain-profile "$RUDDER_NOTARY_PROFILE")
  elif [[ -n "${RUDDER_NOTARY_KEY:-}" ]]; then
    args=(--key "$RUDDER_NOTARY_KEY" --key-id "${RUDDER_NOTARY_KEY_ID:?}" --issuer "${RUDDER_NOTARY_ISSUER:?}")
  elif [[ -n "${RUDDER_NOTARY_APPLE_ID:-}" ]]; then
    args=(--apple-id "$RUDDER_NOTARY_APPLE_ID" --password "${RUDDER_NOTARY_PASSWORD:?}" --team-id "$TEAM")
  else
    die "no notary credentials (set RUDDER_NOTARY_PROFILE, see the top of this script, or RUDDER_SKIP_NOTARIZE=1)"
  fi
  xcrun notarytool submit "$file" "${args[@]}" --wait --timeout 30m
}

staple() {
  [[ -n "${RUDDER_SKIP_NOTARIZE:-}" ]] || xcrun stapler staple "$1"
}

sign() {
  codesign --force --sign "$IDENTITY" --options runtime --timestamp "$@"
}

say "Signing with $IDENTITY (team $TEAM)"
# Inside out, as Sparkle documents for signing by hand, and never --deep. The
# XPC services serve sandboxed apps only, but they ship in the framework, so
# they are signed like the rest. Downloader.xpc keeps Sparkle's entitlements.
HELPERS=(
  "$SPARKLE/Versions/B/XPCServices/Installer.xpc"
  "$SPARKLE/Versions/B/XPCServices/Downloader.xpc"
  "$SPARKLE/Versions/B/Autoupdate"
  "$SPARKLE/Versions/B/Updater.app"
)
sign "${HELPERS[0]}"
sign --preserve-metadata=entitlements "${HELPERS[1]}"
sign "${HELPERS[2]}"
sign "${HELPERS[3]}"
sign "$SPARKLE"
sign --entitlements "$MAC_DIR/App/Rudder.entitlements" "$APP"

say "Checking the signatures"
codesign --verify --deep --strict --verbose=2 "$APP"
for code in "${HELPERS[@]}" "$SPARKLE" "$APP"; do
  # Read it into a variable first: under pipefail, `grep -q` closing the pipe
  # early kills codesign with SIGPIPE and the check fails on a good signature.
  SIGNATURE="$(codesign -dv --verbose=2 "$code" 2>&1)"
  grep -q "TeamIdentifier=$TEAM" <<<"$SIGNATURE" || die "$code is not signed by team $TEAM"
  grep -q "flags=.*runtime" <<<"$SIGNATURE" || die "$code does not have the hardened runtime"
done

say "Notarizing the app"
ZIP="$OUT/Rudder-notarize.zip"
ditto -c -k --keepParent "$APP" "$ZIP"
notarize "$ZIP"
rm -f "$ZIP"
# The ticket was issued for the zip's contents; it staples to the app itself.
staple "$APP"

say "Building Rudder.dmg"
"$MAC_DIR/scripts/make-dmg.sh" "$APP" "$DMG"
codesign --sign "$IDENTITY" --timestamp "$DMG"
notarize "$DMG"
staple "$DMG"

if [[ -z "${RUDDER_SKIP_NOTARIZE:-}" ]]; then
  say "Gatekeeper"
  spctl --assess --type execute --verbose=2 "$APP"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"
fi

PLIST="$APP/Contents/Info.plist"
VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$PLIST")"
BUILD="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$PLIST")"
MINIMUM_SYSTEM="$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$PLIST")"
printf 'VERSION=%s\nBUILD=%s\nMINIMUM_SYSTEM=%s\n' "$VERSION" "$BUILD" "$MINIMUM_SYSTEM" > "$OUT/release.env"
echo
echo "Built Rudder $VERSION ($BUILD): $DMG"
shasum -a 256 "$DMG"
