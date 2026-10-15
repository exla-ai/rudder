#!/usr/bin/env bash
# Build a Debug simulator binary without xcodebuild and launch it headless.
# Usage: ios/scripts/run-sim.sh [server-url] [token]   (screenshot -> ios/build/sim.png)
set -euo pipefail
IOS_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$IOS_DIR/build/sim"; APP="$OUT/Rudder.app"; BUNDLE_ID=dev.viraat.rudder
SERVER="${1:-http://localhost:4321}"; TOKEN="${2:-rdr_simulator_test}"
rm -rf "$OUT"; mkdir -p "$APP"
xcrun -sdk iphonesimulator swiftc -Onone -DDEBUG -swift-version 6 -target arm64-apple-ios17.0-simulator \
  -parse-as-library -module-name Rudder -emit-executable -o "$APP/Rudder" "$IOS_DIR"/App/*.swift
xcrun actool "$IOS_DIR/App/Assets.xcassets" --compile "$APP" --platform iphonesimulator \
  --minimum-deployment-target 17.0 --app-icon AppIcon --target-device iphone \
  --output-partial-info-plist "$OUT/icons.plist" >/dev/null
python3 - "$IOS_DIR/App/Info.plist" "$OUT/icons.plist" "$APP/Info.plist" "$BUNDLE_ID" <<'PY'
import plistlib, sys
src, icons, dst, bundle = sys.argv[1:5]
info = plistlib.load(open(src, "rb")); info.update(plistlib.load(open(icons, "rb")))
info.update({"CFBundleExecutable": "Rudder", "CFBundleIdentifier": bundle, "CFBundleName": "Rudder",
  "CFBundlePackageType": "APPL", "CFBundleShortVersionString": "1.0", "CFBundleVersion": "1",
  "CFBundleDevelopmentRegion": "en", "CFBundleSupportedPlatforms": ["iPhoneSimulator"], "MinimumOSVersion": "17.0",
  "UIDeviceFamily": [1]})
plistlib.dump(info, open(dst, "wb"))
PY
codesign --force --sign - "$APP"
UDID=$(xcrun simctl list devices available -j | python3 -c 'import json,sys; d=json.load(sys.stdin)["devices"]; print(next(x["udid"] for k,v in d.items() if "iOS" in k for x in v if x["name"].startswith("iPhone")))')
xcrun simctl bootstatus "$UDID" -b >/dev/null
xcrun simctl install "$UDID" "$APP"
xcrun simctl terminate "$UDID" "$BUNDLE_ID" 2>/dev/null || true
xcrun simctl launch "$UDID" "$BUNDLE_ID" -RudderServerURL "$SERVER" -probeToken "$TOKEN" >/dev/null
echo "$UDID"
