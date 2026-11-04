#!/bin/sh
# Image-owned boot code. No profiles, updates, Node, credentials or user paths
# execute until setpriv has permanently dropped the initial root identity.
set -eu

managed=${RUDDER_WORKER_MANAGED_EXECUTION:-0}
case "$managed" in
  0|1) ;;
  *) echo "rudder-worker: RUDDER_WORKER_MANAGED_EXECUTION must be 0 or 1" >&2; exit 64 ;;
esac
RUDDER_WORKER_FUSE_READY=0
export RUDDER_WORKER_FUSE_READY

verify_only=0
if [ "${1:-}" = "--verify-managed-execution" ]; then
  [ "$#" = "1" ] || exit 64
  verify_only=1
fi

fuse_shape() {
  [ ! -L /dev/fuse ] && [ -c /dev/fuse ] || return 1
  shape=$(/usr/bin/stat -c '%t:%T:%h' -- /dev/fuse 2>/dev/null) || return 1
  [ "$shape" = "a:e5:1" ]
}
fuse_grant() {
  fuse_shape || return 1
  grant=$(/usr/bin/stat -c '%u:%g:%a' -- /dev/fuse 2>/dev/null) || return 1
  [ "$grant" = "1500:1500:600" ] && [ -r /dev/fuse ] && [ -w /dev/fuse ]
}
provision_fuse() {
  fuse_shape || return 1
  before=$(/usr/bin/stat -c '%d:%i' -- /dev/fuse) || return 1
  owner=$(/usr/bin/stat -c '%u' -- /dev/fuse) || return 1
  case "$owner" in 0|1500) ;; *) return 1 ;; esac
  /usr/bin/chown --no-dereference 1500:1500 -- /dev/fuse \
    && /usr/bin/chmod 0600 -- /dev/fuse \
    && fuse_grant \
    && [ "$before" = "$(/usr/bin/stat -c '%d:%i' -- /dev/fuse)" ]
}
managed_identity() {
  [ "$(/usr/bin/id -u)" = "1500" ] && [ "$(/usr/bin/id -ru)" = "1500" ] \
    && [ "$(/usr/bin/id -g)" = "1500" ] && [ "$(/usr/bin/id -rg)" = "1500" ] \
    && [ "$(/usr/bin/id -G)" = "1500" ]
}
workspace_access() {
  [ ! -L /workspace ] && [ -d /workspace ] \
    && [ "$(/usr/bin/stat -c '%u' -- /workspace 2>/dev/null)" = "1500" ] \
    && [ -w /workspace ] && [ -x /workspace ]
}
privilege_floor() {
  /usr/bin/awk '
    /^Uid:/ { ids=($2==1500&&$3==1500&&$4==1500&&$5==1500) }
    /^Gid:/ { gids=($2==1500&&$3==1500&&$4==1500&&$5==1500) }
    /^NoNewPrivs:/ { nnp=($2==1) }
    /^Cap(Eff|Prm|Inh|Amb):/ { if ($2 !~ /^0+$/) caps=1; count++ }
    END { exit !(ids&&gids&&nnp&&!caps&&count==4) }
  ' /proc/self/status
}
workspace_mount_owner() {
  # Fly init assigns a volume mount's top inode to the image USER. This image
  # starts root only for boot; restore the prior UID1500 mount ownership without
  # touching contents, child owners/modes or a user-selected path.
  [ ! -L /workspace ] && [ -d /workspace ] || return 1
  workspace_owner=$(/usr/bin/stat -c '%u:%g' -- /workspace) || return 1
  case "$workspace_owner" in
    1500:1500) return 0 ;;
    0:*) ;;
    *) return 1 ;;
  esac
  /usr/bin/awk '$5=="/workspace" {found=1} END {exit !found}' /proc/self/mountinfo || return 1
  workspace_inode=$(/usr/bin/stat -c '%d:%i' -- /workspace) || return 1
  workspace_mode=$(/usr/bin/stat -c '%a' -- /workspace) || return 1
  /usr/bin/chown --no-dereference 1500:1500 -- /workspace || return 1
  # chown may clear special mode bits. Restore exactly the original mode on
  # this one directory, then verify its identity rather than walking children.
  if [ "$(/usr/bin/stat -c '%a' -- /workspace)" != "$workspace_mode" ]; then
    /usr/bin/chmod "$workspace_mode" -- /workspace || return 1
  fi
  [ ! -L /workspace ] && [ -d /workspace ] \
    && [ "$workspace_inode" = "$(/usr/bin/stat -c '%d:%i' -- /workspace)" ] \
    && [ "$(/usr/bin/stat -c '%u:%g:%a' -- /workspace)" = "1500:1500:$workspace_mode" ]
}

if [ "$verify_only" = "1" ]; then
  if [ "$managed" != "1" ] || ! managed_identity || ! privilege_floor || ! fuse_grant || ! workspace_access; then
    echo "rudder-worker: managed entrypoint requires its bootstrap, pre-granted FUSE and owned writable /workspace" >&2
    exit 78
  fi
  exit 0
fi

uid=$(/usr/bin/id -u)
if [ "$uid" = "0" ]; then
  # All external programs in this root phase use image-owned absolute paths.
  # User-controlled PATH, shell profiles and volume scripts are never consulted.
  if ! workspace_mount_owner && [ "$managed" = "1" ]; then
    echo "rudder-worker: managed execution requires its assigned /workspace directory or mounted volume" >&2
    exit 78
  fi
  if provision_fuse; then RUDDER_WORKER_FUSE_READY=1; fi
  if [ "$managed" = "1" ] && [ "$RUDDER_WORKER_FUSE_READY" != "1" ]; then
    echo "rudder-worker: managed execution requires a safe /dev/fuse character device 10:229" >&2
    exit 78
  fi
  HOME=/home/rudder USER=rudder LOGNAME=rudder
  export HOME USER LOGNAME RUDDER_WORKER_FUSE_READY
  exec /usr/bin/setpriv --reuid=1500 --regid=1500 --clear-groups \
    --inh-caps=-all --ambient-caps=-all --bounding-set=-all --no-new-privs \
    -- /usr/local/bin/rudder-worker "$@"
fi

# Direct non-root/BYOVM entrypoints retain their user, home and existing legacy
# behavior. An explicitly requested managed profile must already have a safe
# device/identity; an unprivileged launcher never tries to provision the host.
if [ "$managed" = "1" ]; then
  if ! managed_identity || ! fuse_grant || ! workspace_access; then
    echo "rudder-worker: non-root managed execution requires UID/GID 1500, pre-granted FUSE and owned writable /workspace" >&2
    exit 78
  fi
  RUDDER_WORKER_FUSE_READY=1
  export RUDDER_WORKER_FUSE_READY
  exec /usr/bin/setpriv --reuid=1500 --regid=1500 --keep-groups --inh-caps=-all --ambient-caps=-all --no-new-privs \
    -- /usr/local/bin/rudder-worker "$@"
fi
exec /usr/local/bin/rudder-worker "$@"
