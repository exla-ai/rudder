#!/usr/bin/env bash
# Creates the app's record in App Store Connect, once per app. Apple's App
# Store Connect API cannot (POST /v1/apps answers 403 for every key), so this
# goes through fastlane produce, which signs in as a person: it asks for the
# Apple ID's password and two-factor code on this Mac and keeps the session in
# the Keychain. Run it on your own Mac, as someone who can add apps to the
# team in ios/project.yml (Account Holder, Admin or App Manager).
#
#   ios/scripts/create-app.sh you@example.com
#
# The bundle id must already be registered (`node ios/scripts/appstore.mjs
# identity` does that with the team's API key), so the developer portal step
# is skipped. The name comes from ios/appstore/listing.json.
set -euo pipefail

apple_id="${1:?usage: ios/scripts/create-app.sh <the Apple ID to sign in with>}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=identity.sh
source "$here/identity.sh"
name=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["name"])' "$here/../appstore/listing.json")
command -v fastlane > /dev/null || { echo "fastlane is not installed: brew install fastlane" >&2; exit 1; }

echo "Creating \"$name\" ($BUNDLE_ID) on team $TEAM_ID, signed in as $apple_id"
fastlane produce create \
  --username "$apple_id" \
  --app_identifier "$BUNDLE_ID" \
  --app_name "$name" \
  --sku rudder-ios \
  --platform ios \
  --team_id "$TEAM_ID" \
  --skip_devcenter true
echo "Created. Next: gh workflow run ios-appstore.yml --ref main -f step=identity"
