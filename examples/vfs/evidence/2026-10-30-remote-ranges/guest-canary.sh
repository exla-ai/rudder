#!/bin/sh
set -eu
before=$(readlink /proc/1/ns/mnt)
mkdir /tmp/vfs-remote-payload
/bin/tar -xzf /tmp/vfs-remote-payload.tar.gz -C /tmp/vfs-remote-payload
chmod 755 /tmp/vfs-remote-payload/driver
/usr/local/bin/node --version
/tmp/vfs-remote-payload/driver --build-info
if ! test -c /dev/fuse; then mknod /dev/fuse c 10 229; fi
chown 0:1500 /dev/fuse
chmod 660 /dev/fuse
mkdir /tmp/vfs-remote-root /tmp/vfs-remote-user
chmod 700 /tmp/vfs-remote-root /tmp/vfs-remote-user
chown 1500:1500 /tmp/vfs-remote-user
timeout 240 /tmp/vfs-remote-payload/driver qualify /tmp/vfs-remote-root /tmp/vfs-remote-payload/helper/helper.mjs /usr/local/bin/node
timeout 240 /tmp/vfs-remote-payload/driver as-user /tmp/vfs-remote-user /tmp/vfs-remote-payload/helper/helper.mjs /usr/local/bin/node
test ! -e /tmp/vfs-remote-root
test ! -e /tmp/vfs-remote-user
after=$(readlink /proc/1/ns/mnt)
test "$before" = "$after"
printf '{"outer_initial_mount_namespace_unchanged":true,"device_group":1500,"device_mode":"0660","owned_fixture_directories_absent":true,"setuid_helper_installed":false}\n'
