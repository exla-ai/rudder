#!/usr/bin/env bash
# Builds Rudder.app for Developer ID distribution: XcodeGen, tests, an
# unsigned Release archive, then scripts/package.sh signs it (Sparkle's
# helpers first), notarizes and staples it, and writes a signed, notarized,
# stapled Rudder.dmg and release.env to mac/build. scripts/publish.sh releases
# them (with the Sparkle appcast) from wherever the Sparkle key is.
#
#   mac/scripts/build.sh                 # everything (needs notary credentials)
#   RUDDER_SKIP_NOTARIZE=1 mac/scripts/build.sh   # signed but not notarized
#
# Environment:
#   RUDDER_SKIP_TESTS=1   skip the RudderCoreTests run
#   Signing identity and notary credentials: see scripts/package.sh.
set -euo pipefail

MAC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$MAC_DIR"

OUT="$MAC_DIR/build"
ARCHIVE="$OUT/Rudder.xcarchive"
APP="$OUT/Rudder.app"

say() { printf '\n== %s\n' "$*"; }
die() { printf 'build.sh: %s\n' "$*" >&2; exit 1; }

[[ "$(uname)" == "Darwin" ]] || die "run this on a Mac"
command -v xcodegen >/dev/null || die "xcodegen is missing: brew install xcodegen"
command -v xcodebuild >/dev/null || die "xcodebuild is missing: install Xcode 16 or newer"

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

say "Archiving (unsigned: package.sh signs every piece itself)"
rm -rf "$OUT"
mkdir -p "$OUT"
xcodebuild archive \
  -project Rudder.xcodeproj -scheme Rudder -configuration Release \
  -destination 'generic/platform=macOS' -archivePath "$ARCHIVE" \
  CODE_SIGNING_ALLOWED=NO

ditto "$ARCHIVE/Products/Applications/Rudder.app" "$APP"
"$MAC_DIR/scripts/package.sh" "$APP"
