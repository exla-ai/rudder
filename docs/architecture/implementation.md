# Rudder v4 implementation and restart record

Updated: 2026-10-30. Read with [the plan](../../PLAN.md),
[research](research.md), [gates](experiments.md) and repository `AGENTS.md`.
This file preserves the working agreement across context compaction. An entry
here is not deployment proof; record exact checks and delivery identities below.

## Authorized scope and accepted direction

The user authorized implementing the plan and shipping to main, with tighter
Tailscale integration and smoother Cloud terminal text/buffering. The full
rewrite is Rudder v4 and spans multiple engineering milestones. Preserve the
main checkout and unrelated changes; do not restart live production agents to
force image adoption. Follow the version/tag/CI npm workflow and the separate
Fly worker/control-plane workflow.

Accepted architecture:

- Rudder owns immutable source/environment roots, persistent workspace metadata,
  private writes, checkpoints and distributed CAS/hydration. Qualify actual
  materializers before selecting or forking a VFS implementation.
- Keep jj history/conflicts/Git exchange behind a pinned adapter; a narrow fork
  is acceptable if required. CoW alone does not require a new VCS.
- Prefer a supported Buck2 Cloud profile, with REAPI interoperability for Bazel.
  Stock Buck2 does not automatically share one graph across divergent branches.
- One persistent Rust machine engine owns execution; SQLite is local journal,
  transactional project authority is the fleet truth. Fenced ownership and
  version-checked integration protect source and accepted knowledge.
- Direct durable agent mailboxes, first-class Codex app-server and Claude
  Channels adapters, authenticated provenance, active steer/interrupt/resume and
  explicit acknowledgments. Messages and accepted decisions are distinct.
- Native Mac remains supported. Filesystem/source snapshots, build actions,
  conversation forks and process-memory snapshots are distinct contracts.
- Ordinary input chooses discussion, direct work or decomposition automatically.
  Decisions is an optional bounded classifier, not a planner or authority.
  Sunset standalone `/gam` and `/nudge`; retain `/plan` only as compatibility.
- Private previews reuse Tailscale identity and live project/process attribution.
  The existing relay remains available; direct networking does not replace
  authorization. Responsive terminal transport has bounded queues/recovery.

## Current implementation track

Base: `91579cf631b1d9d78afbbd786a6dc1f8120cc1ca` on `origin/main` at start.
All edits are in the user's main checkout. No product edits predated this track;
the three architecture documents already contained this session's planning work.

| Track | Concrete scope | Gate and state |
|---|---|---|
| Planning/commands | Shared deterministic routing with an optional Decisions helper; read-only intent; one Cloud ingress; retired public pair/nudge entries | E11/E12 subset; implementation and regression verification underway |
| Private previews | Persisted Tailscale login, actual readiness, bounded async probes, project-scoped ready links and local open | E13 subset; deterministic and real loopback tests underway |
| Terminal stream | Shared home/sail helper, short output batching, bounded backpressure, connection fencing, UTF-8 and repaint recovery | E13 subset; real WS/PTY tests pass; production verification pending |
| Startup/download caches | Background CLI update, persistent account-local npm/pnpm/uv cache defaults, explicit overrides retained | Behavior tests pass; no distributed action-cache claim |
| Full storage/engine rewrite | Milestones below | Planned, not implemented by these changes |

The current native simple-execution path remains an isolated worker; explicit
main-checkout mode remains available. The initial automatic DAG path retains
the existing approval gate. This first change does not establish the eventual
single persistent task owner through every v4 engine operation. The standalone
TS board/CLI and old installed clients need separate migration checks.

Home web/phone task ingress uses native routing. Legacy task sails have a
different finite TS lifecycle: discussion/planning fail closed with a
home/dashboard alternative until that adapter migrates. Starting a permanent
dashboard in its place without task identity/approval/completion tracking would
misreport completion. Local `/cloud` task forwarding still reaches that legacy
path, so this is a stated alpha limitation, not complete Cloud routing parity.

## Dependency-ordered continuation

1. Finish and ship the current verified track with actual npm/Fly identities,
   installation/runtime smoke checks and a truthful rollout record. Reserve a
   complete stable-v4 claim for the full launch gates.
2. Reproduce E0 baselines on representative TS/Rust, dependency-heavy JS, Python
   and native Apple workloads. Publish workload manifests and raw samples.
3. Ratify serialized source/environment/checkpoint/command contracts, durability
   levels and ownership transfer. Build filesystem/crash/recovery fixtures first.
4. Qualify E1/E2 candidate materializers on disposable Linux views and two Fly
   workers; compare entry-count/byte scaling, copy-up, lazy hydration, publication
   pins and GC races. Do not mount experiments over user checkouts.
5. Extract the persistent Rust engine with one authority flag and legacy attach
   adapter. Prove surface-close survival, restart deduplication and old-owner
   quiescence before giving it scheduling control.
6. Implement durable roots/CAS, private workspace views and checkpoint export,
   then remote publication/retention/restore. Meet E5 fault and filesystem gates.
7. Convert one canonical build workload to the Rudder Buck2 profile, compare
   Bazel/REAPI, prove action-key invalidation and measure completed builds.
   Branch-aware graph/compiler reuse is a separate qualified extension.
8. Extract/generalize adapters and add transactional mailboxes, decision/topic
   versions, scope authority and active peer controls. Prove E6 races and real
   Codex/Claude behavior; PTY output is never an acknowledgment.
9. Embed/bridge pinned jj, implement fenced exact-tree/version integration and
   migration/export/undo. Complete E7/E8, including offline Mac behavior.
10. Add cache-aware fleet placement, migrate each surface to engine projections,
    retire duplicate schedulers/file fan-out/command machinery only after caller
    migration, and complete E9/E10/E11 recovery/scale/release drills.

Workstreams may overlap after shared contracts and dependencies are stable.
Keep one editor per file and one integration/release owner. Do not let parallel
implementation turn independent journals into competing project authorities.

## Verification and delivery record

Current checks: root production build, Cloud TypeScript build and typechecking
pass. Final Node suite: 478 passed, 8 skipped, 0 failed (486 total); native suite:
859 dashboard tests plus 19 library and 1 terminal-color integration test pass
(10 documented ignored tests). Persistent cache/boot behavior, shared terminal
and private-preview fixtures pass. A 1,000-write burst coalesced into one WS
frame in the loopback fixture; this is not a production latency measurement.
Real release-binary terminal end-to-end suite: 53 passed, no failures/skips.
It exercises natural automatic planning through approval/workers/merge,
discussion scope and command retirement. Final Node suite includes the added
legacy sail guard. Packaging and deployment are pending.

Before closing this implementation track, replace pending entries with exact
revision, version, CI workflow result, published npm dist-tag, worker image
digest, Fly release/deployment identity, canonical URL, UTC verification time,
checks and active-home adoption state. A pushed commit is not a Cloud rollout.

Release preference was asked asynchronously: v4 prereleases on `next` versus
3.x foundations or waiting for all gates. No answer arrived during implementation;
proceed with the stated recommended v4 prerelease assumption.
An alpha must not move npm `latest` or masquerade as complete stable v4.
