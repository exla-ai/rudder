#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$repo_root"
mode=${1:-all}
case "$mode" in
  all|--stand-in|--real|--preflight-only) ;;
  *) echo "Usage: $0 [--real|--stand-in|--preflight-only]" >&2; exit 2 ;;
esac

if [[ $(uname -s) != Linux || $(id -ru) != 1500 || $(id -rg) != 1500 || $(id -u) != 1500 || $(id -g) != 1500 ]]; then
  echo "LOCAL_SDK_PRECONDITION: use a Linux development container as real/effective UID/GID1500, with writable /workspace and enabled user namespaces. No SDK execution tested." >&2
  exit 2
fi
if [[ "$mode" != --stand-in ]]; then
  if [[ ! -x /usr/bin/python3 ]]; then
    echo "LOCAL_SDK_PRECONDITION: install pinned /usr/bin/python3.11 and its stdlib. No SDK execution tested." >&2
    exit 2
  fi
  /usr/bin/python3 -I -B native/src/sdk_local_e2e/prepare.py --preflight
fi
if [[ "$mode" == --preflight-only ]]; then exit 0; fi
if [[ "$mode" != --real ]] && ! command -v cc >/dev/null; then
  echo "LOCAL_SDK_PRECONDITION: install cc and static libc for the separately labelled stand-in carrier probes." >&2
  exit 2
fi
if [[ ! -x /workspace/rudder-build/rcargo ]]; then
  echo "LOCAL_SDK_PRECONDITION: provide the shared queued /workspace/rudder-build/rcargo wrapper. No unqueued Cargo fallback." >&2
  exit 2
fi

# Reuse the shared wrapper's compiler profile and serialized target cache.
# Never build the workspace or call plain Cargo from this harness.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
filter=helper::local_tests
if [[ "$mode" == --real ]]; then filter=helper::local_tests::real_sdk; fi
if [[ "$mode" == --stand-in ]]; then filter=helper::local_tests::local_uid1500; fi
exec /workspace/rudder-build/rcargo test -p rudder-native --locked --lib "$filter" -- \
  --ignored --nocapture --test-threads=1
