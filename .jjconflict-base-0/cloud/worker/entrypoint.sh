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

. /opt/rudder-worker/cache-env.sh

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
  # Codex refreshes in the background: it is not the default cloud backend, so
  # the first session need not wait on npm. A plain `npm install -g` removes
  # the old package before writing the new one, so `codex` did not exist for
  # the whole install and anything starting then failed (ENOENT). Install
  # beside it and swap the directory in at the end; skip when already current.
  (
    set +e
    latest=$(timeout 30 npm view @openai/codex version 2>/dev/null)
    current=$(codex --version 2>/dev/null | awk '{print $NF}')
    if [ -n "$latest" ] && [ "$latest" != "$current" ]; then
      next="$HOME/.codex-next"
      global="${NPM_CONFIG_PREFIX:-$HOME/.npm-global}/lib/node_modules/@openai"
      rm -rf "$next"
      if timeout 300 npm install -g --prefix "$next" "@openai/codex@$latest" >/dev/null 2>&1 \
        && [ -d "$next/lib/node_modules/@openai/codex" ]; then
        rm -rf "$global/codex.old"
        mv "$global/codex" "$global/codex.old" && mv "$next/lib/node_modules/@openai/codex" "$global/codex"
        rm -rf "$global/codex.old" "$next"
      fi
    fi
  ) </dev/null >/dev/null 2>&1 &
fi

# A home machine (one per account, every repo on it) runs its own supervisor.
if [ "${RUDDER_HOME:-0}" = "1" ]; then
  exec node /opt/rudder-worker/home.mjs "$@"
fi

exec node /opt/rudder-worker/supervisor.mjs "$@"
