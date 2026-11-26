#!/usr/bin/env bash
# The iPhone app's App Store upload, one stage per call, run on an Offstage
# Mac by .offstage/ios-appstore.mjs from the checkout's root (which holds ios/):
#
#   archive  xcodegen, then a Release archive for iOS devices, built unsigned
#            and then signed ad hoc with the app's entitlements, and
#            build/upload.env with the version and build number in it
#   upload   Xcode signs the archive for App Store distribution with the
#            team's cloud-managed certificate, once into build/check to see
#            that the signed app has every entitlement, then again to upload
#            it to App Store Connect. Needs IOS_ASC_KEY_ID, IOS_ASC_ISSUER_ID
#            and IOS_ASC_KEY_P8, an Admin key of the team in ios/project.yml
#            (cloud signing needs Admin), so no certificate or provisioning
#            profile is stored.
#   collect  ../out.tgz with the logs and build/upload.env
#
# Each stage is its own command in the same sandbox, so tools/ and ios/build/
# carry over. The account has no root and no Homebrew, so tools go next to the
# checkout.
set -euo pipefail

stage="${1:?usage: ios/scripts/appstore.sh archive|upload|collect}"
root="$PWD"
tools="$root/../tools"
source "$root/ios/scripts/identity.sh"

xcode="$(ls -d /Applications/Xcode*.app | sort -V | tail -1)"
export DEVELOPER_DIR="$xcode/Contents/Developer"

# Fail with the lines that say why, not the whole log.
run_logged() {
  local log="$1"; shift
  if ! "$@" > "$log" 2>&1; then
    grep -E "error:|FAILED" -A3 "$log" | cut -c1-300 | tail -60
    echo "failed: $1 $2 (see $(basename "$log"))"
    exit 1
  fi
}

# Stops unless the app's signature carries every entitlement the project
# asks for (ios/App/Rudder.entitlements). An app uploaded without one passes
# App Store Connect and then fails on the phone: no Sign in with Apple.
require_entitlements() {
  local app="$1" signed key missing=0
  signed="$(codesign -d --entitlements - --xml "$app" 2>/dev/null || true)"
  for key in $(sed -n 's/.*<key>\([^<]*\)<\/key>.*/\1/p' "$root/ios/App/Rudder.entitlements"); do
    if [[ "$signed" != *"<key>$key</key>"* ]]; then
      echo "$app is signed without the entitlement $key"
      missing=1
    fi
  done
  if [ "$missing" != 0 ]; then exit 1; fi
}

# ExportOptions.plist for `xcodebuild -exportArchive`: cloud-managed App Store
# signing, written to disk (export) or sent to App Store Connect (upload).
export_options() {
  cat > build/ExportOptions.plist <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>method</key><string>app-store-connect</string>
  <key>destination</key><string>$1</string>
  <key>signingStyle</key><string>automatic</string>
  <key>teamID</key><string>$TEAM_ID</string>
  <key>manageAppVersionAndBuildNumber</key><false/>
</dict>
</plist>
PLIST
}

case "$stage" in
  archive)
    mkdir -p "$tools"
    cd "$tools"
    curl -fsSL -o xcodegen.zip https://github.com/yonaskolb/XcodeGen/releases/download/2.46.0/xcodegen.zip
    echo "4d9e34b62172d645eed6457cac13fc222569974098ef4ee9c3368bedf0196806  xcodegen.zip" | shasum -a 256 -c -
    unzip -q -o xcodegen.zip
    cd "$root/ios"
    xcodebuild -version | sed -n 1p
    "$tools/xcodegen/bin/xcodegen" generate --quiet
    # Built unsigned: signing a device build needs a development certificate
    # and a registered device, and this Mac has neither. Xcode applies the
    # distribution signature at upload.
    run_logged archive.log xcodebuild archive -project Rudder.xcodeproj -scheme Rudder -configuration Release \
      -destination 'generic/platform=iOS' -archivePath build/Rudder.xcarchive \
      CODE_SIGNING_ALLOWED=NO
    app=build/Rudder.xcarchive/Products/Applications/Rudder.app
    # At upload Xcode keeps the entitlements it reads from the archived app's
    # signature, and an unsigned app has none. So the archived app is signed
    # ad hoc with the project's entitlements plus the two every signed app has.
    # (Asking xcodebuild for the ad hoc signature fails: "has entitlements that
    # require signing with a development certificate".)
    cp App/Rudder.entitlements build/archive.entitlements
    /usr/libexec/PlistBuddy -c "Add :application-identifier string $TEAM_ID.$BUNDLE_ID" \
      -c "Add :com.apple.developer.team-identifier string $TEAM_ID" build/archive.entitlements
    run_logged adhoc-sign.log codesign --force --sign - --entitlements build/archive.entitlements "$app"
    require_entitlements "$app"
    if [ ! -f "$app/PrivacyInfo.xcprivacy" ]; then
      echo "the archived app has no PrivacyInfo.xcprivacy; App Store Connect requires it"
      exit 1
    fi
    bundle=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Info.plist")
    if [ "$bundle" != "$BUNDLE_ID" ]; then
      echo "the archived app is $bundle, not $BUNDLE_ID"
      exit 1
    fi
    {
      echo "TEAM=$TEAM_ID"
      echo "BUNDLE_ID=$bundle"
      echo "VERSION=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Info.plist")"
      echo "BUILD=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$app/Info.plist")"
    } > build/upload.env
    cat build/upload.env
    ;;
  upload)
    : "${IOS_ASC_KEY_ID:?IOS_ASC_KEY_ID is not set}" "${IOS_ASC_ISSUER_ID:?IOS_ASC_ISSUER_ID is not set}" "${IOS_ASC_KEY_P8:?IOS_ASC_KEY_P8 is not set}"
    key="$HOME/.appstoreconnect/private_keys/AuthKey_$IOS_ASC_KEY_ID.p8"
    mkdir -p "$(dirname "$key")"
    (umask 077 && printf '%s\n' "$IOS_ASC_KEY_P8" > "$key")
    trap 'rm -f "$key"' EXIT
    cd "$root/ios"
    # Signed for the App Store into build/check first, so an app that lost an
    # entitlement on the way is caught here and never reaches App Store Connect.
    rm -rf build/check build/export
    export_options export
    run_logged export-check.log xcodebuild -exportArchive -archivePath build/Rudder.xcarchive \
      -exportOptionsPlist build/ExportOptions.plist -exportPath build/check \
      -allowProvisioningUpdates -authenticationKeyPath "$key" \
      -authenticationKeyID "$IOS_ASC_KEY_ID" -authenticationKeyIssuerID "$IOS_ASC_ISSUER_ID"
    ipa="$(ls build/check/*.ipa)"
    unzip -q "$ipa" -d build/check/ipa
    require_entitlements build/check/ipa/Payload/Rudder.app
    export_options upload
    run_logged export.log xcodebuild -exportArchive -archivePath build/Rudder.xcarchive \
      -exportOptionsPlist build/ExportOptions.plist -exportPath build/export \
      -allowProvisioningUpdates -authenticationKeyPath "$key" \
      -authenticationKeyID "$IOS_ASC_KEY_ID" -authenticationKeyIssuerID "$IOS_ASC_ISSUER_ID"
    # App Store Connect's warnings (an SDK deadline, a missing API reason) come back here.
    grep -E "warning: |Uploaded |EXPORT SUCCEEDED" export.log | cut -c1-300
    ;;
  collect)
    files=()
    for f in ios/*.log ios/build/upload.env; do
      if [ -e "$f" ]; then files+=("$f"); fi
    done
    if [ ${#files[@]} -eq 0 ]; then echo "nothing to collect"; exit 3; fi
    tar -czf ../out.tgz "${files[@]}"
    ;;
  *)
    echo "unknown stage: $stage (archive, upload or collect)" >&2
    exit 2
    ;;
esac
