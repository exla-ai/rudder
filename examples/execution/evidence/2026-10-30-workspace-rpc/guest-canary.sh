#!/bin/sh
# Run ONLY on the explicitly owned disposable VM selected by the controller.
set -eu
export DEBIAN_FRONTEND=noninteractive
timeout 120 apt-get update -qq >/tmp/execution-rpc-apt.log 2>&1
timeout 180 apt-get install -y -qq nodejs busybox-static >>/tmp/execution-rpc-apt.log 2>&1
mkdir -p /opt/runtime /tmp/execution-rpc
chown 1500:1500 /tmp/execution-rpc
chown 0:1500 /dev/fuse
chmod 660 /dev/fuse
gzip -dc /tmp/controller.gz >/tmp/controller
gzip -dc /tmp/native.gz >/opt/runtime/rudder-native
chmod 755 /tmp/controller /opt/runtime/rudder-native
timeout 180 /tmp/controller rpc-as-user /opt/runtime/rudder-native /tmp/execution-rpc
node -e 'console.log(JSON.stringify({node:process.version,kernel:require("child_process").execFileSync("uname",["-r"],{encoding:"utf8"}).trim()}))'
