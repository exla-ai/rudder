#!/bin/sh
set -eu
export DEBIAN_FRONTEND=noninteractive
timeout 120 apt-get update -qq >/tmp/apt.log 2>&1
timeout 180 apt-get install -y -qq nodejs gcc libc6-dev busybox-static >>/tmp/apt.log 2>&1
mkdir -p /opt/runtime /tmp/execution-fixture
chown 1500:1500 /tmp/execution-fixture
chown 0:1500 /dev/fuse
chmod 660 /dev/fuse
gzip -dc /tmp/controller.gz >/tmp/controller
gzip -dc /tmp/native.gz >/opt/runtime/rudder-native
chmod 755 /tmp/controller /opt/runtime/rudder-native
timeout 180 /tmp/controller as-user /opt/runtime/rudder-native /tmp/execution-fixture
node -e 'console.log(JSON.stringify({node:process.version,cc:require("child_process").execFileSync("cc",["-dumpfullversion","-dumpversion"],{encoding:"utf8"}).trim()}))'
