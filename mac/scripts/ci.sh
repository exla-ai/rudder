#!/usr/bin/env bash
# The Mac app's CI, one stage per call, run on an offstage Mac by
# .offstage/mac.mjs from the checkout's root (which holds mac/):
#
#   check     pinned tools, xcodegen, the core tests and a Debug build
#   sign      archive and export signed with the Developer ID certificate, verify
#   notarize  notarize and staple the app
#   dmg       Rudder.dmg, signed, notarized and stapled, and build/release.env
#   collect   ../out.tgz with the logs and whatever dmg made
#
# Each stage is its own command (30 minutes at most) in the same sandbox, so
# tools/ and mac/build/ carry over between them. sign, notarize and dmg need
# ASC_KEY_ID, ASC_ISSUER_ID and ASC_KEY_P8 (the App Store Connect key, for
# provisioning and notarytool); sign also needs MAC_DEVELOPER_ID_P12 (base64 of
# the exported "Developer ID Application" certificate with its private key)
# and MAC_DEVELOPER_ID_PASSWORD. Apple refuses cloud-managed Developer ID
# signing for this key ("Cloud signing permission error"), so the certificate
# goes into a throwaway keychain. The account has no root and no Homebrew, so
# tools go next to the checkout.
set -euo pipefail

stage="${1:?usage: mac/scripts/ci.sh check|sign|notarize|dmg|collect}"
root="$PWD"
tools="$root/../tools"
TEAM=3C4383262W

xcode="$(ls -d /Applications/Xcode*.app | sort -V | tail -1)"
export DEVELOPER_DIR="$xcode/Contents/Developer"
export UV_PYTHON_INSTALL_DIR="$tools/python"

# Fail with the lines that say why, not a 1 MiB log.
run_logged() {
  local log="$1"; shift
  if ! "$@" > "$log" 2>&1; then
    grep -E "error:|Testing failed|FAILED|xctest encountered" -A3 "$log" | tail -60
    echo "failed: $* (see $log)"
    exit 1
  fi
}

install_tools() {
  mkdir -p "$tools"
  cd "$tools"
  fetch() { curl -fsSL -o "$2" "$1" && echo "$3  $2" | shasum -a 256 -c -; }
  fetch https://github.com/yonaskolb/XcodeGen/releases/download/2.46.0/xcodegen.zip xcodegen.zip \
    4d9e34b62172d645eed6457cac13fc222569974098ef4ee9c3368bedf0196806
  fetch https://github.com/astral-sh/uv/releases/download/0.13.7/uv-aarch64-apple-darwin.tar.gz uv.tar.gz \
    81b811b57e4624df7ed3aac148e0359100d9962f3d9dce00aec533dc2e086731
  unzip -q xcodegen.zip
  tar xzf uv.tar.gz
  # dmgbuild needs Python 3.10 or newer; the Mac ships 3.9.
  uv-aarch64-apple-darwin/uv python install 3.12
  cd "$root"
}

use_tools() {
  local python
  python="$("$tools/uv-aarch64-apple-darwin/uv" python find 3.12)"
  export PATH="$tools/xcodegen/bin:$(dirname "$python"):$PATH"
}

asc_key() {
  : "${ASC_KEY_ID:?ASC_KEY_ID is not set}" "${ASC_ISSUER_ID:?ASC_ISSUER_ID is not set}" "${ASC_KEY_P8:?ASC_KEY_P8 is not set}"
  KEY="$HOME/.appstoreconnect/private_keys/AuthKey_$ASC_KEY_ID.p8"
  mkdir -p "$(dirname "$KEY")"
  (umask 077 && printf '%s\n' "$ASC_KEY_P8" > "$KEY")
  AUTH=(-allowProvisioningUpdates -authenticationKeyPath "$KEY"
        -authenticationKeyID "$ASC_KEY_ID" -authenticationKeyIssuerID "$ASC_ISSUER_ID")
}

developer_id_keychain() {
  : "${MAC_DEVELOPER_ID_P12:?MAC_DEVELOPER_ID_P12 is not set}" "${MAC_DEVELOPER_ID_PASSWORD:?MAC_DEVELOPER_ID_PASSWORD is not set}"
  local kc="$HOME/signing.keychain-db" kc_pass p12="$HOME/devid.p12"
  kc_pass=$(uuidgen)
  (umask 077 && printf '%s' "$MAC_DEVELOPER_ID_P12" | base64 --decode > "$p12")
  security create-keychain -p "$kc_pass" "$kc"
  security set-keychain-settings -lut 21600 "$kc"
  security unlock-keychain -p "$kc_pass" "$kc"
  security import "$p12" -k "$kc" -P "$MAC_DEVELOPER_ID_PASSWORD" -T /usr/bin/codesign -T /usr/bin/security
  rm "$p12"
  security set-key-partition-list -S apple-tool:,apple: -s -k "$kc_pass" "$kc" > /dev/null
  security list-keychains -d user -s "$kc" $(security list-keychains -d user | tr -d '"')
  security find-identity -v -p codesigning "$kc" | grep "Developer ID Application"
}

notarize() {
  xcrun notarytool submit "$1" --key "$KEY" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID" \
    --wait --timeout 25m --output-format json | tee notary.json
  local id
  id=$(python3 -c 'import json;print(json.load(open("notary.json"))["id"])')
  xcrun notarytool log "$id" --key "$KEY" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID" > "notary-$id.json"
  python3 -c 'import json,sys;sys.exit(0 if json.load(open("notary.json"))["status"]=="Accepted" else 1)'
}

case "$stage" in
  check)
    install_tools
    use_tools
    xcodebuild -version | head -1
    echo "xcodegen $(xcodegen --version), $(python3 --version)"
    cd mac
    xcodegen generate --quiet
    run_logged test.log xcodebuild test -project Rudder.xcodeproj -scheme RudderCoreTests \
      -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO
    grep -E "Executed [0-9]+ tests" test.log | tail -1
    run_logged build.log xcodebuild -project Rudder.xcodeproj -scheme Rudder -configuration Debug \
      -destination 'generic/platform=macOS' CODE_SIGNING_ALLOWED=NO build
    echo "Debug build succeeded"
    ;;
  sign)
    use_tools
    asc_key
    developer_id_keychain
    cd mac
    run_logged archive.log xcodebuild -project Rudder.xcodeproj -scheme Rudder -configuration Release \
      -destination 'generic/platform=macOS' -archivePath build/Rudder.xcarchive "${AUTH[@]}" \
      DEVELOPMENT_TEAM="$TEAM" CODE_SIGN_STYLE=Automatic archive
    cat > build/ExportOptions.plist <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>method</key><string>developer-id</string>
  <key>signingStyle</key><string>automatic</string>
  <key>teamID</key><string>$TEAM</string>
</dict>
</plist>
PLIST
    run_logged export.log xcodebuild -exportArchive -archivePath build/Rudder.xcarchive \
      -exportOptionsPlist build/ExportOptions.plist -exportPath build/export "${AUTH[@]}"
    codesign --verify --deep --strict --verbose=2 build/export/Rudder.app
    codesign -dv --verbose=2 build/export/Rudder.app 2>&1 | tee sign.log
    grep -q "Authority=Developer ID Application" sign.log
    grep -q "flags=.*runtime" sign.log
    echo "signed with Developer ID"
    ;;
  notarize)
    use_tools
    asc_key
    cd mac/build
    ditto -c -k --keepParent export/Rudder.app Rudder-notarize.zip
    notarize Rudder-notarize.zip
    xcrun stapler staple export/Rudder.app
    spctl --assess --type execute --verbose=2 export/Rudder.app
    ;;
  dmg)
    use_tools
    asc_key
    cd mac/build
    ../scripts/make-dmg.sh export/Rudder.app Rudder.dmg
    # Signed like build.sh signs it: the sign stage's keychain is still in
    # this sandbox's search list.
    codesign --sign "Developer ID Application" --timestamp Rudder.dmg
    notarize Rudder.dmg
    xcrun stapler staple Rudder.dmg
    spctl --assess --type open --context context:primary-signature --verbose=2 Rudder.dmg
    shasum -a 256 Rudder.dmg
    plist=export/Rudder.app/Contents/Info.plist
    {
      echo "VERSION=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$plist")"
      echo "BUILD=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$plist")"
    } > release.env
    cat release.env
    ;;
  collect)
    # The logs always; Rudder.dmg and release.env once dmg made them.
    files=()
    for f in mac/*.log mac/build/*.json mac/build/Rudder.dmg mac/build/release.env; do
      if [ -e "$f" ]; then files+=("$f"); fi
    done
    if [ ${#files[@]} -eq 0 ]; then echo "nothing to collect"; exit 3; fi
    tar -czf ../out.tgz "${files[@]}"
    ;;
  *)
    echo "unknown stage: $stage (check, sign, notarize, dmg or collect)" >&2
    exit 2
    ;;
esac
