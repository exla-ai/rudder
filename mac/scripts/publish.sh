#!/usr/bin/env bash
# Publishes a Mac release: writes appcast.xml with scripts/appcast.mjs, then
# creates GitHub release mac-v<version>-<build> on exla-ai/rudder with
# Rudder.dmg and appcast.xml, marked latest. Both are read through
# releases/latest/download/: https://rudder.exla.ai/download/mac redirects to
# the DMG, and installed apps check the appcast (SUFeedURL) for updates.
#
#   RUDDER_SPARKLE_KEY=<private key file> mac/scripts/publish.sh <dir> <commit>
#
# <dir> holds Rudder.dmg and release.env as scripts/package.sh or ci.sh's dmg
# stage wrote them. <commit> is the pushed commit they were built from; the
# release's tag points at it. RUDDER_SPARKLE_KEY is the Sparkle EdDSA private
# key (mac/README.md, "Updates"). Runs anywhere with Node 18+ and gh.
set -euo pipefail

die() { printf 'publish.sh: %s\n' "$*" >&2; exit 1; }

[[ $# -eq 2 ]] || die "usage: RUDDER_SPARKLE_KEY=<key file> publish.sh <dir> <commit>"
DIR="$1"
COMMIT="$2"
[[ -n "${RUDDER_SPARKLE_KEY:-}" ]] || die "RUDDER_SPARKLE_KEY is not set: the path of the Sparkle private key file"
[[ -f "$DIR/Rudder.dmg" ]] || die "$DIR/Rudder.dmg is missing"
[[ -f "$DIR/release.env" ]] || die "$DIR/release.env is missing"
command -v node >/dev/null || die "node is missing"
command -v gh >/dev/null || die "gh is missing"

VERSION="" BUILD="" MINIMUM_SYSTEM=""
. "$DIR/release.env"
[[ -n "$VERSION" && -n "$BUILD" && -n "$MINIMUM_SYSTEM" ]] || die "$DIR/release.env must set VERSION, BUILD and MINIMUM_SYSTEM"

# appcast.mjs names the enclosure's release the same way.
TAG="mac-v$VERSION-$BUILD"
node "$(dirname "${BASH_SOURCE[0]}")/appcast.mjs" --dmg "$DIR/Rudder.dmg" --version "$VERSION" --build "$BUILD" \
  --minimum-system "$MINIMUM_SYSTEM" --key "$RUDDER_SPARKLE_KEY" --out "$DIR/appcast.xml"

# One DMG per version and build: a release that exists already fails here
# instead of swapping its download. Bump CURRENT_PROJECT_VERSION. gh uploads
# both assets before the release goes public, so "latest" never lacks the appcast.
gh release create "$TAG" "$DIR/Rudder.dmg" "$DIR/appcast.xml" --repo exla-ai/rudder --latest --target "$COMMIT" \
  --title "Rudder for Mac $VERSION ($BUILD)" \
  --notes "Rudder for macOS $VERSION, signed with Developer ID and notarized. Download: https://rudder.exla.ai/download/mac"
