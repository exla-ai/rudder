#!/bin/sh
set -eu
before=$(readlink /proc/1/ns/mnt)
mkdir /tmp/vfs-remote-payload
/bin/tar -xzf /tmp/vfs-remote-payload.tar.gz -C /tmp/vfs-remote-payload
chmod 755 /tmp/vfs-remote-payload/driver
/usr/local/bin/node --version
/tmp/vfs-remote-payload/driver --build-info
if ! test -c /dev/fuse; then mknod /dev/fuse c 10 229; fi
chown 1500:1500 /dev/fuse
chmod 600 /dev/fuse
test "$(stat -c '%u:%g:%a' /dev/fuse)" = "1500:1500:600"
mkdir -p /workspace
mkdir /workspace/vfs-capture-root /workspace/vfs-capture-user
chmod 700 /workspace/vfs-capture-root /workspace/vfs-capture-user
chown 1500:1500 /workspace/vfs-capture-user
timeout 360 /tmp/vfs-remote-payload/driver qualify /workspace/vfs-capture-root /tmp/vfs-remote-payload/helper/helper.mjs /usr/local/bin/node
timeout 360 /tmp/vfs-remote-payload/driver as-user /workspace/vfs-capture-user /tmp/vfs-remote-payload/helper/helper.mjs /usr/local/bin/node
test ! -e /workspace/vfs-capture-root
test ! -e /workspace/vfs-capture-user
after=$(readlink /proc/1/ns/mnt)
test "$before" = "$after"
printf '{"outer_initial_mount_namespace_unchanged":true,"device_owner":1500,"device_group":1500,"device_mode":"0600","owned_fixture_directories_absent":true,"setuid_helper_installed":false}\n'
