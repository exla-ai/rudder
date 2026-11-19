---
name: rudder-ship
description: Ship a Rudder release end to end. Release checks, version bump and tag (tag CI publishes npm and the ghcr worker image), the Fly worker image and Cloud deploy, the signed Mac app built on an Offstage Mac, and the rudder.exla.ai site on Vercel. Use when asked to ship, release, deploy, publish, cut a version, build the Mac app or DMG, or deploy the site.
---

# Ship Rudder

An explicit ship, release or deploy request authorizes these steps (AGENTS.md,
Shipping). Run only the parts the request covers. "Ship it" or "deploy and
ship" means all of them: npm, Cloud, the Mac app when `mac/` changed since the
last `mac-v*` release, and the site when `site/` changed.

Never force-push. Never `npm publish` Rudder by hand. Never restart the live
home or its agents: homes adopt a new image when idle, or the user presses
Alt+r.

## 1. Release checks (once per release)

Scratch goes in `/workspace/rudder-scratch/release/`; delete it at the end.
Unset `RUDDER_*` (a Cloud home's env breaks native tests) and use a separate
cargo target dir.

```sh
unset $(env | grep -o '^RUDDER_[A-Z_]*')
export CARGO_TARGET_DIR=/workspace/rudder-scratch/release/target
npm run check
npm test
npm --prefix cloud run build
cargo test --workspace --no-fail-fast
npm run build
RUDDER_NATIVE_BIN="$PWD/target/release/rudder-native" npm run test:tui
```

- Use the `rudder-burst` skill for the heavy ones when it is on PATH.
- Never run `npm test` and `npm run build` at the same time: both write `dist/`.
- Re-run a failing test alone before treating it as real. Timing-sensitive
  terminal tests such as `worker_wheel_at_edge_does_not_flash_notice` can flake
  under full-suite load.
- If many agent-broker or agent-mcp tests fail at once, the sccache server's
  TMPDIR is probably gone. Recreate it, or run with `RUSTC_WRAPPER=`.
- TUI color assertions follow `native/src/theme.rs`. Update the tests when the
  palette changes on purpose.

## 2. Version, tag, push

jj is the history authority. Commit whole files, move `main`, tag the commit
with Git.

```sh
npm version X.Y.Z --no-git-tag-version
jj commit package.json package-lock.json mac/project.yml -m "X.Y.Z"
jj bookmark set main -r @-
sha="$(jj log -r main --no-graph -T commit_id)"
git -c credential.helper= -c 'credential.helper=!gh auth git-credential' push origin "$sha:refs/heads/release-check/X.Y.Z"
# wait for that Release run to pass; on a failure, fix, commit and push the branch again
git tag -a vX.Y.Z -m X.Y.Z "$sha"
git -c credential.helper= -c 'credential.helper=!gh auth git-credential' push origin main vX.Y.Z
git -c credential.helper= -c 'credential.helper=!gh auth git-credential' push origin --delete release-check/X.Y.Z
```

Qualify on `release-check/X.Y.Z` first: a failure there costs a push, not a
version. The tag run then reuses that qualification for the same SHA and only
verifies and publishes.

The plain `git push` can hit a bot token that gets 403; the `gh` helper uses
the user's own token.

The tag starts two workflows:

- **Release** builds every platform and publishes `@exla/rudder` to npm.
- **Worker image** waits for npm, then pushes `ghcr.io/exla-ai/rudder-worker:X.Y.Z`
  for external home hosts.

Watch with `gh run list --limit 4` and `gh run watch <id> --exit-status` in the
background. Done when
`curl -s -o /dev/null -w '%{http_code}' https://registry.npmjs.org/@exla%2frudder/X.Y.Z`
prints 200. npm processes a trusted publish before serving it: 5.0.5 took
about 4 minutes after CI printed `+ @exla/rudder@X.Y.Z`, 5.0.7 more than 15.
A 404 then is not a failure; npm emails "Successfully published" to the
account when it is served. Check https://status.npmjs.org before digging.

Release CI trusts only the first attempt of a tag run. `gh run rerun` stops
at "Release qualification evidence rejected", so a failed tag run cannot
publish. Fix the cause, commit, bump the patch version and tag again (5.0.2
and 5.0.3 were skipped this way). Never move a pushed tag. The macOS
qualification runner is slower than a Cloud home (the native suite took 95s
against 59s here), so a test that waits on another process's timing can pass
locally and fail there: re-run it alone, then fix the wait, not the product.

Before a release that depends on a new npm dependency version (vibeprint in
`cloud/worker/package.json`, for example), confirm the registry serves it. The
worker image runs `npm install` against the registry.

## 3. Cloud: Fly worker image and control plane

Only after npm serves the version.

```sh
export FLY_NO_UPDATE_CHECK=1
flyctl auth whoami          # must print an account
# Both builds below use the checkout as their context, and the user's checkout
# holds other agents' uncommitted edits. Build from a clean export instead.
mkdir -p /workspace/rudder-scratch/release/src && git archive --format=tar "$(jj log -r main --no-graph -T commit_id)" | tar -x -C /workspace/rudder-scratch/release/src
cd /workspace/rudder-scratch/release/src && node scripts/worker-image.mjs X.Y.Z
```

This builds on Fly's builder from the published package. It pushes
`registry.fly.io/rudder-workers-exla:X.Y.Z` and copies it to
`registry.fly.io/rudder-worker-images`.

If `flyctl` says the session expired, the laptop's stale `~/.fly/config.yml`
was synced over the login. Log in with
`script -qfec "flyctl auth login" /dev/null </dev/null` in the background, give
the user the `https://fly.io/app/auth/cli/...` link, then save
`flyctl auth token > ~/.fly-token` (mode 600) and use
`FLY_API_TOKEN=$(cat ~/.fly-token)` from then on.

Then pin the image and deploy the control plane:

1. Set `RUDDER_WORKER_IMAGE = "registry.fly.io/rudder-workers-exla:X.Y.Z"` in
   `cloud/fly.toml`.
2. Commit it.
3. Export that commit as above and run `flyctl deploy --remote-only` in the
   export's `cloud/`, never in the user's checkout.
4. Push `main`.

Verify:

- `https://rudder.exla.ai/app` loads. It redirects to
  `rudder-cloud-control.fly.dev/app`; check a changed app file there
  (rudder.exla.ai/app/<file> is the Vercel site, always 404).
- `flyctl releases -a rudder-cloud-control | head -3` shows the new release with the expected
  image and a current UTC time.
- A signed-in smoke check of the app passes (projects list, one agent's chat).

Report the verified deploy separately from the npm publish. They are separate
facts.

## 4. Mac app on an Offstage Mac

`.github/workflows/mac.yml` runs on `runs-on: offstage-macos-gui`. A headless
`offstage-macos` slot cannot reach `testmanagerd`, so `xcodebuild test` fails
there before any test runs. Each job is a fresh macOS account with no root and
no Homebrew. The workflow therefore uses
the newest `/Applications/Xcode*.app` through `DEVELOPER_DIR`. It also fetches
pinned, checksummed xcodegen, uv (for Python 3.12, which dmgbuild needs) and
`gh` into `$RUNNER_TEMP`.

1. The Mac app has Rudder's version: `npm version` (step 2) runs
   `scripts/mac-version.mjs`, which sets `MARKETING_VERSION` and the derived
   build number in `mac/project.yml`. Release the Mac app from a tagged
   Rudder version, never with its own number. Push `main` first.
2. The workflow may be disabled. If so, run `gh workflow enable mac.yml`.
3. Run `gh workflow run mac.yml --ref main -f release=true`.

The job archives and signs with the Developer ID secrets, notarizes, staples and
builds `Rudder.dmg`. `mac/scripts/publish.sh` then writes `appcast.xml` (the
Sparkle feed, EdDSA-signed with the `SPARKLE_ED_PRIVATE_KEY` secret) and
publishes GitHub release `mac-v<version>-<build>` as latest with both files.
`https://rudder.exla.ai/download/mac` redirects to
`releases/latest/download/Rudder.dmg`, and installed apps (1.2.0 and later)
update from `releases/latest/download/appcast.xml`. Rudder's npm releases
create no GitHub release, so the Mac release stays "latest". A latest release
without `appcast.xml` stops every installed app's updates: never publish a Mac
release with a bare `gh release create`.

The Sparkle private key is `~/.rudder-sparkle-ed25519` on the Linux home (base64
of the Ed25519 seed, mode 600; never print it). If the `SPARKLE_ED_PRIVATE_KEY`
secret is missing, the job stops at its first step; the user sets it with
`gh secret set SPARKLE_ED_PRIVATE_KEY --repo exla-ai/rudder < ~/.rudder-sparkle-ed25519`.

Signing needs the Developer ID Application identity. Its private key exists
only in the user's MacBook Pro keychain. Without the `MAC_DEVELOPER_ID_P12`
and `MAC_DEVELOPER_ID_PASSWORD` repo secrets, the job fails at
`exportArchive` with "No signing certificate "Developer ID Application"
found" (a push-triggered build without `release=true` still passes). Either
the user exports that identity as a .p12 into those secrets, or the build
runs on the MacBook Pro itself over the tailnet, while the laptop is open; the
first codesign there needs "Always Allow" on the keychain prompt:

- `mac/scripts/build.sh` builds, signs (Sparkle's helpers first), notarizes
  and packages in `mac/build`. If Xcode hangs there, build the unsigned
  Release archive on an Offstage Mac (`CODE_SIGNING_ALLOWED=NO`), copy its
  `Rudder.app` over and run `mac/scripts/package.sh <Rudder.app>` instead. Never
  sign the app alone with a bare `codesign`: `Sparkle.framework` and its
  helpers need this team's signature first.
- Copy `Rudder.dmg` and `release.env` back to the Linux home, push the commit
  they were built from, and publish from there:
  `RUDDER_SPARKLE_KEY=~/.rudder-sparkle-ed25519 mac/scripts/publish.sh <dir> <commit>`.

If the job sits in "Queued" with no runner, the exla-ai org is not connected to
Offstage's GitHub App (console, CI page). Tell the user; do not move the job
back to GitHub's Macs without asking.

To pin a tool update, use a release at least two weeks old. Put its sha256 next
to the URL in the Tools step.

Verify:

- `gh release view mac-v<version>-<build>` lists `Rudder.dmg` and `appcast.xml`.
- The download URL serves the new asset with `filename=Rudder.dmg`.
- `curl -sL https://github.com/exla-ai/rudder/releases/latest/download/appcast.xml`
  shows the new version and build, and its enclosure's `length` equals the
  DMG's size.
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

- Delete `/workspace/rudder-scratch/release/`.
- Update `docs/v4-handoff.md` if the release changes what is open.
- Send a short final message covering:
  - what shipped, with versions;
  - each verified fact (npm 200, image, Fly release, Mac release, site);
  - anything still waiting, such as a home that updates when idle, or an
    Offstage job stuck in the queue.
