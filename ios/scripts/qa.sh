#!/usr/bin/env bash
# The iPhone click-through, one stage per call, run on an Offstage Mac by
# .offstage/ios-qa.mjs from the checkout's root (which holds ios/):
#
#   build    xcodegen, then the app and its UI test built for the simulator
#   run      boot an iPhone, record its screen, run ios/UITests/steps.json
#            against QA_SERVER signed in with QA_PAIR_CODE
#   collect  ../out.tgz: clickthrough.mp4, the screenshots, the trees and logs
#
# Nothing from the signed-in account is printed: it goes to files only.
set -euo pipefail

stage="${1:?usage: ios/scripts/qa.sh build|run|collect}"
root="$PWD"
tools="$root/../tools"
out="$root/../qa-out"
derived="$root/../derived"
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
    open "$DEVELOPER_DIR/Applications/Simulator.app" --args -CurrentDeviceUDID "$udid"
    xcrun simctl ui "$udid" appearance dark
    xcrun simctl uninstall "$udid" dev.viraat.rudder 2>/dev/null || true
    rm -f "$out/clickthrough.mp4"
    nohup xcrun simctl io "$udid" recordVideo --codec h264 --force "$out/clickthrough.mp4" > "$out/record.log" 2>&1 &
    recorder=$!
    sleep 2
    code=0
    cd "$root/ios"
    TEST_RUNNER_QA_PAIR_CODE="$QA_PAIR_CODE" TEST_RUNNER_QA_SERVER="$QA_SERVER" \
      quiet "$out/test.log" xcodebuild test-without-building -project Rudder.xcodeproj -scheme Rudder -configuration Debug \
      -destination "platform=iOS Simulator,id=$udid" -derivedDataPath "$derived" || code=$?
    kill -INT "$recorder"
    wait "$recorder" 2>/dev/null || true
    data=$(xcrun simctl get_app_container "$udid" dev.viraat.rudder.uitests.xctrunner data)
    rm -rf "$out/shots"
    cp -R "$data/Documents/qa" "$out/shots"
    echo "$(ls "$out/shots" | grep -c '\.png$') screenshots, $(grep -c '^MISS' "$out/shots/log.txt" || true) steps missed, video $(du -h "$out/clickthrough.mp4" | cut -f1)"
    exit "$code"
    ;;
  collect)
    cd "$root/.."
    tar -czf out.tgz qa-out
    ;;
  *)
    echo "unknown stage $stage"; exit 2
    ;;
esac
