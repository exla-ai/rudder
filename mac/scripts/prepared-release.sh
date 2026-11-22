#!/usr/bin/env bash
# Run from the checked-out release commit on a Linux Actions runner.
set -euo pipefail
action="${1:?expected sign, verify or publish}"
dir="${2:?expected the directory containing Rudder.dmg and release.env}"
fail() { echo "::error::$*" >&2; exit 1; }

version=$(sed -n 's/^VERSION=//p' "$dir/release.env")
build=$(sed -n 's/^BUILD=//p' "$dir/release.env")
minimum=$(sed -n 's/^MINIMUM_SYSTEM=//p' "$dir/release.env")
[[ "$version" == "$(node -p 'require("./package.json").version')" ]] || fail "DMG version differs from package.json"
[[ "$build" == "$(sed -n 's/^ *CURRENT_PROJECT_VERSION: "\([0-9]*\)"$/\1/p' mac/project.yml)" ]] || fail "DMG build differs from mac/project.yml"
args=(--dmg "$dir/Rudder.dmg" --version "$version" --build "$build" --minimum-system "$minimum" --info-plist mac/App/Info.plist)
verify() { node mac/scripts/appcast.mjs verify "${args[@]}" --appcast "$dir/appcast.xml"; }
case "$action" in
  sign)
    [[ "$GITHUB_REF" == "refs/heads/release-check/$version" ]] || fail "Candidate branch must match $version"
    key=$(mktemp)
    trap 'rm -f "$key"' EXIT
    printf '%s' "${SPARKLE_ED_PRIVATE_KEY:?missing Sparkle signing key}" > "$key"
    node mac/scripts/appcast.mjs write "${args[@]}" --key "$key" --out "$dir/appcast.xml"
    verify
    ;;
  verify) verify ;;
  publish)
    [[ "$GITHUB_REF" == "refs/tags/v$version" ]] || fail "Publish from the v$version tag"
    verify
    tag="mac-v$version-$build"
    existing=$(gh api "repos/$GITHUB_REPOSITORY/git/matching-refs/tags/$tag" \
      --jq ".[] | select(.ref == \"refs/tags/$tag\") | .object.sha")
    if [[ -z "$existing" ]]; then
      gh release create "$tag" "$dir/Rudder.dmg" "$dir/appcast.xml" --target "$GITHUB_SHA" --latest \
        --title "Rudder for Mac $version ($build)" \
        --notes "Signed and notarized. Download Rudder for Mac at https://rudder.exla.ai/download/mac"
    elif [[ "$existing" != "$GITHUB_SHA" ]]; then
      fail "$tag already points to a different commit"
    fi
    [[ "$(gh api "repos/$GITHUB_REPOSITORY/releases/latest" --jq .tag_name)" == "$tag" ]] || fail "$tag is not the latest release"
    ;;
  *) fail "Unknown action $action" ;;
esac
