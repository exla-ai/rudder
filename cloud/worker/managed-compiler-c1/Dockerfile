# syntax=docker/dockerfile:1
# C1 is a NEW compiler DATA OCI, never an A2/runtime/SDK execution grant.
ARG RUST_SOURCE_IMAGE=docker.io/library/rust:1-bookworm@sha256:9181b56c2a21ab1d5e73cffe04fc762426d30aa7fac3c91141dd033f49a7fc8b
ARG NODE_SOURCE_IMAGE=docker.io/library/node:22-bookworm-slim@sha256:4b150b48b4116e2a22f2c39cc27025fbf4f153e2078fd3b14d159fdb5c7279da
FROM ${NODE_SOURCE_IMAGE} AS node-input
FROM ${RUST_SOURCE_IMAGE} AS assemble
ARG RUST_SOURCE_IMAGE
ARG NODE_SOURCE_IMAGE
ARG TARGETARCH
ARG RUDDER_COMPILER_DEBIAN_SNAPSHOT=20261030T000000Z
USER root
COPY --from=node-input --chown=0:0 /usr/local/bin/node /compiler-input/node
COPY --chown=0:0 --chmod=0444 inputs/ /compiler-input/policy/
COPY --chown=0:0 --chmod=0555 compiler_artifact.py compiler_c1.py /compiler-input/
COPY --chown=0:0 --chmod=0444 workloads/ /compiler-input/workloads/
RUN test "$TARGETARCH" = amd64 \
 && test "$RUDDER_COMPILER_DEBIAN_SNAPSHOT" = 20261030T000000Z \
 && rm -f /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources \
 && rm -rf /var/lib/apt/lists/* \
 && printf '%s\n' \
      "deb [signed-by=/usr/share/keyrings/debian-archive-keyring.gpg check-valid-until=no] https://snapshot.debian.org/archive/debian/$RUDDER_COMPILER_DEBIAN_SNAPSHOT bookworm main" \
      "deb [signed-by=/usr/share/keyrings/debian-archive-keyring.gpg check-valid-until=no] https://snapshot.debian.org/archive/debian-security/$RUDDER_COMPILER_DEBIAN_SNAPSHOT bookworm-security main" \
      > /etc/apt/sources.list \
 && env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin DEBIAN_FRONTEND=noninteractive apt-get update \
 && env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      python3 util-linux libc-bin libzstd1 gcc g++ binutils libc6-dev linux-libc-dev make pkg-config libssl-dev zlib1g-dev libsqlite3-dev ca-certificates \
 && install -d -o 0 -g 0 -m 0700 /workspace/compiler-artifact-scratch /out \
 && env -i PATH=/usr/bin:/bin LANG=C LC_ALL=C /usr/bin/python3 -I -B /compiler-input/compiler_artifact.py record-prerequisites \
      --policy /compiler-input/policy --snapshot "$RUDDER_COMPILER_DEBIAN_SNAPSHOT" --output /compiler-input/prerequisites.json \
 && env -i PATH=/usr/bin:/bin LANG=C LC_ALL=C /usr/bin/python3 -I -B /compiler-input/compiler_c1.py build \
      --output /out/rudder-compiler-c1 --scratch /workspace/compiler-artifact-scratch \
      --policy /compiler-input/policy --node /compiler-input/node --prerequisites /compiler-input/prerequisites.json \
      --normalize-pinned-rust --rust-source-image "$RUST_SOURCE_IMAGE" --node-source-image "$NODE_SOURCE_IMAGE" \
 && env -i PATH=/usr/bin:/bin /usr/bin/python3 -I -B /compiler-input/compiler_c1.py verify --root /out/rudder-compiler-c1

# Optional explicit functional component target. It is NOT part of the artifact
# constructor receipt or production SDK policy. No network in the RUN phase.
FROM assemble AS compiler-workloads
RUN --network=none env -i PATH=/usr/bin:/bin /usr/bin/python3 -I -B /compiler-input/workloads/qualify_c1.py \
      --artifact /out/rudder-compiler-c1 --scratch /workspace/compiler-c1-workload

FROM scratch AS compiler-artifact
COPY --from=assemble --chown=0:0 /out/ /
LABEL ai.exla.rudder.artifact="compiler-c1-data-v1" \
      ai.exla.rudder.profile="rudder-linux-rust-cxx-typescript-c1" \
      ai.exla.rudder.execution-qualified="false"
