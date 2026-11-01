# syntax=docker/dockerfile:1
ARG WORKER_NODE_IMAGE=node:22-bookworm-slim
FROM rust:1-bookworm AS native-builder
WORKDIR /src
COPY native ./native
COPY engine ./engine
COPY vcs-bridge ./vcs-bridge
# The binary embeds assets (the notifier icon) with include_bytes!.
COPY assets ./assets
COPY Cargo.toml Cargo.lock ./
# Reuse external dependencies, but rebuild workspace crates from these exact
# inputs. Restored source mtimes cannot validate an old cached local binary.
# Copy outputs outside the cache mount before this RUN finishes.
RUN --mount=type=cache,id=rudder-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=rudder-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=rudder-native-release,target=/src/target,sharing=locked \
    cargo clean --locked --release -p rudder-engine -p rudder-native -p rudder-vcs-bridge \
  && cargo build --release --locked --manifest-path Cargo.toml \
       -p rudder-native -p rudder-vcs-bridge --features rudder-vcs-bridge/cli \
       --bin rudder-native --bin rudder-jj --bin rudder-root-storage --bin rudder-machine-root --bin rudder-image-preseed \
  && cp target/release/rudder-native /rudder-native \
  && cp target/release/rudder-jj /rudder-jj \
  && cp target/release/rudder-root-storage /rudder-root-storage \
  && cp target/release/rudder-machine-root /rudder-machine-root \
  && cp target/release/rudder-image-preseed /rudder-image-preseed

FROM debian:bookworm-slim AS git-builder
# Git 2.41 introduced fetch --porcelain, required by pinned jj0.40. Build the
# current upstream release against this same Debian ABI; no foreign apt suites.
ARG GIT_VERSION=2.56.0
ARG GIT_SHA256=26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl xz-utils build-essential gettext perl cargo rustc \
    libcurl4-openssl-dev libexpat1-dev libssl-dev zlib1g-dev \
  && rm -rf /var/lib/apt/lists/*
RUN set -eu; \
    curl --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 180 --retry 2 -fsSL "https://www.kernel.org/pub/software/scm/git/git-${GIT_VERSION}.tar.xz" -o /git.tar.xz; \
    printf '%s  %s\n' "$GIT_SHA256" /git.tar.xz | sha256sum -c -; \
    mkdir /git-src; tar -xJf /git.tar.xz -C /git-src --strip-components=1; \
    make -C /git-src -j"$(nproc)" prefix=/usr/local NO_TCLTK=YesPlease all; \
    make -C /git-src prefix=/usr/local NO_TCLTK=YesPlease DESTDIR=/git-runtime install; \
    /git-runtime/usr/local/bin/git --version

FROM ${WORKER_NODE_IMAGE} AS worker-tools

ENV RUDDER_CLOUD_WORKER=1 \
    RUDDER_DISABLE_AUTO_UPDATE=1 \
    RUDDER_DISABLE_UPDATE_CHECK=1 \
    RUDDER_DISABLE_ONBOARD_INSTALL=1

# A new project on the cloud machine should build without an agent first
# installing tools it cannot install (no root): the everyday CLI tools, Python
# with pip/venv, and native build deps are here; Go and Rust go onto the volume
# on first boot (cloud/worker/home.mjs), bun/uv/pnpm below.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates curl wget git git-lfs openssh-client tar gzip zip unzip xz-utils bzip2 zsh \
    python3 python3-pip python3-venv python3-dev build-essential cmake pkg-config libssl-dev \
    jq ripgrep fd-find tree file less lsof procps htop tmux nano vim-tiny \
    sqlite3 postgresql-client redis-tools util-linux gettext-base libcurl4 libexpat1 zlib1g \
  && ln -s /usr/bin/fdfind /usr/local/bin/fd \
  && rm -rf /var/lib/apt/lists/*

# Debian marks its Python as externally managed; agents run `pip install` as a
# non-root user on a disposable machine, so let it install into ~/.local.
ENV PIP_BREAK_SYSTEM_PACKAGES=1

# bun and uv: single static binaries, installed system-wide.
RUN set -eux; \
    curl -fsSL https://bun.sh/install | BUN_INSTALL=/usr/local bash; \
    bun --version; \
    curl -LsSf https://astral.sh/uv/install.sh | env UV_INSTALL_DIR=/usr/local/bin INSTALLER_NO_MODIFY_PATH=1 sh; \
    uv --version

# GitHub CLI: clone-mode workspaces wire git credentials via `gh auth setup-git`
# using the vault-delivered ~/.config/gh, and agents get `gh pr create`.
RUN set -eux; \
    curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
      -o /usr/share/keyrings/githubcli-archive-keyring.gpg; \
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list; \
    apt-get update; \
    apt-get install -y --no-install-recommends gh; \
    rm -rf /var/lib/apt/lists/*

# Tailscale: the home machine joins the user's tailnet so dev servers agents
# start are reachable (as HTTPS previews) from the laptop and the phone.
# tailscaled runs in userspace-networking mode as the non-root user.
RUN set -eux; \
    curl -fsSL https://pkgs.tailscale.com/stable/debian/bookworm.noarmor.gpg \
      -o /usr/share/keyrings/tailscale-archive-keyring.gpg; \
    curl -fsSL https://pkgs.tailscale.com/stable/debian/bookworm.tailscale-keyring.list \
      -o /etc/apt/sources.list.d/tailscale.list; \
    apt-get update; \
    apt-get install -y --no-install-recommends tailscale; \
    rm -rf /var/lib/apt/lists/*; \
    tailscale version

# Agent CLIs are installed later, as the non-root user, into locations that
# user can write: Claude Code through its native installer (~/.local/bin, which
# `claude update` and its auto-updater can replace in place), the npm ones
# under an npm prefix in the user's home. The image used to `npm install -g`
# claude-code as root, so a worker could never actually update it: the
# auto-updater printed "Updated to latest" into ~/.local/share/claude while
# every launch kept running the root-owned npm copy the image was built with.

# jj (Jujutsu) is Rudder's required internal VCS substrate. node:22-slim ships no
# jj; install the prebuilt static musl binary system-wide so every user on the
# machine has it (Rudder's own per-user copy in ~/.rudder/bin is skipped below).
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) JJ_ARCH=x86_64-unknown-linux-musl ;; \
      arm64) JJ_ARCH=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}"; exit 1 ;; \
    esac; \
    JJ_VERSION=v0.40.0; \
    curl -fsSL "https://github.com/jj-vcs/jj/releases/download/${JJ_VERSION}/jj-${JJ_VERSION}-${JJ_ARCH}.tar.gz" -o /tmp/jj.tar.gz; \
    mkdir -p /tmp/jjx; \
    tar -xzf /tmp/jj.tar.gz -C /tmp/jjx; \
    mv /tmp/jjx/jj /usr/local/bin/jj; \
    chmod +x /usr/local/bin/jj; \
    rm -rf /tmp/jj.tar.gz /tmp/jjx; \
    jj --version

COPY --from=git-builder /git-runtime/usr/local/ /usr/local/
RUN test "$(/usr/local/bin/git --version)" = "git version 2.56.0" \
  && git --version

# Backend archives depend on this tool layer, independently of Rudder's native
# build and package. A code change must not download the same backends again.
FROM worker-tools AS worker-common
WORKDIR /opt/rudder
# Copy scripts BEFORE npm ci so the package's postinstall hook (scripts/postinstall.mjs)
# resolves; RUDDER_SKIP_POSTINSTALL makes it a no-op since jj is installed above.
COPY --chmod=0644 package.json package-lock.json ./
COPY scripts ./scripts
RUN RUDDER_SKIP_POSTINSTALL=1 npm ci --omit=dev
COPY dist ./dist
COPY assets ./assets
# Local npm builds can contain platform-keyed binaries that the resolver would
# prefer over our fresh container build. Install exactly the builder's pair.
RUN rm -rf ./dist/native && mkdir -p ./dist/native
# Link only the already copied JS package here. Its postinstall immediately
# returns under RUDDER_SKIP_POSTINSTALL; no native executable is needed yet.
RUN RUDDER_SKIP_POSTINSTALL=1 npm link

WORKDIR /opt/rudder-worker
COPY cloud/worker/package.json ./package.json
RUN npm install --omit=dev --no-audit --no-fund \
  && npm cache clean --force
COPY cloud/worker/supervisor.mjs ./supervisor.mjs
COPY cloud/worker/active-runs.mjs ./active-runs.mjs
COPY cloud/worker/home.mjs ./home.mjs
COPY cloud/worker/transcripts.mjs ./transcripts.mjs
COPY cloud/worker/hook-paths.mjs ./hook-paths.mjs
COPY cloud/worker/asks.mjs ./asks.mjs
COPY cloud/worker/agent-status.mjs ./agent-status.mjs
COPY cloud/worker/logins.mjs ./logins.mjs
COPY cloud/worker/digest.mjs ./digest.mjs
COPY cloud/worker/plan.mjs ./plan.mjs
COPY cloud/worker/theme.mjs ./theme.mjs
COPY cloud/worker/terminal-transport.mjs ./terminal-transport.mjs
COPY cloud/worker/tailscale-previews.mjs ./tailscale-previews.mjs
COPY cloud/worker/cache-env.sh ./cache-env.sh
COPY cloud/worker/task-launch.mjs ./task-launch.mjs

COPY cloud/worker/entrypoint.sh /usr/local/bin/rudder-worker
COPY cloud/worker/root-bootstrap.sh /usr/local/bin/rudder-worker-root-bootstrap
RUN chmod 0755 /usr/local/bin/rudder-worker /usr/local/bin/rudder-worker-root-bootstrap

# Claude refuses --permission-mode bypassPermissions as root, so we run the
# supervisor (and everything it spawns) as a non-root user.
RUN groupadd --gid 1500 rudder \
  && useradd --create-home --shell /bin/bash --uid 1500 --gid 1500 rudder \
  && mkdir -p /workspace \
  && chown -R rudder:rudder /workspace /opt/rudder-worker

# Native-only source changes must not reinstall the unchanged worker packages
# or recreate the worker account. Install the SAME exact builder outputs after
# those stable preparations, preserving worker-common's final working directory.
WORKDIR /opt/rudder
COPY --from=native-builder /rudder-native ./dist/native/rudder-native
COPY --from=native-builder /rudder-jj ./dist/native/rudder-jj
COPY --from=native-builder --chown=0:0 --chmod=0555 /rudder-root-storage /usr/local/bin/rudder-root-storage
COPY --from=native-builder --chown=0:0 --chmod=0555 /rudder-machine-root /usr/local/bin/rudder-machine-root
RUN chmod +x ./dist/native/rudder-native ./dist/native/rudder-jj
# Bind deployed executable observations to the actual image build output.
RUN set -eu; \
    for executable in /opt/rudder/dist/native/rudder-native /opt/rudder/dist/native/rudder-jj /usr/local/bin/rudder-root-storage /usr/local/bin/rudder-machine-root; do \
      digest="$(sha256sum "$executable")"; digest="${digest%% *}"; \
      bytes="$(stat -c %s "$executable")"; \
      printf 'RUDDER_PRODUCTION_ELF %s %s %s\n' "$executable" "$digest" "$bytes"; \
    done

WORKDIR /opt/rudder-worker

# Only the explicit managed target depends on these pinned assets. No model,
# credentials, installer scripts or optional native addons execute here.
FROM worker-tools AS managed-assets
USER root
ENV PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ARG TARGETARCH
ARG WORKER_NODE_IMAGE
WORKDIR /build
COPY --chown=0:0 --chmod=0555 cloud/worker/managed-runtime/assets.py /build/assets.py
COPY --chown=0:0 --chmod=0555 cloud/worker/managed-runtime/claude_assets.py /build/claude_assets.py
COPY --chown=0:0 --chmod=0444 cloud/worker/managed-runtime/claude/* /build/claude/
COPY --chown=0:0 --chmod=0444 cloud/worker/managed-runtime/policy.json /build/policy.json
COPY --chown=0:0 --chmod=0444 native/tests/fixtures/codex_protocol/codex-acquisition.json /build/codex-acquisition.json
RUN test "$TARGETARCH" = amd64 \
  && test "$WORKER_NODE_IMAGE" = node:22-bookworm-slim@sha256:4b150b48b4116e2a22f2c39cc27025fbf4f153e2078fd3b14d159fdb5c7279da \
  && test -x /usr/bin/gpgv \
  && mkdir -p /out/runtime /out/sidecar/node_modules /out/receipts
COPY --chown=0:0 --chmod=0555 cloud/worker/host-control.mjs /out/sidecar/host-control.mjs
COPY --chown=0:0 --chmod=0555 cloud/worker/host-control-core.mjs /out/sidecar/host-control-core.mjs
COPY --chown=0:0 --chmod=0555 cloud/worker/runtime-qualification.mjs /out/sidecar/runtime-qualification.mjs
COPY --chown=0:0 --chmod=0555 cloud/worker/root-upload.mjs /out/sidecar/root-upload.mjs
COPY --chown=0:0 --chmod=0555 cloud/worker/managed-runtime/sidecar-package.json /out/sidecar/package.json
COPY --chown=0:0 --chmod=0555 cloud/worker/managed-runtime/sidecar-package-lock.json /out/sidecar/package-lock.json
RUN /usr/bin/python3 -I /build/assets.py acquire --policy /build/policy.json --artifact codex \
      --metadata /build/codex-acquisition.json --output /out/runtime/codex --receipt /out/receipts/codex-acquisition.json \
  && /usr/bin/python3 -I /build/assets.py acquire --policy /build/policy.json --artifact ws \
      --output /out/sidecar/node_modules/ws --receipt /out/receipts/ws-acquisition.json \
  && /usr/bin/python3 -I /build/claude_assets.py --policy /build/policy.json --metadata-root /build/claude \
      --scratch-root /build --output /out/runtime/claude --receipt /out/receipts/claude-acquisition.json \
  && mkdir -p /out/receipts/claude \
  && cp /build/claude/* /out/receipts/claude/ \
  && chmod 0444 /out/receipts/claude/* && chmod 0555 /out/receipts/claude \
  && /usr/bin/python3 -I /build/assets.py deny-addons --root /out/sidecar \
  && /usr/bin/python3 -I /build/assets.py manifest --policy /build/policy.json --kind runtime \
      --root /out/runtime --pin /out/receipts/runtime-config-pin.json \
  && /usr/bin/python3 -I /build/assets.py manifest --policy /build/policy.json --kind sidecar \
      --root /out/sidecar --ws-receipt /out/receipts/ws-acquisition.json --pin /out/receipts/sidecar-policy-pin.json \
  && /usr/bin/python3 -I /build/assets.py manifest --policy /build/policy.json --kind runtime --root /out/runtime --verify \
  && /usr/bin/python3 -I /build/assets.py manifest --policy /build/policy.json --kind sidecar --root /out/sidecar \
      --ws-receipt /out/receipts/ws-acquisition.json --verify

# Explicit new-managed-home image. It inherits no legacy backend install layer;
# it contains one pinned ELF per backend. Claude is a packaged candidate;
# managed Claude execution stays unsupported pending actual guest qualification.
FROM worker-common AS managed
USER root
ENV PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
COPY --from=native-builder --chown=0:0 --chmod=0555 /rudder-image-preseed /usr/local/bin/rudder-image-preseed
COPY --from=managed-assets --chown=0:0 /out/runtime/ /opt/rudder-managed-runtime/v1/
COPY --from=managed-assets --chown=0:0 /out/sidecar/ /opt/rudder-host-control/
COPY --from=managed-assets --chown=0:0 /out/receipts/ /usr/local/share/rudder-managed-image/
COPY --from=managed-assets --chown=0:0 --chmod=0444 /build/policy.json /usr/local/share/rudder-managed-image/policy.json
COPY --from=managed-assets --chown=0:0 --chmod=0555 /build/assets.py /usr/local/libexec/rudder-managed-image-assets
COPY --chown=0:0 --chmod=0555 cloud/worker/managed-runtime/verify_image_layout.py /usr/local/libexec/rudder-managed-image-layout
# Qualification retries can refresh these installed-byte observations while
# reusing compiled artifacts. This argument is not a runtime environment value.
ARG RUDDER_IMAGE_QUALIFICATION_NONCE=release
# Build-only profile selection. Baked CAS still requires separate runtime
# admission; selecting this argument never enables managed execution by itself.
ARG RUDDER_IMAGE_PRESEED_PROFILE=legacy-v1
RUN test -n "$RUDDER_IMAGE_QUALIFICATION_NONCE" \
  && case "$RUDDER_IMAGE_PRESEED_PROFILE" in legacy-v1|baked-cas-v1) ;; *) exit 1 ;; esac \
  # The protected SDK mount checks every ancestor. worker-common makes this
  # directory user-owned for legacy workers; normalize only the managed image.
  && chown 0:0 /opt/rudder-worker && chmod 0555 /opt/rudder-worker \
  && install -d -o 0 -g 0 -m 0555 /opt/rudder-worker/compiler-sdk-v1 \
  && test -L /usr/lib/ssl/private \
  && test "$(readlink /usr/lib/ssl/private)" = /etc/ssl/private \
  && rm /usr/lib/ssl/private \
  # The release archive preserves jj's upstream uid. Debian's public helpers
  # also carry privileged groups that an unprivileged managed guest cannot use.
  # Normalize these exact executable inputs before the immutable tree is pinned.
  && chown 0:0 /usr/local/bin/jj /usr/lib/x86_64-linux-gnu/utempter/utempter \
       /usr/sbin/unix_chkpwd /usr/bin/chage /usr/bin/expiry /usr/bin/ssh-agent \
  && chmod 0755 /usr/local/bin/jj /usr/lib/x86_64-linux-gnu/utempter/utempter \
       /usr/sbin/unix_chkpwd /usr/bin/chage /usr/bin/expiry /usr/bin/ssh-agent \
  && test -d /opt/yarn-v1.22.22 \
  && test ! -e /usr/local/lib/rudder-yarn \
  && mv /opt/yarn-v1.22.22 /usr/local/lib/rudder-yarn \
  && chown -R 0:0 /usr/local/lib/rudder-yarn \
  && ln -sfn ../lib/rudder-yarn/bin/yarn /usr/local/bin/yarn \
  && ln -sfn ../lib/rudder-yarn/bin/yarnpkg /usr/local/bin/yarnpkg \
  && test "$(/usr/local/bin/yarn --version)" = 1.22.22 \
  # Slim Debian omits the documentation targets. Preserve every working link.
  && find /usr/share/doc /usr/share/man -xtype l -delete \
  && find /etc/alternatives -xtype l -name '*.gz' -delete \
  && chmod 0755 /usr/share/man/man1 /usr/share/man/man5 /usr/share/man/man7 /usr/share/man/man8 \
  # Build-time only: prepare the exact empty provider boundary, never erase
  # module payload from a booted Machine. Boot modules are not IMAGE bytes.
  && test ! -L /usr/lib/modules \
  && if test -e /usr/lib/modules; then \
       test -d /usr/lib/modules && test ! -L /usr/lib/modules \
       && test -z "$(find /usr/lib/modules -mindepth 1 -maxdepth 1 -print -quit)"; \
     fi \
  && install -d -o 0 -g 0 -m 0555 /usr/lib/modules \
  # Fly replaces swapon at boot. Guests receive an independent empty mask.
  && test ! -L /usr/sbin/swapon \
  && install -o 0 -g 0 -m 0444 /dev/null /usr/sbin/swapon \
  # Fly boot creates this empty backup of the disabled swap tool. Declare it
  # before pinning so the regular-file namespace is identical on first boot.
  && test ! -L /usr/bin/swapon.original \
  && install -o 0 -g 0 -m 0444 /dev/null /usr/bin/swapon.original \
  && install -d -o 0 -g 0 -m 0755 /workspace \
  && chmod 0555 /opt/rudder-managed-runtime/v1 /opt/rudder-host-control \
  && /usr/bin/python3 -I /usr/local/libexec/rudder-managed-image-assets manifest \
      --policy /usr/local/share/rudder-managed-image/policy.json --kind runtime --root /opt/rudder-managed-runtime/v1 --verify \
  && /usr/bin/python3 -I /usr/local/libexec/rudder-managed-image-assets manifest \
      --policy /usr/local/share/rudder-managed-image/policy.json --kind sidecar --root /opt/rudder-host-control \
      --ws-receipt /usr/local/share/rudder-managed-image/ws-acquisition.json --verify \
  && /usr/bin/python3 -I /usr/local/libexec/rudder-managed-image-assets components \
      --policy /usr/local/share/rudder-managed-image/policy.json --output /usr/local/share/rudder-managed-image/components.json \
  && install -d -o 0 -g 0 -m 0700 /run/rudder-host-control \
  && chmod 0555 /usr/local/share/rudder-managed-image \
  && /usr/bin/python3 -I -B /usr/local/libexec/rudder-managed-image-layout \
  && /usr/bin/python3 -I -B -c 'import hashlib,pathlib; names=("/opt/rudder/dist/native/rudder-native","/opt/rudder/dist/native/rudder-jj","/usr/local/bin/rudder-root-storage","/usr/local/bin/rudder-machine-root"); [(print("RUDDER_PRODUCTION_ELF",name,hashlib.sha256(pathlib.Path(name).read_bytes()).hexdigest(),pathlib.Path(name).stat().st_size)) for name in names]' \
  && case "$RUDDER_IMAGE_PRESEED_PROFILE" in \
       legacy-v1) /usr/local/bin/rudder-image-preseed \
         && /usr/bin/python3 -c 'import hashlib,pathlib; p=pathlib.Path("/var/lib/rudder-image-preseed-v1"); print("RUDDER_IMAGE_PRESEED_MANIFEST_SHA256",hashlib.sha256((p/"manifest.json").read_bytes()).hexdigest()); print("RUDDER_IMAGE_PRESEED_PIN",(p/"pin.json").read_text())' ;; \
       baked-cas-v1) /usr/local/bin/rudder-image-preseed --bake-cas-v1 ;; \
     esac
LABEL ai.exla.rudder.worker-role="managed-opt-in" \
      ai.exla.rudder.runtime-profile="fly-managed-image-v1" \
      ai.exla.rudder.claude-supported="false"
WORKDIR /
# Explicit provider/config/fresh-volume qualification is still required. No
# fallback into the mutable legacy home/supervisor when config is absent.
ENTRYPOINT ["/usr/bin/env", "-i", "/usr/local/bin/rudder-machine-root"]
CMD []

# Last target stays the legacy/default worker, with unchanged backend updater
# paths/install behavior. Current deploys that omit --target still use this.
FROM worker-common AS legacy

USER rudder
ENV NPM_CONFIG_PREFIX=/home/rudder/.npm-global \
    PATH=/home/rudder/.local/bin:/home/rudder/.npm-global/bin:${PATH}
RUN curl -fsSL https://claude.ai/install.sh | bash \
  && claude --version \
  && npm install -g @openai/codex@latest acpx@latest hunkdiff@latest pnpm@latest \
  && npm cache clean --force \
  && codex --version
WORKDIR /workspace

# Boot root touches only the validated device, then setpriv enters UID/GID1500
# before the existing entrypoint can read profiles, start updates or run agents.
USER root
ENTRYPOINT ["/usr/local/bin/rudder-worker-root-bootstrap"]
