# Rudder v4 handoff (2026-11-04)

How to work: see "Development speed" in `AGENTS.md` (main only, targeted
checks, CI only at release, no new status docs, no hardening waves).

This is the single entry point for whoever picks up v4 next. It reconciles
the status of every agent from the parallel plan `plan-20181-0`, says which
doc owns what, and lists what is open. Nothing is running right now: all
workers are merged or cancelled, the planner run is deleted, and the
hardening follow-up queue was emptied (backup:
`/workspace/rudder-build/hardening-queue-backup.json`).

## 1. Which doc is authoritative (locked 2026-11-04)

Only two docs steer work. Everything else is reference or evidence.

| Doc | Role |
|---|---|
| `PLAN.md` section 0 | **The locked direction.** Overrides the rest of PLAN.md. |
| `docs/v4-handoff.md` (this file) | **Current status and open work.** Update it, do not start new status docs. |
| `PLAN.md` sections 1-20 | Background design. Read the relevant section only. |
| `docs/engineering-reference.md` | How things work; release workflow. |
| `docs/architecture/v4-release-readiness.md` | E0-E14 gate detail (written before the scope cut). |
| `docs/architecture/*.md` (implementation, experiments, research and about 30 more) | Background reference. Do not extend. The JSON receipts and `examples/*/evidence` were deleted 2026-11-04; they are in git history before that date. |
| `DECISIONS.md` | Auto-written run log. Skim only. |
| `/workspace/rudder-v4-restart-handoff.md` | Stale, outside the repo. |

## 2. Where main is (step 9, 2026-11-04)

Collapsed sidebar sections (review, done, closed) expand their agent rows in
the sidebar when clicked or opened with Enter. The worker pane continues to
show the selected agent. Click the header again or press Esc to collapse.

**4.0.0 shipped 2026-11-04.** npm `latest` is 4.0.0 for `@exla/rudder` and
`@viraatdas/rudder` (tag v4.0.0, commit 500e5c3b, all release checks green on
Linux, macOS arm64 and Intel). Cloud control plane deployed 16:25Z running
worker image `registry.fly.io/rudder-workers-exla:4.0.0`; homes adopt it when
idle. The image was built before the last macOS test-only fixes; Linux
behavior differs only in the migration path-spelling fix.

PLAN.md section 0 steps 0-8 are merged. Step 9 results:

- Full suites green: Node 943/943, TUI 59/59 (one load flake passes alone),
  cargo workspace, Cloud build, release build.
- Real sessions (Claude and Codex, local CLI) on a Python repo and a Node repo
  with dependencies. Forks start in about 0.1-0.3s and use about 40K of disk
  against 23M for a full checkout. Agents see current code plus warm
  `node_modules`/build output, run tests inside the fork, and merge on their own.
- Fixed during step 9: CLI workers never integrated; forks used the snapshot's
  stale commit; headless backends ran outside the fork mount; login shells
  inside forks picked stock jj; auto-integration raced the plan scheduler.

Agent context: the selected agent card and diff panel show the latest backend
request input, cached/fresh/cache-write breakdown, and Codex context capacity
when reported. This is a request snapshot, separate from cumulative billing;
Claude capacity and prompt/file/tool categories are not inferred. Usage is
read off the UI thread and refreshed every ten seconds.

Merge conflicts: a run someone meant to merge no longer waits on a conflict.
That is a planned node nobody has sent a follow-up to, or a run the user
pressed `m` on (recorded as `mergeRequested` in run.json, cleared by a new
prompt). The dashboard merges it again against the latest main change off the
UI thread, one per project at a time (the rest queue), settles mechanical
files, and starts the resolver in its row for what is left. Such rows found in
`merge-conflict` at startup are picked up the same way. Two resolver attempts
(counted from `resolver` turns in run.json), then "needs you" with the reason;
`m` grants one more. Any other conflicted row (a dashboard worker that
conflicted under older code) shows "conflict · press m to resolve" and is never
merged on its own. Automatic
integration no longer rebases or runs checks before merging.

Selection speed: moving the selection to a busy agent now shows its current
screen on the next frame. The selected pane drains its whole output queue
before drawing (it used to replay up to 256 KB one 64 KB batch per frame), and
input is handled before the poll, so a PTY wake is no longer swallowed (that
cost a 33 ms tick). Key-to-frame on the host was already about 1 ms; on a Cloud
home the rest is the network and the home relay (`home.mjs` blocks its event
loop for about 80 ms every 5 s while it spawns the preview scan).

Linux fork creation: moving a fork from its snapshot to the base used three
jj processes per differing file (182 s for a 9,294-file branch fork of
libra). It is now one Git two-tree checkout (`git read-tree --reset -u`) with
a throwaway index, run through the mounted overlay, with checkout conversions
and repo hooks turned off (needs Git 2.42+ for `--attr-source`, else a full
checkout). Synthetic repo: 4,000 files 55.8 s to 0.6 s; 9,300 files about
1.2 s. A launch now uses the ready snapshot nearest its base (not the newest),
starts a warm build for a branch base too, and waits up to 90 s for that build
when every snapshot is over 500 files away. Conflicted bases get a full checkout.

Workspace preparation no longer freezes the dashboard. A new task, a plan
node, `b` (branch) and `/handoff` add their row at once as "preparing
workspace"; `rudder __launch-node` and the Claude transcript copy run on a
background thread (one at a time, as before), and the App loop starts the
agent when the result arrives. Keys typed into the pane meanwhile reach the
agent once it starts; `x` or `dd` cancels and the workspace is removed when it
finishes; a quit drops the row and leaves any late workspace to the orphan
sweep. `/gam` still prepares its generator's workspace on the loop. On
2026-11-06 a 182s `__launch-node` froze a libra dashboard for its whole run.

Warm snapshots follow trunk (src/warm-snapshot.ts header). One detached
builder per project (started by launches and clean integrations, exits after
30 idle minutes) polls the jj op heads and builds a snapshot whenever
`currentBaseCommit` moves; quick moves coalesce, and after a refresh a small
move waits 60 s (or as long as that refresh took) while a move over 500 files
builds at once. Linux refreshes are a thin OverlayFS layer over the nearest
snapshot's layers: the builder mounts the stack at the fixed path
`.warm/build`, applies the base change with the same Git two-tree checkout and
runs the warm commands there, so outputs are reused at the paths they were
made at. `.warm/<key>.lowerdir` lists the stack; the fork enter script mounts
it and binds the fork at `.warm/build` so paths baked into build output
resolve. At 8 layers the next refresh copies the merged view into a plain
directory. rudder-jj's overlay snapshot reads `.gitignore` through the stack.
macOS refreshes are APFS clones of the previous snapshot (not run on a Mac
yet). Branch launches no longer build: they fork the nearest base snapshot
and patch. A project's first launch waits up to 90 s for its first snapshot.
Pruning keeps the newest snapshot plus every layer a kept snapshot or a fork
(`.fork/lower` or its current lower) mounts. New forks link trees under
`.warm/lower-links`, so forks with the old one-layer enter script never
resolve a tree to a stacked snapshot. Synthetic pnpm project (3,000 files,
Next/React/AWS SDK deps, incremental tsc, 425 MB of node_modules and dist):
a 10-file base move is ready 2.6 s later; the refresh itself takes 1.6-1.9 s
against 1.8-2.1 s from scratch and adds 0.9 MB of disk against 427 MB; a fork
of it takes 0.09 s; compaction took 2.1 s.

After 4.0.0 (2026-11-04): files moved into a fork keep the executable bit;
`rudder merge` (and the dashboard `m`) merges first and then runs the
rudder.toml check once inside the workspace, reporting a failure without
undoing the merge (`--skip-checks` skips it); `rudder ls` no longer shows zero
phase times for headless runs; `LocalStore::gc` refuses stores that hold jj
objects. Fork commits stay in Git on purpose: a fork is a workspace of the
user's colocated repo, so it shares that repo's backend.

macOS agents now get forks too (src/warm-snapshot.ts `createCloneWorkspace`,
`rudder-jj rudder-clone-add`): warm snapshots build locally as on Linux, and
each agent workspace is one APFS `clonefile` of the snapshot that keeps its
recorded jj file states, then checks out the base (only differing files are
written). It is a plain local jj workspace, so nothing runs through `enter`.
On a 5,000-file Node repo a fork takes about 0.14s against 1.25s for a full
checkout, with `node_modules` already there. Another volume or a failed clone
falls back to a full checkout; there is no byte copy. Codex agents launched
from TS now get the tool cache through `shell_environment_policy.set`, since a
user policy like `inherit = "core"` hid it from Codex's shell; native Codex
launches (native/src/launch.rs) pass the same `-c` arguments. A Mac warm build
only starts with 10 GB free; a project opts out with `[warm] commands = []`.

Shipped in 4.2.2/4.2.3 (2026-11-05; the v4.2.1 tag never published: the
macOS clone change broke the Linux-only rudder-jj fork_add, which a Mac
build does not compile). Control plane v187 runs worker image 4.2.3; the
home adopts it on its first idle window.
- Enter on a finished agent's worker pane resumes or reopens it, like Enter on
  its row; ⌥h full screen animates (about 200ms, 30 fps, PTY resized once).
- Predictive echo no longer turns itself off at normal typing speed (keys
  typed during a hold were re-arming it) and recognizes Rudder's own task
  hint. Emulated at production delays, per-character lag at 40-160ms gaps went
  from about 193ms to 2ms (`node scripts/cloud-perf.mjs`).
- Typing in a read-only `rudder cloud` terminal takes control instead of
  dropping the keys (a slept laptop's dead socket kept control for up to 30s).
- Measured but not changed: the control plane machine runs in iad although
  fly.toml says sjc, so every keystroke crosses the US twice (about 180ms of
  the 190ms echo). Moving it west is the largest remaining win; its S3 buckets
  are in us-east-1. Attach also replays up to 256KB that the repaint it asks
  for makes redundant.

Dashboard look for 4.2 (native render.rs/theme.rs): agents are cards (title,
then a colored status line with elapsed time and +N -N; the selected card is a
rounded outline), a header row (wordmark, project, branch, version), a
one-line prompt pill, a footer with the keys for the current pane and `?` for
all keys. Motion only while something moves (about 11 fps cap): a 600ms
startup reveal any key skips, a shimmer on working cards, a short glow when a
card changes state. Notices show as a pill and then settle into the footer and
never resize the prompt. Pill words and footer keys use the terminal's own
text color (`term_fg()`, `Color::Reset`), so they read on a light, dark or
unreported background; severity shows in the glyph and border. The faint grey
is for hairlines and separators only, never words (theme.rs has a contrast test).

## 3. Status of every agent

Plan lanes (all merged locally):

| Node | Area | Result |
|---|---|---|
| n0 | Single-owner real Fly qualification | INCOMPLETE. Held by user rule (no Fly runs or remote builds without approval). SDK/runtime/ordinary stages never reached on Fly. |
| n1 | Local SDK end to end | Done. 7/7 real scenarios via `scripts/test-sdk-helper-local.sh --real`. |
| n2 | Compiler isolation without Landlock | Done, 11 kernel adversarial tests. |
| n3 | Engine restart recovery, normal drain | Done, 14 recovery + 31 durability tests. |
| n4 | Enforced managed compiler cache | Not qualified. Reuse later disabled fail-closed (harden-4, harden-9). |
| n5 | App owner startup/shutdown | Done, 9 lifecycle tests. |
| n6 | Existing-user migration and rollback | Done. |
| n7 | Real model profiles | Done. |
| n8 | Public command surface (45 ops registry) | Done; E11 caller migration incomplete. |
| n9 | File/document viewer | Done, 27 tests. Real-browser check pending. |
| n10 | Agent efficiency, scoped memory | Code done; E14 has zero real samples. |
| n11 | Automatic planning, durable tasks | Done, 95 tests. |
| n12 | Private previews, responsive terminals | Done, 43 + 15 tests. |
| n13 | Integration wiring | Done; two installed-provider tests skipped. |
| n14 | Launch gates / readiness doc | Done. Its "stable blocked" verdict used the full E0-E14 bar, since replaced by the Cloud-first scope. |

Hardening waves (all merged locally):

| Run | Result |
|---|---|
| harden | Partial. Final owner release blocked on machine-drain proof. |
| harden-2 | Attach socket fence, done. |
| harden-3 | Migration wiring, done. |
| harden-4 | Managed compiler execution disabled fail-closed. |
| harden-5 | Preparation binary tests, done. |
| harden-6 | Production cancellation, done (merge conflict resolved). |
| harden-7 | Ordinary SDK reuse fail-closed. |
| harden-8 | Partial, both findings open. |
| harden-9 | Compiler input hardening; reuse still disabled. |
| harden-10 | Runtime producer census. `other_runtime_producers` still Pending. Final additions were merged UNCOMPILED; verify they build. |
| harden-11 | Root/SDK storage retirement + `owner_reconciliation.rs`. 3 retained-host regression tests failed fixture validation before merge; confirm whether fixed or ignored. Production external-work adapters incomplete. A follow-up "Hardening pass (2 findings)" also merged. |
| harden-12 | `managed_machine_drain.rs` RetainedMachineDrain. Final release only yields Pending/Uncertain; needs harden-10/11 types plugged into its seam. |
| harden-13/14/15 | Cancelled before doing work. |

Dropped queued follow-ups (from harden-11): tests for build admission
closure in `compiler_sdk_client`, `SdkProcess::retire_startup_retaining()`
startup retirement tracking, and reconciliation in
`engine_host_ingress_acquisition.rs`.

Other runs: TestFlight session, morning digest and sign-out merged;
cloud-sync paused; `main-branch-jrqtg` (Sol) completed; `ps` cancelled
(accidental launch); planner `finish-rudder-v4-plan-fucki` deleted.

## 4. Why this went off course

- Hardening waves close partial subsets ("fail-closed", "remains Pending")
  and each spawns more follow-ups. Depth limit 3 per node still allowed 15
  hardening runs. More local hardening cannot reach stable.
- The stable gates that remain need things local work cannot provide:
  E2 (two real Fly workers), E5/E10 (replacement worker restore), Mac
  hardware gates, real-model samples, E14 efficiency samples.
- Every Rust check goes through one shared cargo queue (one big rustc on
  8 cores). About 6 minutes wall time for a test that runs in 0.25s.
- Fly qualification ran ~10 remote builds for 5 revisions over a day
  before the user stopped it.

## 5. Two hard limits shipped on purpose

1. Managed compiler cache reuse and managed Buck2 execution are disabled
   until writer isolation is enforced.
2. Managed runtime producer census is Pending, so final managed owner
   retirement cannot certify release.

## 6. Decisions the user needs to make

- DECIDED 2026-11-04: the plan is PLAN.md section 0. 4.0.0 ships after
  step 9. The user approves the release and Cloud deploy.
- Whether to approve the one optional Fly campaign (single retained owner,
  SDK + runtime + ordinary on one image) described in the readiness doc.
- Cloud deploy (worker image + Fly control plane) needs explicit approval.
- `origin/rudder/mac-pty-diagnostic`: 7 unmerged PTY/broker commits from
  10-30, about 210 behind main. Needs a Mac to verify; merge or drop.

## 7. What gets built

The plan is `PLAN.md` section 0 (steps 0-9). One test gate only, at step 9.
No evidence files, no automatic follow-ups, no prereleases or Cloud deploys
before step 9.

4.1 Cloud capacity is merged onto main (2026-11-04) from
`cloud-capacity-build-pool`; that branch can be deleted. Node 1025/1025 and
the rudder-native cargo suite pass. Not yet released or deployed; see
PLAN.md section 0 "After 4.0.0" for what remains.

Linear (2026-11-05, live on the control plane): the Exla workspace is
connected (Libra, Offstage and Rudder projects linked). "Generate tasks" turns
a goal into 1 to 10 `rudder`-labeled issues (OpenAI `gpt-6-luna`), and the
webhook starts an agent per labeled issue. Rudder also tracks the user's own
work: a plan gets one issue that its workers and branches share, closed after
the last worker merges; a main row (one long session) is judged per user
turn, at most one model call a minute, and never auto-closed. Each piece of
work attaches to a related open issue, creates one, or is skipped when it is
not real work ("summarize the README", "thanks"). Main-row turns and plan
close counts need the 4.2.3 worker image on the home (engineering reference
§9.8.2). Never create test issues in the real workspace.

4.2 external home host (2026-11-05): `rudder cloud host <ssh-target>` runs
the home on any Linux box with Docker (engineering reference §9.6). The
user's home moved off Fly to an AWS m8azn.6xlarge (us-west-2, 5GHz, 24 vCPU,
92GB, 500GB gp3) as a stopgap until a bare-metal box is ready. The disk was
copied under the 4.1.2 image with the Fly machine stopped (`init.exec` set to
sleep); on 2026-11-05 after the 4.2.0 deploy, `rudder cloud host` switched it
to external mode on `rudder-workers-exla:4.2.0`. The old Fly machine
(8edd225a7d0378) and its 150GB volume were destroyed on 2026-11-05 at the
user's request; Cloud never calls Fly for an external home. Agent scratch now lives
in `/workspace/scratch/<run>` and is cleaned hourly. Release worker images:
`node scripts/worker-image.mjs <version>` after npm publish (about 2 minutes,
no Rust compile; see cloud/README.md).

Fly cleanup and builds (2026-11-05): the failed home-223b (still running on
Fly), 26 stale v2/v3 workspaces, 14 old sails and 24 orphan volumes were
deleted through the control plane's delete reaper and `flyctl`; the empty
`rudder-v4-qualification` app is gone; the karna-montage v3 workspace is
stopped. Left on Fly: the control plane, three stopped
homes of other accounts (two people and the App Store review account) and the
stopped dad-job-apply workspace. The burst pool stays off (an external host
cannot reach Fly's private network); `rudder-burst` now runs the command on
the home instead of failing. 4.2.4 ships sccache in the worker image with
per-workspace keys; 4.2.5 keeps the tool cache (and CARGO_HOME) where it was,
since a moved CARGO_HOME rebuilds every dependency. Cloud runs the 4.2.5 image
(2026-11-06); these reach the home when it next updates while idle. Warm snapshots were rebuilt by hand for the
jj projects with something to build (exla-ai-rudder, libra, offstage,
vibeprint; viraat-gazette needs an uncommitted data file to build); an earlier Rudder one failed inside an old sccache
binary. Repos without root lockfiles (smol, snap, raindrop, ...) get a plain
checkout, and non-jj repos get a snapshot after their first worktree agent.

Update now (2026-11-06, 4.2.6): when Cloud runs a newer worker image than
the home, its dashboards show "update" in the footer, and Alt+r twice updates
the home at once, busy or not (engineering reference "Update now"). Agents
that were mid-turn reopen with one "continue where you left off" turn. The
worker image also carries headless Chromium's libraries now.

Stuck home updates (2026-11-06, for 4.2.9, not released): home-53c2 stayed
on 4.2.0 because its host's `docker login` expired, the pull failed, and the
pending replacement token then refused every Alt+r and idle update forever.
Now a pending token stops blocking after 20 min (control plane only, so the
4.2.0 home unblocks once Cloud deploys), the host writes why it failed next
to `desired.failed.json`, and 4.2.9 homes report it so Cloud clears the token
at once and the footer shows the reason (engineering reference 9.6). Still
on the user: log home-53c2's host in to registry.fly.io again, then press
Alt+r; rerun `rudder-host-setup` there to get the new apply script. Open
decision: the hand-made, expiring registry login. Recommended: also push each
worker image to a public registry (ghcr.io; the image holds no secrets and the
repo is public) and point external hosts at it, so hosts need no login at all.

Public worker image (2026-11-06, for 4.2.9, not released): CI now pushes every
release's worker image to the public `ghcr.io/exla-ai/rudder-worker:<version>`
(`.github/workflows/worker-image.yml`, on `v*` tags after npm serves the
version, or by hand with `-f version=X`). New external hosts run it with no
login. Cloud sends an external home the new tag in the repository it already
runs, so GHCR hosts update with Alt+r and old registry.fly.io hosts behave as
before. To move home-53c2 off the expiring Fly login (on its host, once 4.2.8
is public there): `sudo docker pull ghcr.io/exla-ai/rudder-worker:4.2.8`, then
`echo ghcr.io/exla-ai/rudder-worker:4.2.8 | sudo tee /etc/rudder/image` and
`sudo rudder-home-start`.

Fewer checks (2026-11-06, not released): the user wants agents to test and
use CI sparingly. Worker prompts now say "run one quick check for what you
changed; add a test only for a real bug or tricky logic", the default
`Done when:` is "the task is implemented and works as asked", and planners
name at most one targeted check in `success`. AGENTS.md keeps the full suite
for releases only.

Cloud link clicks (2026-11-05, not released): a click on a link in a
`rudder cloud` pane ran `xdg-open` on the home, which has none. The dashboard
now asks the attached laptop to open it (engineering reference "Clickable
links"). Takes effect once the home runs the new dashboard (worker image) and
the laptop runs a `rudder` release with `src/link-open.ts`.

## 8. Disk

Cloud build speed (2026-11-06): inspected the running external home: 24 CPUs,
92 GiB RAM, no visible CPU quota, and ample idle capacity. The scheduler's
parallel limit is already 1000; it was not throttling agents to the old home.
The local `/workspace/rudder-build/rcargo` still forced six build jobs and both
build wrappers checked an absent sccache path and used an 800 MB cache budget.
Both wrappers now use `cloud/host/build-resources.mjs` to size builds from CPU,
memory and cgroup limits (22 jobs here), preserving explicit job overrides.
They discover sccache on PATH and use the agent tool cache with a 20 GiB default.
The image-pinned sccache 0.18.0 was missing from the running container; installed
its checksum-verified binary in `/home/rudder/.cargo/bin` without restarting
agents. A real Rust compile probe produced a miss followed by a hit. The shared
Cargo target lock remains intentional. These wrapper edits are local host setup,
not tracked product files; keep the helper available at the current checkout path.
Native and headless prompt guidance now asks agents to inspect current host
limits and wait on commands directly while reporting progress. Existing sessions
retain their prompts; updated prompt guidance needs a later build/install.
The observed 50-second sleep loop was registry-release polling, not a scheduler
delay. No model preferences changed. These changes are included in the combined
4.2.8 release candidate; publication and Cloud adoption must be verified separately.

Cleaned 2026-11-04: Fly qualification captures, old scratch dirs and stale jj
workspaces are gone (`/workspace` 85%, 22G free). Old qualification `*.py`
scripts moved to `/workspace/rudder-scratch/old-py/`. The shared cargo target
`/workspace/rudder-build/target` (16G) is kept as the build cache.
