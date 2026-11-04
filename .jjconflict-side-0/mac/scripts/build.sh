#!/usr/bin/env bash
# Builds Rudder.app for Developer ID distribution: XcodeGen, tests, archive,
# notarize + staple the app, then a signed, notarized, stapled Rudder.dmg.
#
#   mac/scripts/build.sh                 # everything (needs notary credentials)
#   RUDDER_SKIP_NOTARIZE=1 mac/scripts/build.sh   # signed but not notarized
#   RUDDER_RELEASE_TAG=mac-v1.0.0 mac/scripts/build.sh   # also publish a GitHub release
#
# Environment:
#   RUDDER_TEAM_ID        Apple team (default: project.yml's DEVELOPMENT_TEAM, 3C4383262W)
#   RUDDER_SIGN_IDENTITY  default "Developer ID Application" (must be in the login keychain)
#   Notary credentials, one of:
#     RUDDER_NOTARY_PROFILE                 a `xcrun notarytool store-credentials` profile
#     RUDDER_NOTARY_KEY, RUDDER_NOTARY_KEY_ID, RUDDER_NOTARY_ISSUER   App Store Connect API key (.p8 path)
#     RUDDER_NOTARY_APPLE_ID, RUDDER_NOTARY_PASSWORD (app-specific)  plus RUDDER_TEAM_ID
#   RUDDER_SKIP_TESTS=1   skip the RudderCoreTests run
#   RUDDER_RELEASE_TAG    after building, `gh release create <tag> Rudder.dmg --latest`
#                         on exla-ai/rudder (what https://rudder.exla.ai/download/mac serves)
set -euo pipefail

MAC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$MAC_DIR"

TEAM="${RUDDER_TEAM_ID:-3C4383262W}"
IDENTITY="${RUDDER_SIGN_IDENTITY:-Developer ID Application}"
OUT="$MAC_DIR/build"
ARCHIVE="$OUT/Rudder.xcarchive"
APP="$OUT/Rudder.app"
DMG="$OUT/Rudder.dmg"

say() { printf '\n== %s\n' "$*"; }
die() { printf 'build.sh: %s\n' "$*" >&2; exit 1; }

[[ "$(uname)" == "Darwin" ]] || die "run this on a Mac"
command -v xcodegen >/dev/null || die "xcodegen is missing: brew install xcodegen"
command -v xcodebuild >/dev/null || die "xcodebuild is missing: install Xcode 16 or newer"

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

say "Generating Rudder.xcodeproj"
xcodegen generate --spec project.yml

if [[ -z "${RUDDER_SKIP_TESTS:-}" ]]; then
  say "Testing RudderCore"
  TEST_LOG="$(mktemp -t rudder-mac-tests)"
  if ! xcodebuild test -project Rudder.xcodeproj -scheme RudderCoreTests -destination 'platform=macOS' \
    CODE_SIGNING_ALLOWED=NO >"$TEST_LOG" 2>&1; then
    tail -n 60 "$TEST_LOG"
    die "RudderCoreTests failed (full log: $TEST_LOG)"
  fi
  grep -E "Executed [0-9]+ tests" "$TEST_LOG" | tail -n 1 || true
fi

say "Archiving (signed with $IDENTITY, team $TEAM)"
rm -rf "$OUT"
mkdir -p "$OUT"
xcodebuild archive \
  -project Rudder.xcodeproj -scheme Rudder -configuration Release \
  -destination 'generic/platform=macOS' -archivePath "$ARCHIVE" \
  CODE_SIGN_STYLE=Manual CODE_SIGN_IDENTITY="$IDENTITY" DEVELOPMENT_TEAM="$TEAM" \
  ENABLE_HARDENED_RUNTIME=YES OTHER_CODE_SIGN_FLAGS="--timestamp" \
  ARCHS="arm64 x86_64" ONLY_ACTIVE_ARCH=NO

ditto "$ARCHIVE/Products/Applications/Rudder.app" "$APP"

say "Checking the signature"
codesign --verify --deep --strict --verbose=2 "$APP"
# Read it into a variable first: under pipefail, `grep -q` closing the pipe
# early kills codesign with SIGPIPE and the check fails on a good signature.
SIGNATURE="$(codesign -dv "$APP" 2>&1)"
grep -q "flags=.*runtime" <<<"$SIGNATURE" || die "hardened runtime is not on"

say "Notarizing the app"
ZIP="$OUT/Rudder-notarize.zip"
ditto -c -k --keepParent "$APP" "$ZIP"
notarize "$ZIP"
rm -f "$ZIP"
# The ticket was issued for the zip's contents; it staples to the app itself.
staple "$APP"

say "Building Rudder.dmg"
STAGE="$OUT/dmg"
mkdir -p "$STAGE"
ditto "$APP" "$STAGE/Rudder.app"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Rudder" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"
codesign --sign "$IDENTITY" --timestamp "$DMG"
notarize "$DMG"
staple "$DMG"

if [[ -z "${RUDDER_SKIP_NOTARIZE:-}" ]]; then
  say "Gatekeeper"
  spctl --assess --type execute --verbose=2 "$APP"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"
fi

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")"
echo
echo "Built Rudder $VERSION: $DMG"
shasum -a 256 "$DMG"

if [[ -n "${RUDDER_RELEASE_TAG:-}" ]]; then
  command -v gh >/dev/null || die "gh is missing: brew install gh"
  say "Publishing $RUDDER_RELEASE_TAG on exla-ai/rudder"
  gh release create "$RUDDER_RELEASE_TAG" "$DMG" --repo exla-ai/rudder --latest \
    --title "Rudder for Mac $VERSION" --notes "Rudder for macOS $VERSION. Download: https://rudder.exla.ai/download/mac"
fi
