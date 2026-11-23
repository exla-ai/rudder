---
name: rudder-ship
description: Ship a Rudder release end to end, one version everywhere from one commit. Offstage Macs build the darwin pair and the signed Mac app, the home runs the release checks and builds linux-x64 and the npm tarball, then the Fly worker image and Cloud deploy, then the version tag publishes npm and the Mac app. Also the rudder.exla.ai site on Vercel. Use when asked to ship, release, deploy, publish, cut a version, build the Mac app or DMG, or deploy the site.
---

# Ship Rudder

An explicit ship, release or deploy request authorizes these steps (AGENTS.md,
Shipping). "Ship it" means all of them: npm, Cloud and the Mac app, always at
the same version, and the site when `site/` changed.

Never force-push. Never `npm publish` Rudder by hand. Never restart the live
home or its agents: homes adopt a new image when idle, or the user presses
Alt+r.

GitHub builds and tests nothing (the user's rule). `scripts/release.mjs` has
the design in its header; `.github/workflows/release.yml` only moves files,
checks their hashes and publishes.

| Step | Where | Takes |
|---|---|---|
| 1. Version commit, push `release-check/X.Y.Z` | Release run: draft release `vX.Y.Z`, darwin-arm64 pair on Offstage, Mac app tested, signed and notarized on Offstage, Sparkle feed signed | about 5 min, during step 2 |
| 2. Checks, then `release.mjs stage` | the home: clean clone, full checks once, linux-x64 pair, npm tarball into the draft | checks, then about 2 min |
| 3. Worker image, Cloud deploy | Fly's builder, from the staged tarball | about 10 to 15 min |
| 4. Tag `vX.Y.Z` | Release run: npm publish (OIDC), Mac release once Cloud runs X.Y.Z, draft and branch removed | about 3 min |

## 1. Version commit and release-check push

Release from a scratch jj workspace, never the user's checkout or an isolated
worker's workspace. Commit whole files.

```sh
v=X.Y.Z
jj workspace add /workspace/rudder-scratch/release/ws -r main   # once
cd /workspace/rudder-scratch/release/ws
npm version $v --no-git-tag-version
jj commit package.json package-lock.json mac/project.yml cloud/fly.toml -m "$v"
jj bookmark set main -r @-
jj git export
sha="$(jj log -r main --no-graph -T commit_id)"
git -C /workspace/code/exla-ai-rudder -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
  push origin "$sha:refs/heads/release-check/$v"
```

- `npm version` runs `scripts/version.mjs`: it sets the Mac app's
  `MARKETING_VERSION` and build number in `mac/project.yml` and the worker
  image pin `registry.fly.io/rudder-workers-exla:X.Y.Z` in `cloud/fly.toml`.
  Never set them by hand. The Mac release waits for Cloud to run that pin, so
  it must be in the tagged commit.
- The push starts **Release** on the branch. Its first job checks every
  release secret and version line and creates the draft release `vX.Y.Z`
  (seconds). Then, in parallel, `.offstage/native.mjs` builds the darwin-arm64
  pair and `.offstage/mac.mjs` tests, signs and notarizes `Rudder.dmg`, and the
  feed is signed with `SPARKLE_ED_PRIVATE_KEY`. Watch with
  `gh run list --workflow release.yml --limit 2`.
- The plain `git push` can hit a bot token that gets 403; the `gh` helper
  uses the user's own token.
- A fix after this push: commit it on top (the version commit stays below
  it), move `main` and push the new head to `release-check/$v` again. That
  cancels the staging in flight and stages the new commit. The tag goes on the
  last commit pushed there.
- Offstage holds credit for work in flight: up to 20 minutes for the native
  task and 80 for the Mac sandbox. If a job fails with "The credit has $X
  left", ask the user to top up at console.offstage.cloud (Billing).

## 2. Release checks and stage, on the home

Run the checks once, in a clean clone of the pushed branch. Unset `RUDDER_*`
(a Cloud home's env breaks native tests) and use a separate cargo target dir.

```sh
unset $(env | grep -o '^RUDDER_[A-Z_]*')
export CARGO_TARGET_DIR=/workspace/rudder-scratch/release/target
rm -rf /workspace/rudder-scratch/release/src
git clone --depth 1 --branch "release-check/$v" https://github.com/exla-ai/rudder.git /workspace/rudder-scratch/release/src
cd /workspace/rudder-scratch/release/src
npm ci && npm --prefix cloud ci
npm run check
npm test
npm --prefix cloud run build
cargo test --workspace --locked --no-fail-fast
npm run build
RUDDER_NATIVE_BIN="$CARGO_TARGET_DIR/release/rudder-native" npm run test:tui
node scripts/release.mjs stage
```

`stage` refuses a dirty clone, a version line that is not `$v`, a tag that
exists, and a branch on GitHub that is not this commit. It builds the
linux-x64 pair (a no-op after `npm run build`), waits for the darwin pair,
packs `.release/exla-rudder-$v.tgz` with both pairs, checks it the way the tag
run will, uploads it, waits for the signed Mac app, verifies its feed and
prints exactly what the tag will publish. Re-run it after any failure; it
starts over from an empty `.release/`.

- Use the `rudder-burst` skill for the heavy checks when it is on PATH.
- Never run `npm test` and `npm run build` at the same time: both write `dist/`.
- Re-run a failing test alone before treating it as real. Timing-sensitive
  terminal tests such as `worker_wheel_at_edge_does_not_flash_notice` can flake
  under full-suite load.
- If many agent-broker or agent-mcp tests fail at once, the sccache server's
  TMPDIR is probably gone. Recreate it, or run with `RUSTC_WRAPPER=`.
- TUI color assertions follow `native/src/theme.rs`. Update the tests when the
  palette changes on purpose.
- A failing check needs a fix commit: see the last notes of step 1, then
  `git fetch --depth 1 origin "release-check/$v" && git checkout --detach FETCH_HEAD`
  in the clone.

Before a release that depends on a new npm dependency version (vibeprint in
`cloud/worker/package.json`, for example), confirm the registry serves it. The
worker image runs `npm install` against the registry.

## 3. Cloud: worker image and control plane, before the tag

The image takes dist and the linux-x64 pair from the staged tarball, so it
does not wait for npm, and Cloud can run the release before the tag. The
clone is clean, so it is the build context for both.

```sh
export FLY_NO_UPDATE_CHECK=1
flyctl auth whoami          # must print an account
cd /workspace/rudder-scratch/release/src
node scripts/worker-image.mjs $v
(cd cloud && flyctl deploy --remote-only)
```

`worker-image.mjs` checks the tarball against `.release/npm.json`, builds on
Fly's builder, pushes `registry.fly.io/rudder-workers-exla:$v` and copies it to
`registry.fly.io/rudder-worker-images` for external home hosts. The clone's
`cloud/fly.toml` already pins that image.

- If `flyctl` says the session expired, the laptop's stale `~/.fly/config.yml`
  was synced over the login. Log in with
  `script -qfec "flyctl auth login" /dev/null </dev/null` in the background,
  give the user the `https://fly.io/app/auth/cli/...` link, then save
  `flyctl auth token > ~/.fly-token` (mode 600) and use
  `FLY_API_TOKEN=$(cat ~/.fly-token)` from then on.
- A `~/.fly-token` deploy token can get 404 from registry.fly.io manifests, so
  `worker-image.mjs` times out after the push. The token
  `FLY_API_TOKEN=$(cat ~/.fly-token) flyctl auth token` prints reads the
  registry; finish the copy with it.

Verify:

- `https://rudder.exla.ai/app` loads. It redirects to
  `rudder-cloud-control.fly.dev/app`; check a changed app file there
  (rudder.exla.ai/app/<file> is the Vercel site, always 404).
- `flyctl releases -a rudder-cloud-control | head -3` shows the new release
  with the expected image and a current UTC time.
- `curl -s https://rudder.exla.ai/health` reports `workerImage` as the new pin.
  The tag's Mac release waits for exactly this.
- A signed-in smoke check of the app passes (projects list, one agent's chat).

## 4. Tag and publish

```sh
git -C /workspace/code/exla-ai-rudder tag -a "v$v" -m "$v" "$sha"
git -C /workspace/code/exla-ai-rudder -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
  push origin main "v$v"
```

The tag starts **Release** again, which only checks and publishes:

- **Publish npm** downloads the staged tarball, checks it against `npm.json`,
  the tag's `package.json` and the darwin pair Offstage built, then runs
  `npm publish <tarball> --provenance` through trusted publishing.
- **Publish the Mac app** checks the DMG and the signed feed against
  `mac/App/Info.plist`, waits until `rudder.exla.ai/health` reports the pinned
  image (at most 30 minutes), publishes `mac-v<version>-<build>` as the latest
  release with `Rudder.dmg` and `appcast.xml`, and checks both public URLs.
- **Remove the draft** deletes the draft release and `release-check/$v`.

Watch it with `gh run watch <id> --exit-status` in the background. A failed
job can be re-run with `gh run rerun <id> --failed`: the npm job compares the
registry's integrity with the staged tarball and never publishes twice, and
the Mac job reuses an existing `mac-v` tag at the same commit. Fix the cause
first (`gh run view <id> --log-failed`); a code fix needs the next version.
Never move a pushed tag.

npm is done when
`curl -s -o /dev/null -w '%{http_code}' https://registry.npmjs.org/@exla%2frudder/$v`
prints 200. npm processes a trusted publish before serving it: 5.0.5 took
about 4 minutes after CI printed `+ @exla/rudder@X.Y.Z`, 5.0.7 more than 15.
A 404 then is not a failure; npm emails "Successfully published" to the
account when it is served. Check https://status.npmjs.org before digging.
Re-running the npm job in that window fails on npm's "cannot publish over"
answer; wait for the 200 instead.

Report the verified deploy, npm and the Mac release separately. They are
separate facts.

### The Mac release

- The Mac app ships at every version, whether or not `mac/` changed, so the
  versions always match.
- Its build number is derived from the version, so Sparkle always sees the
  newer build as an update.
- Signing uses the `MAC_DEVELOPER_ID_P12` and `MAC_DEVELOPER_ID_PASSWORD`
  secrets (the exported Developer ID Application certificate), notarization
  `ASC_KEY_ID`, `ASC_ISSUER_ID` and `ASC_KEY_P8`.
- The Sparkle private key (base64 of the Ed25519 seed) exists only as the
  `SPARKLE_ED_PRIVATE_KEY` repo secret. No machine has a key file:
  `~/.rudder-sparkle-ed25519` was lost in a machine update on 2026-11-19.
  Never print the secret. If it is missing, the branch run stops at its first
  step; only the user can set it again, from their own backup.
- `https://rudder.exla.ai/download/mac` redirects to
  `releases/latest/download/Rudder.dmg`, and installed apps (1.2.0 and later)
  update from `releases/latest/download/appcast.xml`. Rudder's npm releases
  create no GitHub release, and the staging draft is never published, so the
  Mac release stays "latest". A latest release without `appcast.xml` stops
  every installed app's updates: never publish a release with a bare
  `gh release create`, and never publish the `vX.Y.Z` draft.
- `gh workflow run mac.yml` (optionally `-f signing_check=true`) checks the
  Mac app on Offstage without publishing.
- A DMG built by hand on the MacBook Pro (`mac/scripts/build.sh`) is published
  with `mac/scripts/publish.sh <dir> <commit>`, which stages a draft
  `mac-v<version>-<build>` and runs `mac-publish.yml` to sign the feed and
  publish it. `publish.sh` refuses a tag that already has a release or draft.

Verify:

- `gh release view mac-v<version>-<build>` lists `Rudder.dmg` and `appcast.xml`
  (no `release.env`).
- `curl -sL https://github.com/exla-ai/rudder/releases/latest/download/appcast.xml`
  shows the new version and build, and its enclosure's `length` equals the
  DMG's size. The Mac job checks that both public URLs serve the staged bytes.
- On a Mac running the previous Sparkle build, Rudder > Check for Updates…
  offers the new version and installs it.

## 5. Site (rudder.exla.ai on Vercel)

The site in `site/` is static and has no Git integration. Deploy it by hand:

```sh
cd site
env -u VERCEL_TOKEN vercel link --yes --project rudder
env -u VERCEL_TOKEN vercel deploy --prod --yes
rm -f .env.local
cd .. && jj restore site/.gitignore
```

- `VERCEL_TOKEN` in the env is invalid; the CLI's own login works without it.
- `vercel link` writes `site/.env.local` (an OIDC token, ignored by Git) and
  edits `site/.gitignore`. Remove both after deploying.

Verify with `curl -s https://rudder.exla.ai/ | grep -o '<title>[^<]*</title>'`,
or check whatever you changed.

Keep the page `<title>` as plain "Rudder". Browsers that rename downloads from
the page title, such as Arc and Dia's Tidy Downloads, otherwise save the DMG
under a longer name.

## 6. Finish

- Delete `/workspace/rudder-scratch/release/` (the clone, its target dir and
  the jj workspace: `jj workspace forget` it first).
- Update `docs/v4-handoff.md` if the release changes what is open.
- Send a short final message covering:
  - what shipped, with versions;
  - each verified fact (npm 200, image, Fly release, Mac release, site);
  - anything still waiting, such as a home that updates when idle, or an
    Offstage job stuck in the queue.
