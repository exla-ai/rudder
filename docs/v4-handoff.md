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

After 4.0.0 (2026-11-04): files moved into a fork keep the executable bit;
`rudder merge` (and the dashboard `m`) runs the rudder.toml check inside the
workspace first (`--skip-checks` overrides); `rudder ls` no longer shows zero
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
to external mode on `rudder-workers-exla:4.2.0`. The old Fly machine and
volume are kept until the user says to destroy them. Agent scratch now lives
in `/workspace/scratch/<run>` and is cleaned hourly. Release worker images:
`node scripts/worker-image.mjs <version>` after npm publish (about 2 minutes,
no Rust compile; see cloud/README.md).

Cloud link clicks (2026-11-05, not released): a click on a link in a
`rudder cloud` pane ran `xdg-open` on the home, which has none. The dashboard
now asks the attached laptop to open it (engineering reference "Clickable
links"). Takes effect once the home runs the new dashboard (worker image) and
the laptop runs a `rudder` release with `src/link-open.ts`.

## 8. Disk

Cleaned 2026-11-04: Fly qualification captures, old scratch dirs and stale jj
workspaces are gone (`/workspace` 85%, 22G free). Old qualification `*.py`
scripts moved to `/workspace/rudder-scratch/old-py/`. The shared cargo target
`/workspace/rudder-build/target` (16G) is kept as the build cache.
