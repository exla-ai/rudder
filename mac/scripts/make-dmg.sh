#!/usr/bin/env bash
# Builds Rudder.dmg from a built Rudder.app: the app, an Applications link and
# the window art in mac/dmg. dmgbuild writes the Finder layout directly (no
# AppleScript driving Finder), so it runs the same on a laptop and in CI.
# Signing and notarizing the DMG are the caller's job.
#
#   mac/scripts/make-dmg.sh <path/to/Rudder.app> <out.dmg>
set -euo pipefail

die() { printf 'make-dmg.sh: %s\n' "$*" >&2; exit 1; }

[[ $# -eq 2 ]] || die "usage: make-dmg.sh <Rudder.app> <out.dmg>"
[[ -d "$1" ]] || die "$1 is not an app bundle"
APP="$(cd "$1" && pwd)"
OUT_DMG="$2"
DMG_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../dmg" && pwd)"

command -v python3 >/dev/null || die "python3 is missing: brew install python"
python3 -c 'import sys; sys.exit(sys.version_info < (3, 10))' \
  || die "dmgbuild needs Python 3.10 or newer, $(python3 --version) is first on PATH: brew install python"

# dmgbuild, pinned, in a throwaway venv: nothing installed globally.
VENV="$(mktemp -d -t rudder-dmgbuild)"
trap 'rm -rf "$VENV"' EXIT
python3 -m venv "$VENV"
"$VENV/bin/pip" install --quiet --disable-pip-version-check dmgbuild==1.6.7

rm -f "$OUT_DMG"
"$VENV/bin/dmgbuild" -s "$DMG_DIR/settings.py" -D app="$APP" -D dmgdir="$DMG_DIR" Rudder "$OUT_DMG"
