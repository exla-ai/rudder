#!/bin/sh
# Executed only through the nonce/identity-checked private Fly fixture runner.
set -eu
[ "$(/usr/bin/id -u)" = "0" ]
[ -n "${RUDDER_BOOTSTRAP_QUALIFICATION_NONCE:-}" ]
[ -n "${RUDDER_BOOTSTRAP_OWNED_MACHINE:-}" ]
[ "$(cat /run/rudder-worker-bootstrap-qualification)" = "$RUDDER_BOOTSTRAP_QUALIFICATION_NONCE:$RUDDER_BOOTSTRAP_OWNED_MACHINE" ]
export DEBIAN_FRONTEND=noninteractive
phase=packages
failed() {
  code=$?
  if [ "$code" != 0 ]; then
    echo "worker-bootstrap fixture failed phase=$phase exit=$code" >&2
    if [ -x /workspace/git-src/git ]; then
      echo GIT_BINARY_PRODUCED >&2
      timeout 5 /workspace/git-src/git --version >&2 || :
    else echo GIT_BINARY_NOT_PRODUCED >&2; fi
    tail -n 35 /workspace/git-build.log /workspace/bootstrap-apt.log 2>/dev/null || :
  fi
}
trap failed EXIT
printf 'BOOTSTRAP_PHASE:packages\n'
timeout 120 apt-get update -qq >/workspace/bootstrap-apt.log 2>&1
timeout 240 apt-get install -y --no-install-recommends -qq nodejs git ca-certificates curl xz-utils build-essential gettext gettext-base perl cargo rustc util-linux passwd libcurl4-openssl-dev libexpat1-dev libssl-dev zlib1g-dev >>/workspace/bootstrap-apt.log 2>&1
mkdir -p /workspace/bootstrap-tools /opt/runtime /opt/rudder-worker
printf 'BOOTSTRAP_PHASE:unpack-verified-fixtures\n'
phase=unpack-fixtures
tar -xzf /workspace/boot-artifacts/boot-fixture.tar.gz -C /workspace/bootstrap-tools
cd /workspace/bootstrap-tools
sha256sum -c SHA256SUMS
/usr/bin/install -m 0755 root-bootstrap.sh /usr/local/bin/rudder-worker-root-bootstrap
/usr/bin/install -m 0755 entrypoint.sh /usr/local/bin/rudder-worker
/usr/bin/install -m 0644 cache-env.sh /opt/rudder-worker/cache-env.sh
/usr/bin/install -m 0644 probe.mjs /opt/rudder-worker/home.mjs
/usr/bin/install -m 0644 probe.mjs /opt/rudder-worker/supervisor.mjs
/usr/bin/install -m 0644 update-probe.mjs /opt/rudder-worker/update-probe.mjs
gzip -dc /workspace/boot-artifacts/native.gz >/opt/runtime/rudder-native
gzip -dc /workspace/boot-artifacts/controller.gz >/workspace/controller
chmod 0755 /opt/runtime/rudder-native /workspace/controller
printf 'BOOTSTRAP_PHASE:build-official-pinned-git\n'
phase=git-build
mkdir -p /workspace/git-src
tar -xJf git-2.56.0.tar.xz -C /workspace/git-src --strip-components=1
timeout 480 make -C /workspace/git-src -j2 prefix=/usr/local NO_TCLTK=YesPlease all >/workspace/git-build.log 2>&1
timeout 90 make -C /workspace/git-src prefix=/usr/local NO_TCLTK=YesPlease install >>/workspace/git-build.log 2>&1
[ "$(/usr/local/bin/git --version)" = 'git version 2.56.0' ]
mkdir -p /workspace/jj-unpack
tar -xzf jj-v0.40.0-x86_64-unknown-linux-musl.tar.gz -C /workspace/jj-unpack
/usr/bin/install -m 0755 /workspace/jj-unpack/jj /usr/local/bin/jj
/usr/local/bin/jj --version
node -e 'console.log(JSON.stringify({kind:"worker-bootstrap-tools",baseGit:require("child_process").execFileSync("/usr/bin/git",["--version"],{encoding:"utf8"}).trim(),git:require("child_process").execFileSync("/usr/local/bin/git",["--version"],{encoding:"utf8"}).trim(),jj:require("child_process").execFileSync("/usr/local/bin/jj",["--version"],{encoding:"utf8"}).trim(),gitSha256:require("crypto").createHash("sha256").update(require("fs").readFileSync("/usr/local/bin/git")).digest("hex"),kernel:require("child_process").execFileSync("uname",["-r"],{encoding:"utf8"}).trim()}))'
printf 'BOOTSTRAP_PHASE:actual-device-and-privilege-matrix\n'
phase=bootstrap-matrix
cd /
timeout 300 /usr/bin/node /workspace/bootstrap-tools/matrix.mjs
printf 'BOOTSTRAP_PHASE:complete\n'
