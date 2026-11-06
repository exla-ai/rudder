# syntax=docker/dockerfile:1
ARG WORKER_NODE_IMAGE=node:22-bookworm-slim
# checkout: compile rudder-native/rudder-jj and copy ./dist from this checkout.
# release: take dist and the linux-x64 pair CI built from the published npm
# package @exla/rudder@RUDDER_VERSION (scripts/worker-image.mjs); no Rust stage.
ARG RUDDER_SOURCE=checkout
FROM rust:1-bookworm AS native-checkout
WORKDIR /src
COPY native ./native
COPY engine ./engine
COPY vcs-bridge ./vcs-bridge
# The command surface embeds the operations registry with include_str!.
COPY src/operations ./src/operations
# The binary embeds assets (the notifier icon) with include_bytes!.
COPY assets ./assets
# The dashboard header shows the version, read from package.json with include_str!.
COPY package.json ./
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
       --bin rudder-native --bin rudder-jj \
  && cp target/release/rudder-native /rudder-native \
  && cp target/release/rudder-jj /rudder-jj

FROM ${WORKER_NODE_IMAGE} AS release-package
ARG RUDDER_VERSION
RUN test -n "$RUDDER_VERSION" && mkdir /pkg && cd /tmp \
  && npm pack "@exla/rudder@${RUDDER_VERSION}" --silent \
  && tar xzf ./*.tgz -C /pkg --strip-components=1 && rm ./*.tgz \
  && test -x /pkg/dist/native/linux-x64/rudder-native && test -x /pkg/dist/native/linux-x64/rudder-jj

FROM scratch AS native-release
COPY --from=release-package /pkg/dist/native/linux-x64/rudder-native /rudder-native
COPY --from=release-package /pkg/dist/native/linux-x64/rudder-jj /rudder-jj

FROM scratch AS dist-checkout
COPY dist /dist
FROM scratch AS dist-release
COPY --from=release-package /pkg/dist /dist

FROM native-${RUDDER_SOURCE} AS native-builder
FROM dist-${RUDDER_SOURCE} AS dist-source

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
# The last three package lines are what headless Chromium (Playwright,
# Puppeteer) needs to start at all. Without them every agent that screenshots a web app spent 1 to
# 5 minutes unpacking .debs into its scratch dir (no root), and some gave up.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates curl wget git git-lfs openssh-client tar gzip zip unzip xz-utils bzip2 zsh \
    python3 python3-pip python3-venv python3-dev build-essential cmake pkg-config libssl-dev \
    jq ripgrep fd-find tree file less lsof procps htop tmux nano vim-tiny rsync \
    sqlite3 postgresql-client redis-tools util-linux gettext-base libcurl4 libexpat1 zlib1g \
    libasound2 libatk-bridge2.0-0 libatk1.0-0 libatspi2.0-0 libcairo2 libcups2 libdbus-1-3 libdrm2 libgbm1 \
    libglib2.0-0 libnspr4 libnss3 libpango-1.0-0 libx11-6 libxcb1 libxcomposite1 libxdamage1 libxext6 \
    libxfixes3 libxkbcommon0 libxrandr2 fontconfig fonts-liberation \
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

# sccache: Rudder points every Rust build at the machine's shared compile cache
# (src/tool-cache.ts sets RUSTC_WRAPPER when sccache is on PATH), so agents
# reuse the warm snapshot builder's compiled crates instead of rebuilding them.
ARG SCCACHE_VERSION=0.18.0
ARG SCCACHE_SHA256=45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89
RUN set -eux; \
    url="https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz"; \
    curl -fsSL "$url" -o /tmp/sccache.tgz; \
    echo "${SCCACHE_SHA256}  /tmp/sccache.tgz" | sha256sum -c -; \
    tar -xzf /tmp/sccache.tgz -C /tmp; \
    install -m 0755 "/tmp/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache" /usr/local/bin/sccache; \
    rm -rf /tmp/sccache.tgz "/tmp/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl"; \
    sccache --version

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

# Claude refuses --permission-mode bypassPermissions as root, so we run the
# supervisor (and everything it spawns) as a non-root user. The account and its
# agent CLIs depend only on this tool layer, so a Rudder release reuses them
# from cache; workers still run bounded Claude/Codex updates at boot.
RUN groupadd --gid 1500 rudder \
  && useradd --create-home --shell /bin/bash --uid 1500 --gid 1500 rudder
WORKDIR /home/rudder
USER rudder
RUN export NPM_CONFIG_PREFIX=/home/rudder/.npm-global \
    PATH=/home/rudder/.local/bin:/home/rudder/.npm-global/bin:${PATH} \
  && curl -fsSL https://claude.ai/install.sh | bash \
  && claude --version \
  && npm install -g @openai/codex@latest acpx@latest hunkdiff@latest pnpm@latest \
  && npm cache clean --force \
  && codex --version
USER root

# Backend archives depend on this tool layer, independently of Rudder's native
# build and package. A code change must not download the same backends again.
FROM worker-tools AS worker-common
WORKDIR /opt/rudder
# Copy scripts BEFORE npm ci so the package's postinstall hook (scripts/postinstall.mjs)
# resolves; RUDDER_SKIP_POSTINSTALL makes it a no-op since jj is installed above.
COPY --chmod=0644 package.json package-lock.json ./
COPY scripts ./scripts
RUN RUDDER_SKIP_POSTINSTALL=1 npm ci --omit=dev
COPY --from=dist-source /dist ./dist
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
COPY cloud/worker/codex-trust.mjs ./codex-trust.mjs
COPY cloud/worker/home-host.mjs ./home-host.mjs
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
COPY cloud/worker/steer-inbox.mjs ./steer-inbox.mjs
COPY cloud/worker/cleanup.mjs ./cleanup.mjs
COPY cloud/worker/telemetry.mjs ./telemetry.mjs
COPY cloud/worker/burst-protocol.mjs ./burst-protocol.mjs
COPY cloud/worker/burst-broker.mjs ./burst-broker.mjs
COPY cloud/worker/burst.mjs ./burst.mjs
COPY cloud/worker/builder.mjs ./builder.mjs
RUN printf '#!/bin/sh\nexec node /opt/rudder-worker/burst.mjs "$@"\n' > /usr/local/bin/rudder-burst \
  && chmod 0755 /usr/local/bin/rudder-burst

COPY cloud/worker/entrypoint.sh /usr/local/bin/rudder-worker
COPY cloud/worker/root-bootstrap.sh /usr/local/bin/rudder-worker-root-bootstrap
RUN chmod 0755 /usr/local/bin/rudder-worker /usr/local/bin/rudder-worker-root-bootstrap

RUN mkdir -p /workspace \
  && chown -R rudder:rudder /workspace /opt/rudder-worker

# Native-only source changes must not reinstall the unchanged worker packages
# or recreate the worker account. Install the SAME exact builder outputs after
# those stable preparations, preserving worker-common's final working directory.
WORKDIR /opt/rudder
COPY --from=native-builder /rudder-native ./dist/native/rudder-native
COPY --from=native-builder /rudder-jj ./dist/native/rudder-jj
RUN chmod +x ./dist/native/rudder-native ./dist/native/rudder-jj
# Bind deployed executable observations to the actual image build output.
RUN set -eu; \
    for executable in /opt/rudder/dist/native/rudder-native /opt/rudder/dist/native/rudder-jj; do \
      digest="$(sha256sum "$executable")"; digest="${digest%% *}"; \
      bytes="$(stat -c %s "$executable")"; \
      printf 'RUDDER_PRODUCTION_ELF %s %s %s\n' "$executable" "$digest" "$bytes"; \
    done

WORKDIR /opt/rudder-worker

# Last target stays the legacy/default worker, with unchanged backend updater
# paths/install behavior. Current deploys that omit --target still use this.
FROM worker-common AS legacy

USER rudder
ENV NPM_CONFIG_PREFIX=/home/rudder/.npm-global \
    PATH=/home/rudder/.local/bin:/home/rudder/.npm-global/bin:${PATH}
WORKDIR /workspace

# Boot root touches only the validated device, then setpriv enters UID/GID1500
# before the existing entrypoint can read profiles, start updates or run agents.
USER root
ENTRYPOINT ["/usr/local/bin/rudder-worker-root-bootstrap"]
