#!/usr/bin/env bash
# The iPhone click-through, one stage per call, run on an Offstage Mac by
# .offstage/ios-qa.mjs from the checkout's root (which holds ios/):
#
#   build    xcodegen, then the app and its UI test built for the simulator
#   run      boot an iPhone, record its screen, run ios/UITests/steps.json
#            against QA_SERVER signed in with QA_PAIR_CODE
#   shots    one App Store screenshot set: ios/UITests/appstore.json on the
#            simulator named QA_DEVICE, signed in with QA_PAIR_CODE (a demo
#            account), saved as JPEG in qa-out/appstore/QA_DISPLAY_TYPE
#   collect  ../out.tgz: clickthrough.mp4, the screenshots, the trees and logs
#
# Nothing from the signed-in account is printed: it goes to files only.
set -euo pipefail

stage="${1:?usage: ios/scripts/qa.sh build|run|shots|collect}"
root="$PWD"
tools="$root/../tools"
out="$root/../qa-out"
derived="$root/../derived"
source "$root/ios/scripts/identity.sh"
# The UI test runner (RudderUITests in ios/project.yml), where the test writes its output.
runner="$BUNDLE_ID.uitests.xctrunner"
xcode="$(ls -d /Applications/Xcode*.app | sort -V | tail -1)"
export DEVELOPER_DIR="$xcode/Contents/Developer"

quiet() {
  local log="$1"; shift
  if ! "$@" > "$log" 2>&1; then
    grep -E "error:|BUILD FAILED|Testing failed" "$log" | cut -c1-300 | tail -30 || true
    echo "failed: $1 $2 (see $(basename "$log"))"
    return 1
  fi
}

case "$stage" in
  build)
    mkdir -p "$tools" "$out"
    cd "$tools"
    curl -fsSL -o xcodegen.zip https://github.com/yonaskolb/XcodeGen/releases/download/2.46.0/xcodegen.zip
    echo "4d9e34b62172d645eed6457cac13fc222569974098ef4ee9c3368bedf0196806  xcodegen.zip" | shasum -a 256 -c -
    unzip -q -o xcodegen.zip
    cd "$root/ios"
    "$tools/xcodegen/bin/xcodegen" generate --quiet
    udid=$(xcrun simctl list devices available -j | python3 -c 'import json,sys; d=json.load(sys.stdin)["devices"]; print(next(x["udid"] for k,v in sorted(d.items(), reverse=True) if "iOS" in k for x in v if x["name"].startswith("iPhone")))')
    echo "$udid" > "$out/udid"
    quiet "$out/build.log" xcodebuild build-for-testing -project Rudder.xcodeproj -scheme Rudder -configuration Debug \
      -destination "platform=iOS Simulator,id=$udid" -derivedDataPath "$derived"
    echo "built for $(xcrun simctl list devices | grep "$udid" | sed 's/ (.*//' | xargs)"
    ;;
  run)
    : "${QA_PAIR_CODE:?QA_PAIR_CODE is not set}" "${QA_SERVER:?QA_SERVER is not set}"
    udid=$(cat "$out/udid")
    xcrun simctl bootstatus "$udid" -b > /dev/null
    xcrun simctl ui "$udid" appearance dark
    xcrun simctl uninstall "$udid" "$BUNDLE_ID" 2>/dev/null || true
    rm -f "$out/clickthrough.mp4"
    nohup xcrun simctl io "$udid" recordVideo --codec hevc --force "$out/clickthrough.mp4" > "$out/record.log" 2>&1 &
    recorder=$!
    sleep 2
    code=0
    cd "$root/ios"
    TEST_RUNNER_QA_PAIR_CODE="$QA_PAIR_CODE" TEST_RUNNER_QA_SERVER="$QA_SERVER" TEST_RUNNER_QA_STEPS=steps \
      quiet "$out/test.log" xcodebuild test-without-building -project Rudder.xcodeproj -scheme Rudder -configuration Debug \
      -destination "platform=iOS Simulator,id=$udid" -derivedDataPath "$derived" || code=$?
    kill -INT "$recorder"
    wait "$recorder" 2>/dev/null || true
    data=$(xcrun simctl get_app_container "$udid" "$runner" data)
    rm -rf "$out/shots"
    cp -R "$data/Documents/qa" "$out/shots"
    echo "$(ls "$out/shots" | grep -c '\.png$') screenshots, $(grep -c '^MISS' "$out/shots/log.txt" || true) steps missed, video $(du -h "$out/clickthrough.mp4" | cut -f1)"
    exit "$code"
    ;;
  shots)
    : "${QA_PAIR_CODE:?QA_PAIR_CODE is not set}" "${QA_SERVER:?QA_SERVER is not set}"
    : "${QA_DEVICE:?QA_DEVICE is not set}" "${QA_DISPLAY_TYPE:?QA_DISPLAY_TYPE is not set}"
    udid=$(xcrun simctl list devices available -j | python3 -c '
import json, sys
devices = json.load(sys.stdin)["devices"]
ios = [(runtime, d) for runtime, ds in sorted(devices.items(), reverse=True) if "iOS" in runtime for d in ds]
match = next((d["udid"] for _, d in ios if d["name"] == sys.argv[1]), None)
if not match:
    sys.exit("no available simulator named %s; these are: %s" % (sys.argv[1], ", ".join(sorted({d["name"] for _, d in ios}))))
print(match)' "$QA_DEVICE")
    xcrun simctl bootstatus "$udid" -b > /dev/null
    xcrun simctl ui "$udid" appearance dark
    # The status bar Apple's own screenshots show.
    xcrun simctl status_bar "$udid" override --time 9:41 --dataNetwork wifi --wifiMode active --wifiBars 3 \
      --cellularMode active --cellularBars 4 --batteryState charged --batteryLevel 100
    xcrun simctl uninstall "$udid" "$BUNDLE_ID" 2>/dev/null || true
    dest="$out/appstore/$QA_DISPLAY_TYPE"
    rm -rf "$dest"
    mkdir -p "$dest"
    code=0
    cd "$root/ios"
    TEST_RUNNER_QA_PAIR_CODE="$QA_PAIR_CODE" TEST_RUNNER_QA_SERVER="$QA_SERVER" TEST_RUNNER_QA_STEPS=appstore \
      quiet "$dest/test.log" xcodebuild test-without-building -project Rudder.xcodeproj -scheme Rudder -configuration Debug \
      -destination "platform=iOS Simulator,id=$udid" -derivedDataPath "$derived" || code=$?
    data=$(xcrun simctl get_app_container "$udid" "$runner" data)
    # App Store Connect refuses images with an alpha channel; JPEG has none.
    # A test that stopped early leaves fewer files, or none.
    shopt -s nullglob
    for png in "$data"/Documents/qa/*.png; do
      sips -s format jpeg -s formatOptions 92 "$png" --out "$dest/$(basename "${png%.png}").jpg" > /dev/null
    done
    for txt in "$data"/Documents/qa/*.txt; do cp "$txt" "$dest/"; done
    xcrun simctl shutdown "$udid"
    echo "$QA_DISPLAY_TYPE: $(ls "$dest" | grep -c '\.jpg$') screenshots on $QA_DEVICE, $(grep -c '^MISS' "$dest/log.txt" || true) steps missed"
    exit "$code"
    ;;
  collect)
    cd "$root/.."
    # One read of a large file can drop: hand it over in 8 MiB parts.
    rm -f out.part.*
    COPYFILE_DISABLE=1 tar -czf out.tgz qa-out
    split -b 8m out.tgz out.part.
    ls out.part.*
    ;;
  *)
    echo "unknown stage $stage"; exit 2
    ;;
esac
