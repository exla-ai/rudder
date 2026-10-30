#!/bin/sh
# ONLY on the exact owned disposable VM asserted by qualify-mounted-rpc.mjs.
set -eu
export DEBIAN_FRONTEND=noninteractive
timeout 120 apt-get update -qq >/tmp/vcs-rpc-apt.log 2>&1
timeout 180 apt-get install -y -qq git busybox-static >>/tmp/vcs-rpc-apt.log 2>&1
mkdir -p /opt/runtime/vcs-endpoints /tmp/vcs-data
chown 1500:1500 /opt/runtime/vcs-endpoints /tmp/vcs-data
chmod 700 /opt/runtime/vcs-endpoints /tmp/vcs-data
chown 0:1500 /dev/fuse
chmod 660 /dev/fuse
gzip -dc /tmp/controller.gz >/opt/runtime/qualify-vcs
gzip -dc /tmp/jj.gz >/opt/runtime/rudder-jj
chmod 755 /opt/runtime/qualify-vcs /opt/runtime/rudder-jj
printf 'kernel='; uname -r
git --version
sha256sum /opt/runtime/qualify-vcs /opt/runtime/rudder-jj
timeout 480 /opt/runtime/qualify-vcs as-user /opt/runtime/rudder-jj /tmp/vcs-data
