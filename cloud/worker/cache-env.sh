#!/usr/bin/env sh
# Source before starting either supervisor. Share package/download caches within
# this account's machine; build outputs and writable checkouts stay per agent.
if [ "${RUDDER_BUILD_CACHE:-1}" != "0" ]; then
  if [ "${RUDDER_HOME:-0}" = "1" ]; then
    rudder_cache_default="${RUDDER_HOME_VOLUME:-/workspace}/cache/build"
  else
    rudder_cache_default="${HOME}/.cache/rudder-build"
  fi
  RUDDER_CACHE_DIR="${RUDDER_CACHE_DIR:-$rudder_cache_default}"
  if mkdir -p "$RUDDER_CACHE_DIR" && [ -w "$RUDDER_CACHE_DIR" ]; then
    # All tools retain their own integrity, locking and cache-key semantics.
    # Honor explicit environment settings, including npm's uppercase variants.
    npm_config_cache="${npm_config_cache:-${NPM_CONFIG_CACHE:-$RUDDER_CACHE_DIR/npm}}"
    npm_config_store_dir="${npm_config_store_dir:-${NPM_CONFIG_STORE_DIR:-$RUDDER_CACHE_DIR/pnpm}}"
    UV_CACHE_DIR="${UV_CACHE_DIR:-$RUDDER_CACHE_DIR/uv}"
    export RUDDER_CACHE_DIR npm_config_cache npm_config_store_dir UV_CACHE_DIR
  else
    echo "rudder-worker: build cache directory is unavailable; using tool defaults" >&2
  fi
  unset rudder_cache_default
fi
