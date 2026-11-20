#!/usr/bin/env bash
# Publishes a Mac release: uploads Rudder.dmg and release.env to a draft GitHub
# release mac-v<version>-<build> on exla-ai/rudder, then runs
# .github/workflows/mac-publish.yml on it and waits. That job signs Rudder.dmg
# into appcast.xml with the Sparkle EdDSA private key (the repo secret
# SPARKLE_ED_PRIVATE_KEY, the only copy of the key), verifies the feed against
# the SUPublicEDKey the app ships, uploads it and publishes the release as
# latest. Both files are read through releases/latest/download/:
# https://rudder.exla.ai/download/mac redirects to the DMG, and installed apps
# check the appcast (SUFeedURL) for updates.
#
#   mac/scripts/publish.sh <dir> <commit>
#
# <dir> holds Rudder.dmg and release.env as scripts/package.sh or ci.sh's dmg
# stage wrote them. <commit> is the pushed commit they were built from; the
# release's tag is created at it. Runs anywhere with gh signed in to an account
# that can push to the repo (it creates a release and dispatches a workflow);
# no key is needed here.
set -euo pipefail

REPO=exla-ai/rudder
WORKFLOW=mac-publish.yml

die() { printf 'publish.sh: %s\n' "$*" >&2; exit 1; }

[[ $# -eq 2 ]] || die "usage: publish.sh <dir> <commit>"
DIR="$1"
COMMIT="$2"
[[ -f "$DIR/Rudder.dmg" ]] || die "$DIR/Rudder.dmg is missing"
[[ -f "$DIR/release.env" ]] || die "$DIR/release.env is missing"
command -v gh >/dev/null || die "gh is missing"

VERSION="" BUILD="" MINIMUM_SYSTEM=""
. "$DIR/release.env"
[[ -n "$VERSION" && -n "$BUILD" && -n "$MINIMUM_SYSTEM" ]] || die "$DIR/release.env must set VERSION, BUILD and MINIMUM_SYSTEM"
# appcast.mjs names the enclosure's release the same way.
TAG="mac-v$VERSION-$BUILD"

# Publishing creates the tag at this commit, and the workflow reads the
# app's SUPublicEDKey from it, so it must be on GitHub.
SHA="$(gh api "repos/$REPO/commits/$COMMIT" --jq .sha)" || die "cannot find commit $COMMIT on GitHub: push it first"

# One DMG per version and build: a release or draft that exists already stops
# here instead of swapping its download. A new DMG needs a new Rudder version.
if existing="$(gh release view "$TAG" --repo "$REPO" --json isDraft --jq .isDraft 2>&1)"; then
  [[ "$existing" == true ]] || die "release $TAG is published already"
  die "a draft of $TAG exists already: to publish it as it is, run gh workflow run $WORKFLOW --repo $REPO --ref main -f tag=$TAG; to start over, gh release delete $TAG --repo $REPO --yes"
elif [[ "$existing" != "release not found" ]]; then
  die "gh release view $TAG: $existing"
fi

gh release create "$TAG" "$DIR/Rudder.dmg" "$DIR/release.env" --repo "$REPO" --draft --target "$SHA" \
  --title "Rudder for Mac $VERSION ($BUILD)" \
  --notes "Rudder for macOS $VERSION, signed with Developer ID and notarized. Download: https://rudder.exla.ai/download/mac"

# gh prints the new run's URL when it is not on a terminal.
RUN_URL="$(gh workflow run "$WORKFLOW" --repo "$REPO" --ref main -f tag="$TAG")"
[[ "$RUN_URL" =~ /actions/runs/([0-9]+) ]] || die "gh workflow run printed no run URL (${RUN_URL:-nothing}); find the run with gh run list --repo $REPO --workflow $WORKFLOW and watch it with gh run watch"
RUN_ID="${BASH_REMATCH[1]}"
echo "publish.sh: signing and publishing $TAG in $RUN_URL"
# The dispatch response can return before the run is visible to the Actions API.
for attempt in {1..10}; do
  if lookup=$(gh api "repos/$REPO/actions/runs/$RUN_ID?exclude_pull_requests=true" --silent 2>&1); then
    break
  fi
  if [[ "$lookup" != *"(HTTP 404)"* || "$attempt" -eq 10 ]]; then
    die "cannot read run $RUN_URL: $lookup"
  fi
  sleep 2
done
if ! gh run watch "$RUN_ID" --repo "$REPO" --exit-status --interval 10 >/dev/null; then
  echo "publish.sh: could not confirm that $WORKFLOW published $TAG. See $RUN_URL." >&2
  gh run view "$RUN_ID" --repo "$REPO" --log-failed | tail -40
  exit 1
fi
gh release view "$TAG" --repo "$REPO" --json url,assets --jq '.url, (.assets[] | "\(.name) \(.size)")'
