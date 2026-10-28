#!/usr/bin/env bash
# Screenshots of the app on iPad simulators, signed in against the web app's
# dev mock: an iPad (9th generation) in portrait (sidebar, plan inline) and
# landscape (sidebar, plan column), and a 13-inch iPad in portrait.
# Usage: ios/scripts/ipad-screens.sh [out-dir]
# Needs Xcode, xcodegen's Rudder.xcodeproj (cd ios && xcodegen generate) and node.
set -euo pipefail
IOS_DIR="$(cd "$(dirname "$0")/.." && pwd)"; ROOT="$(dirname "$IOS_DIR")"
OUT="${1:-$IOS_DIR/build/screens}"; mkdir -p "$OUT"
BUNDLE_ID=dev.viraat.rudder; PORT="${PORT:-4321}"; SERVER="http://localhost:$PORT"

MOCK_ONBOARDED=1 PORT="$PORT" node "$ROOT/cloud/app/dev-mock.mjs" >"$OUT/mock.log" 2>&1 &
MOCK=$!
trap 'kill $MOCK 2>/dev/null || true' EXIT
for _ in $(seq 50); do curl -sf "$SERVER/app" >/dev/null && break; sleep 0.2; done

xcodebuild -project "$IOS_DIR/Rudder.xcodeproj" -scheme Rudder -configuration Debug \
  -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath "$IOS_DIR/build/dd" CODE_SIGNING_ALLOWED=NO build >"$OUT/build.log" 2>&1 \
  || { tail -40 "$OUT/build.log"; exit 1; }
APP="$IOS_DIR/build/dd/Build/Products/Debug-iphonesimulator/Rudder.app"
codesign --force --sign - "$APP"

# The newest iOS 18 runtime (what the iPad runs) that has the device, else the newest.
pick() {
  xcrun simctl list runtimes available -j | python3 -c '
import json, sys
wanted = sys.argv[1].split("|")
runtimes = [r for r in json.load(sys.stdin)["runtimes"] if r.get("platform") == "iOS"]
runtimes.sort(key=lambda r: [int(x) for x in r["version"].split(".")], reverse=True)
runtimes.sort(key=lambda r: not r["version"].startswith("18."))
for name in wanted:
    for r in runtimes:
        for d in r.get("supportedDeviceTypes", []):
            if d["name"] == name:
                print(r["identifier"], d["identifier"], sep="\t"); sys.exit(0)
sys.exit("no runtime with " + sys.argv[1])
' "$1"
}

shot() { # device-label shot-name [launch args...]
  local label=$1 name=$2; shift 2
  xcrun simctl terminate "$UDID" "$BUNDLE_ID" 2>/dev/null || true
  xcrun simctl launch "$UDID" "$BUNDLE_ID" -RudderServerURL "$SERVER" "$@" >/dev/null
  sleep 8
  xcrun simctl io "$UDID" screenshot "$OUT/$label-$name.png" >/dev/null
  echo "$OUT/$label-$name.png"
}

signed_in() { # device-label suffix
  shot "$1" "home$2" -probeToken rdr_simulator -startPath /app
  shot "$1" "project$2" -probeToken rdr_simulator -startPath /app/p/wsp_rudder
  shot "$1" "agent$2" -probeToken rdr_simulator -startPath /app/p/wsp_rudder/a/run_a1
}

# Rotates the Simulator window's device (Cmd+Right). Needs the Simulator app
# and Accessibility for System Events; best effort.
rotate() {
  open -a Simulator --args -CurrentDeviceUDID "$UDID"; sleep 4
  osascript -e 'tell application "Simulator" to activate' \
    -e 'delay 1' -e 'tell application "System Events" to key code 124 using command down' >/dev/null 2>&1
}

for spec in "ipad9|iPad (9th generation)|iPad (10th generation)|iPad (A16)" \
            "ipad13|iPad Pro 13-inch (M4)|iPad Pro (12.9-inch) (6th generation)|iPad Air 13-inch (M3)|iPad Air 13-inch (M2)"; do
  label=${spec%%|*}
  read -r RUNTIME DEVTYPE < <(pick "${spec#*|}")
  UDID=$(xcrun simctl create "rudder-$label" "$DEVTYPE" "$RUNTIME")
  echo "$label: $DEVTYPE on $RUNTIME ($UDID)"
  xcrun simctl boot "$UDID"; xcrun simctl bootstatus "$UDID" -b >/dev/null
  xcrun simctl install "$UDID" "$APP"
  shot "$label" signin
  signed_in "$label" ""
  if [ "$label" = ipad9 ]; then
    xcrun simctl ui "$UDID" appearance dark
    shot "$label" home-dark -probeToken rdr_simulator -startPath /app
    xcrun simctl ui "$UDID" appearance light
    if rotate; then
      signed_in "$label" -landscape
      shot "$label" signin-landscape
    else
      echo "could not rotate the simulator (no Accessibility for System Events)"
    fi
    osascript -e 'tell application "Simulator" to quit' >/dev/null 2>&1 || true
  fi
  xcrun simctl shutdown "$UDID" || true
done
