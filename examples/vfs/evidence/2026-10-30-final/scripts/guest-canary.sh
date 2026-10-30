#!/bin/sh
set -eu
before=$(readlink /proc/1/ns/mnt)
mkdir /tmp/vfs-root-only
chmod 700 /tmp/vfs-root-only
printf 'disposable canary root data' >/tmp/vfs-root-only/secret
chmod 600 /tmp/vfs-root-only/secret
mkdir /tmp/vfs-device-denied /tmp/vfs-rootless-fixture /tmp/vfs-fixture
chown 1500:1500 /tmp/vfs-device-denied /tmp/vfs-rootless-fixture
gzip -dc /tmp/vfs-driver.gz >/tmp/vfs-driver
chmod 755 /tmp/vfs-driver
timeout 30 /tmp/vfs-driver missing-device /tmp/vfs-device-denied
# Provision only this disposable VM; no setuid binary, broker or host edit.
chown 0:1500 /dev/fuse
chmod 660 /dev/fuse
timeout 180 /tmp/vfs-driver qualify /tmp/vfs-fixture
timeout 180 /tmp/vfs-driver as-user /tmp/vfs-rootless-fixture
after=$(readlink /proc/1/ns/mnt)
test "$before" = "$after"
printf '{"initial_mount_namespace_unchanged":true,"device_group":1500,"device_mode":"0660","setuid_helper_installed":false}
'
