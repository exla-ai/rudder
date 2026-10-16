#!/usr/bin/env sh
set -eu

# The image bakes whatever Claude Code was current on its build day, and a
# machine boots from the image, so without this a worker ran a stale binary
# for the life of the image tag (the auto-updater's "Updated to latest" only
# ever reached the NEXT launch, and a fresh boot had no next launch to reach).
# Bounded: a fresh image is a two-second no-op, a stale one downloads once.
# Skipped entirely with RUDDER_WORKER_SKIP_AGENT_UPDATE=1.
if [ "${RUDDER_WORKER_SKIP_AGENT_UPDATE:-0}" != "1" ] && command -v claude >/dev/null 2>&1; then
  echo "rudder-worker: checking for a Claude Code update ($(claude --version 2>/dev/null || echo unknown))" >&2
  if ! timeout 90 claude update >&2; then
    echo "rudder-worker: claude update did not complete; continuing with $(claude --version 2>/dev/null || echo unknown)" >&2
  fi
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
  ) >/dev/null 2>&1 &
fi

# A home machine (one per account, every repo on it) runs its own supervisor.
if [ "${RUDDER_HOME:-0}" = "1" ]; then
  exec node /opt/rudder-worker/home.mjs "$@"
fi

exec node /opt/rudder-worker/supervisor.mjs "$@"
