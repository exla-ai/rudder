#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$repo_root"

if [[ $(uname -s) != Linux || $(id -ru) != 1500 || $(id -rg) != 1500 || $(id -u) != 1500 || $(id -g) != 1500 ]]; then
  echo "Run this test in a Linux development container as UID/GID 1500 (real and effective), with writable /workspace and enabled user namespaces." >&2
  exit 1
fi
for program in cargo cc; do
  if ! command -v "$program" >/dev/null; then
    echo "Install $program first; the local SDK fixture also needs static libc development libraries for cc -static." >&2
    exit 1
  fi
done

# Match the canonical workspace test graph and its existing development cache.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
export CARGO_TARGET_DIR="$repo_root/target"
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0
exec cargo test --workspace --locked --features rudder-vcs-bridge/cli --lib -j 2 -- \
  --ignored --exact --nocapture --test-threads=1 \
  root_storage_bootstrap::runtime_image::compiler_sdk::helper::local_tests::local_uid1500_runs_actual_fixed_namespace_carrier_with_stand_in_payload \
  root_storage_bootstrap::runtime_image::compiler_sdk::helper::local_tests::local_uid1500_runs_actual_selected_namespace_carrier_with_retained_source_and_output
