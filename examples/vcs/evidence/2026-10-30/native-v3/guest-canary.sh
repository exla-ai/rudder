#!/bin/sh
# ONLY the exact owned disposable VM asserted by qualify-native-vcs.mjs.
set -eu
export DEBIAN_FRONTEND=noninteractive
timeout 120 apt-get update -qq >/tmp/native-vcs-apt.log 2>&1
timeout 180 apt-get install -y -qq git busybox-static >>/tmp/native-vcs-apt.log 2>&1
mkdir -p /opt/runtime/vcs-endpoints /tmp/native-vcs-data
chown 1500:1500 /opt/runtime/vcs-endpoints /tmp/native-vcs-data
chmod 700 /opt/runtime/vcs-endpoints /tmp/native-vcs-data
chown 0:1500 /dev/fuse
chmod 660 /dev/fuse
gzip -dc /tmp/controller.gz >/opt/runtime/qualify-vcs
gzip -dc /tmp/jj.gz >/opt/runtime/rudder-jj
gzip -dc /tmp/native.gz >/opt/runtime/rudder-native
chmod 755 /opt/runtime/qualify-vcs /opt/runtime/rudder-jj /opt/runtime/rudder-native
printf 'kernel='; uname -r
git --version
sha256sum /opt/runtime/qualify-vcs /opt/runtime/rudder-jj /opt/runtime/rudder-native
timeout 420 /opt/runtime/qualify-vcs native-as-user /opt/runtime/rudder-native /opt/runtime/rudder-jj /tmp/native-vcs-data
