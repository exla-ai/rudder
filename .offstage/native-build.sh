#!/usr/bin/env bash
# Builds the darwin-arm64 release binaries inside an Offstage task checkout.
set -euo pipefail

[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || {
  echo "Native release requires an Apple silicon Mac" >&2
  exit 1
}
sw_vers
xcrun --sdk macosx --show-sdk-version

tools="$PWD/.offstage-tools"
export CARGO_HOME="$tools/cargo"
export RUSTUP_HOME="$tools/rustup"
export RUSTUP_TOOLCHAIN=1.99.1
export CARGO_TARGET_DIR="$PWD/target"
export CARGO_BUILD_JOBS=4
export CARGO_TERM_COLOR=never
export PATH="$tools/node/bin:$CARGO_HOME/bin:$PATH"
mkdir -p "$tools/node"

echo "Installing Node and Rust"
curl -fsSL https://nodejs.org/dist/v22.23.5/node-v22.23.5-darwin-arm64.tar.gz -o "$tools/node.tgz"
echo "e7131995759b61a0170b646010d2ad35a5d37b4cb0d471f3e8317eac6f742f3d  $tools/node.tgz" | shasum -a 256 -c -
tar -xzf "$tools/node.tgz" --strip-components 1 -C "$tools/node"
curl -fsSL https://static.rust-lang.org/rustup/archive/1.29.1/aarch64-apple-darwin/rustup-init -o "$tools/rustup-init"
echo "ec1b9233e7f72990ecd8e62063fa7f6c3dfc2bec8e97f88bff165f9100ac696a  $tools/rustup-init" | shasum -a 256 -c -
chmod 700 "$tools/rustup-init"
"$tools/rustup-init" -y --no-modify-path --profile minimal --default-toolchain "$RUSTUP_TOOLCHAIN"
node --version
rustc --version --verbose
cargo --version

echo "Running board host access preflight"
npm ci --ignore-scripts
node --test tests/board-capability.test.mjs tests/board-host-access.test.mjs

echo "Building macOS native pair"
node scripts/build-native.mjs
for name in rudder-native rudder-jj; do
  binary="target/release/$name"
  [[ -x "$binary" && "$(xcrun lipo -archs "$binary")" == arm64 ]] || {
    echo "$binary is not an executable arm64 Mach-O" >&2
    exit 1
  }
done

artifact=native-dist/darwin-arm64
mkdir -p "$artifact"
cp target/release/rudder-native target/release/rudder-jj "$artifact/"
node scripts/native-artifacts.mjs create --directory "$artifact" --platform darwin-arm64 --revision "$(git rev-parse HEAD)"
COPYFILE_DISABLE=1 tar -czf native-release.tgz -C "$artifact" rudder-native rudder-jj native-manifest.json
shasum -a 256 native-release.tgz > native-release.sha256
