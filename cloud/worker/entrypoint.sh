#!/bin/sh
set -eu

# Direct/root invocations enter the same tiny boot gate before sourcing any
# account cache/profile path or starting updates. Non-root BYOVM keeps its path.
if [ "$(/usr/bin/id -u)" = "0" ]; then
  exec /usr/local/bin/rudder-worker-root-bootstrap "$@"
fi
case "${RUDDER_WORKER_MANAGED_EXECUTION:-0}" in
  0|1) ;;
  *) echo "rudder-worker: RUDDER_WORKER_MANAGED_EXECUTION must be 0 or 1" >&2; exit 64 ;;
esac
if [ "${RUDDER_WORKER_MANAGED_EXECUTION:-0}" = "1" ]; then
  /usr/local/bin/rudder-worker-root-bootstrap --verify-managed-execution
  RUDDER_WORKER_FUSE_READY=1
  export RUDDER_WORKER_FUSE_READY
fi

# Only the supervisor receives its Cloud credential. Keep it out of bootstrap
# helpers, background CLI updates and their descendants.
unset rudder_boot_worker_token rudder_boot_builder_cache_token rudder_boot_builder_token_hash
rudder_boot_worker_token=${RUDDER_WORKER_TOKEN-}
rudder_boot_builder_cache_token=${RUDDER_BUILDER_CACHE_TOKEN-}
rudder_boot_builder_token_hash=${RUDDER_BUILDER_TOKEN_SHA256-}
unset RUDDER_WORKER_TOKEN RUDDER_BUILDER_CACHE_TOKEN RUDDER_BUILDER_TOKEN_SHA256
. /opt/rudder-worker/cache-env.sh

# A builder in a home's burst build pool runs builds and nothing else: no
# agent CLIs to update, no supervisor.
if [ "${RUDDER_BUILDER:-0}" = "1" ]; then
  export RUDDER_BUILDER_CACHE_TOKEN="$rudder_boot_builder_cache_token" RUDDER_BUILDER_TOKEN_SHA256="$rudder_boot_builder_token_hash"
  exec node /opt/rudder-worker/builder.mjs "$@"
fi

# The image bakes whatever Claude Code was current on its build day, and a
# machine boots from the image, so without this a worker ran a stale binary
# for the life of the image tag (the auto-updater's "Updated to latest" only
# ever reached the NEXT launch, and a fresh boot had no next launch to reach).
# Check in the background: image-provided CLIs can start immediately, even when
# an update server is unavailable. Native Claude updates leave a running binary
# intact and affect subsequent launches. Neither updater reads terminal input.
# Skipped entirely with RUDDER_WORKER_SKIP_AGENT_UPDATE=1.
if [ "${RUDDER_WORKER_SKIP_AGENT_UPDATE:-0}" != "1" ]; then
  (
    if command -v claude >/dev/null 2>&1; then
      echo "rudder-worker: checking for a Claude Code update" >&2
      if ! timeout 90 claude update >&2; then
        echo "rudder-worker: Claude Code update did not complete; keeping the installed version" >&2
      fi
    fi
  ) </dev/null >&2 &
  # Codex stays at the version whose "/" commands Rudder lists
  # (RUDDER_CODEX_VERSION, set by the image), so this only puts it back when
  # something else changed it. It runs in the background: Codex is not the
  # default cloud backend. A plain `npm install -g` removes the old package
  # before writing the new one, so `codex` did not exist for the whole install
  # and anything starting then failed (ENOENT). Install beside it and swap the
  # directory in at the end. A failure is logged: the "/" list refuses any
  # other Codex version.
  (
    set +e
    want="${RUDDER_CODEX_VERSION:?the image sets RUDDER_CODEX_VERSION}"
    current=$(codex --version 2>/dev/null | awk '{print $NF}')
    if [ "$want" != "$current" ]; then
      echo "rudder-worker: putting Codex back to $want (found ${current:-none})" >&2
      next="$HOME/.codex-next"
      global="${NPM_CONFIG_PREFIX:-$HOME/.npm-global}/lib/node_modules/@openai"
      rm -rf "$next"
      if timeout 300 npm install -g --prefix "$next" "@openai/codex@$want" >/dev/null 2>&1 \
        && [ -d "$next/lib/node_modules/@openai/codex" ]; then
        rm -rf "$global/codex.old"
        mv "$global/codex" "$global/codex.old" && mv "$next/lib/node_modules/@openai/codex" "$global/codex"
        rm -rf "$global/codex.old" "$next"
      else
        echo "rudder-worker: Codex $want did not install; Codex is still ${current:-missing}" >&2
      fi
    fi
  ) </dev/null >&2 &
fi

# A home machine (one per account, every repo on it) runs its own supervisor.
[ -z "$rudder_boot_worker_token" ] || export RUDDER_WORKER_TOKEN="$rudder_boot_worker_token"
if [ "${RUDDER_HOME:-0}" = "1" ]; then
  exec node /opt/rudder-worker/home.mjs "$@"
fi

exec node /opt/rudder-worker/supervisor.mjs "$@"
