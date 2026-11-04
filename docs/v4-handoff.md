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

## 2. Where main is

- Local `main` holds every merged worker. Published: `v4.0.0-alpha.34`
  (origin/main `368c72f2`). package.json is still `4.0.0-alpha.34`.
- Gate rerun 2026-11-04: check and Cloud build pass. The 5 earlier Cloud
  timeouts passed (load flakes). One real Node failure:
  `tests/cloud-web-app.test.mjs:221`, a home row with status "deleting"
  disappears after a passive `/api/app/state` poll. Cargo, build and TUI
  results pending.

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

- DECIDED 2026-11-04: ship 4.0.0 Cloud-first from what is built, Mac work
  in 4.1, build caching per PLAN.md section 0. Open: confirm `latest`.
- Whether to approve the one optional Fly campaign (single retained owner,
  SDK + runtime + ordinary on one image) described in the readiness doc.
- Cloud deploy (worker image + Fly control plane) needs explicit approval.
- `origin/rudder/mac-pty-diagnostic`: 7 unmerged PTY/broker commits from
  10-30, about 210 behind main. Needs a Mac to verify; merge or drop.

## 7. Scope and order

4.0.0 = Cloud on Fly, Linux, what is built today (see PLAN.md section 0).

1. Full gate green on main (running 2026-11-04). Classify the 5 Cloud test timeouts.
2. Rewrite `v4-release-readiness.md` for the Cloud-first scope.
3. Release 4.0.0 to `latest` (npm version, release-check CI, push main + tag).
4. Cloud worker image + Fly deploy, with user approval.

4.1 headline, in order: wire overlay workspaces into agent launch, warm forks
from a trusted builder snapshot, one Rudder cache per machine for tool-native
caches, REAPI for Buck2/Bazel repos, then cross-machine cache (E2, one Fly run)
and Mac work.

Product follow-up: Rudder's own scheduler auto-queues hardening follow-ups
(`.rudder/hardening.json`, `MAX_FOLLOWUP_DEPTH = 3` in `native/src/main.rs`).
That is what produced 15 hardening runs. Consider turning it off by default.

## 8. Disk and leftovers (ask before deleting)

`/workspace` is 89% full (17G free).

- `/workspace/rudder-build` 15G (shared cargo target + coordinator scripts).
- `/workspace/rudder-one-capture-*` about 11 x 900M, plus
  `rudder-one-capture-controller-context-*` 6 x 255M (Fly qualification
  evidence).
- `/workspace/rudder-genuine-*`, `rudder-runtime-qualification-*`,
  `rudder-alpha34-delivery-20261103` (500M), about 77 loose `*.py`
  qualification scripts in `/workspace` root.
- `/workspace/rudder-scratch/{coordinator,harden-11,harden-12,n13,n13-model-receipts,n14}`.
- 33 jj workspaces, almost all for merged runs (`jj workspace list`).
