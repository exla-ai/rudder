#!/usr/bin/env bash
# Build Rudder on this Mac and install it straight onto a paired iPhone: no
# TestFlight, no App Store Connect, no Apple ID 2FA. Needs the development
# profile from `cd ios && fastlane dev_signing` (registers the phone once).
# Usage: ios/scripts/install-device.sh [device-id]   (default: first paired iPhone)
set -euo pipefail
IOS_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$IOS_DIR/build/device"; APP="$OUT/Rudder.app"; BUNDLE_ID=dev.viraat.rudder
PROFILE="$IOS_DIR/build/profiles/Development_$BUNDLE_ID.mobileprovision"
[[ -f "$PROFILE" ]] || { echo "error: run 'cd ios && fastlane dev_signing' first" >&2; exit 1; }
IDENTITY="${SIGNING_IDENTITY:-Apple Development}"
VERSION=$(sed -n 's/^ *MARKETING_VERSION: *"\(.*\)"/\1/p' "$IOS_DIR/project.yml")
BUILD=$(sed -n 's/^ *CURRENT_PROJECT_VERSION: *"\(.*\)"/\1/p' "$IOS_DIR/project.yml")
DEVICE="${1:-$(xcrun devicectl list devices 2>/dev/null | awk '/iPhone/ && /available/ {for (i=1;i<=NF;i++) if ($i ~ /^[0-9A-F-]{36}$/) {print $i; exit}}')}"
[[ -n "$DEVICE" ]] || { echo "error: no paired iPhone available" >&2; exit 1; }

rm -rf "$OUT"; mkdir -p "$APP"
echo "==> Compiling"
xcrun -sdk iphoneos swiftc -O -swift-version 6 -target arm64-apple-ios17.0 -parse-as-library \
  -module-name Rudder -emit-executable -o "$APP/Rudder" "$IOS_DIR"/App/*.swift
xcrun actool "$IOS_DIR/App/Assets.xcassets" --compile "$APP" --platform iphoneos \
  --minimum-deployment-target 17.0 --app-icon AppIcon --target-device iphone \
  --output-partial-info-plist "$OUT/icons.plist" >/dev/null
python3 - "$IOS_DIR/App/Info.plist" "$OUT/icons.plist" "$APP/Info.plist" "$BUNDLE_ID" "$VERSION" "$BUILD" <<'PY'
import plistlib, sys
src, icons, dst, bundle, version, build = sys.argv[1:7]
info = plistlib.load(open(src, "rb")); info.update(plistlib.load(open(icons, "rb")))
info.update({"CFBundleExecutable": "Rudder", "CFBundleIdentifier": bundle, "CFBundleName": "Rudder",
  "CFBundlePackageType": "APPL", "CFBundleShortVersionString": version, "CFBundleVersion": build,
  "CFBundleDevelopmentRegion": "en", "CFBundleInfoDictionaryVersion": "6.0", "CFBundleSupportedPlatforms": ["iPhoneOS"],
  "LSRequiresIPhoneOS": True, "MinimumOSVersion": "17.0", "UIDeviceFamily": [1], "UIRequiredDeviceCapabilities": ["arm64"],
  "DTPlatformName": "iphoneos"})
plistlib.dump(info, open(dst, "wb"))
PY
printf 'APPL????' > "$APP/PkgInfo"
cp "$PROFILE" "$APP/embedded.mobileprovision"
echo "==> Signing"
security cms -D -i "$PROFILE" | plutil -extract Entitlements xml1 -o "$OUT/entitlements.plist" -
codesign --force --sign "$IDENTITY" --entitlements "$OUT/entitlements.plist" --timestamp=none "$APP"
echo "==> Installing on $DEVICE"
xcrun devicectl device install app --device "$DEVICE" "$APP" >/dev/null
xcrun devicectl device process launch --device "$DEVICE" "$BUNDLE_ID" >/dev/null 2>&1 || true
echo "==> Rudder $VERSION ($BUILD) is on the phone"
