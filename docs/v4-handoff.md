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

## 8. Disk

Cleaned 2026-11-04: Fly qualification captures, old scratch dirs and stale jj
workspaces are gone (`/workspace` 85%, 22G free). Old qualification `*.py`
scripts moved to `/workspace/rudder-scratch/old-py/`. The shared cargo target
`/workspace/rudder-build/target` (16G) is kept as the build cache.
