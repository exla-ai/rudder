# Rudder v4 experiments and decision gates

Status: specification, 2026-10-30. No experiment results are claimed here.
Supports [PLAN.md](../../PLAN.md) and [the research evidence](research.md).

## Measurement contract

Report raw samples, hardware/OS/kernel, source revision, backend versions,
configuration, region, storage/network placement, concurrency and cache state.
Use synthetic fixtures for mechanisms and real repositories for outcomes.
Keep all existing user checkouts and production agents outside experiments.

Separate:

- Command acceptance: durable receipt, intent, idempotency key and owner epoch.
- Workspace ready: source/environment accessible and first real shell/tool can run.
- Agent ready: actual supported CLI/session can accept its task.
- First useful command: command has completed; it is not merely queued.
- First completed build/test: valid action result or successful execution under
  the declared profile, including deferred reads and artifact materialization.
- Accepted change: reconciled, verified exact tree landed at the integration owner.

Record p50/p95 for sufficiently repeated trials. Report sample counts and ranges;
do not publish a p99 from a handful of runs. Compare matched baselines, not a
warm new system against a cold legacy one. Separate mocked protocol load from
real agent throughput and real provider quotas.

Initial target hypotheses, to revise after E0:

| Metric | Initial hypothesis, not measured |
|---|---|
| Warm virtual workspace creation | p95 <=250 ms on a designated worker/corpus, excluding CLI/model startup |
| Cold-worker view readiness | p95 <=2 s within one region for the designated corpus; report metadata bytes and first-command stalls |
| Unchanged inherited data per fork | No bulk file-data copying; report all metadata, mount, journal and scratch allocation |
| Repeated compatible build | At least 50% lower median end-to-end time than the matched current workflow on selected cacheable workloads |
| Cross-worker cache benefit | Demonstrable first-build gain after all downloads; transferred data follows the accessed working set rather than full environment size |
| Correctness | Zero invalid hits or lost acknowledged replicated checkpoints in the defined fault matrix |
| Coordination | Every retained addressed event is replayable; explicit receipts remain distinct from agent adoption/compliance |

Physical bytes, peak RAM, CPU-seconds, remote traffic and dollars are co-equal
outputs. There is no universal per-agent memory or disk bound.

## E0. Reproduce current costs and choose representative workloads

Run current Rudder launch/open/resume/build paths and instrument the critical
path. Measure dashboard spawn, agent CLI initialization, workspace creation,
dependency installation, output/state reading, model metadata and first test.
Reproduce or update the earlier 17 MB/1.6 GB workspace observations.

Corpus:

1. Rudder itself: Rust native crate plus TypeScript, Node tests and UI bundle.
2. A dependency-heavy JS monorepo with many small files and native packages.
3. A Python project with an explicit environment and test inputs.
4. A native Mac/Xcode workload on real Mac hardware.
5. Generated stress trees varying bytes independently from entry count.

Control cache states: same process warm, new process warm disk, empty local disk
with warm regional store, completely empty store, and offline with pinned roots.
Record filesystem/page-cache state; clearing caches is permitted only on dedicated
test workers. Output a baseline report and fixed reproducible workload manifests.

## E1. Cheap forks and isolated writes

Compare native materialization, Linux OverlayFS, Nydus view/overlay, a
Buildbarn-derived view with copy-up, and JuiceFS clone where feasible. Include
current BranchFS as a diagnostic comparison, not an assumed fast baseline.
Use APFS cloning and the candidate lazy backend on Mac.

Prepare a built environment once. Create 1/10/100 views with equal contents and
vary tree sizes/entry counts. Measure metadata work, physical copied bytes,
mount latency and memory. This is a view-capacity test; do not call 100 views
100 running coding agents.

Run independent edits and builds. Verify parent/sibling isolation, concurrent
reads, pinned inherited roots and shared immutable data after eviction/remount.
Reject accidental shared writable dependency/build/Git state. On APFS verify
that cloning occurred and report fallback separately. Test source/destination
on different filesystems explicitly.

Exit: the chosen fast backend avoids inherited-tree byte copies, has acceptable
entry-count scaling, and passes isolation/normal-tool behavior. If no backend
meets the goal, specify the missing VFS primitive before starting a custom one.

## E2. Distribution, lazy reads and publication

Use two independently provisioned Fly workers, not two processes sharing a
local volume. Publish a prepared root on A, restore on empty B, and run actual
builds/tests. Repeat with changed source and unchanged dependencies, changed
dependencies, changed toolchain, and a partial remote cache.

Measure metadata startup, data fetched before readiness and before first build,
remote request counts, read amplification, prefetch, decompression and CPU.
Test many tiny files, a few large files, deep trees and mmap-heavy tooling.
Compare packed metadata/chunks with naive per-file object fetches.

Kill or partition builders during upload and root publication. Inject duplicate
uploads and digest/size mismatches. Explicitly enable and verify integrity checks
in each candidate; feature support alone does not prove checks are enabled.
Race staging retention, publication,
cache eviction and GC. Verify that no published root references missing data,
new roots are protected from concurrent sweeps, and adapter-specific references
are traversed. Test quota exhaustion and cancelled preparation owners.

Exit: independently reconstructible replicated roots, bounded redundant work,
and a real cold-worker time-to-build benefit. Mount/metadata readiness alone
cannot pass this gate.

## E3. Build contract and action-cache soundness

Compare representative workloads under Buck2 and Bazel with matched pinned
toolchains and REAPI services. Track engineering time, unsupported tooling,
analysis/graph reuse across divergent views and onboarding steps. Shared backend
storage must not be reported as cross-engine action hits without matching keys.
Compare selected language-native integrations under their own cache contracts.

For each action, change one input at a time:

- Source file used by the action, an unrelated source file, generated input.
- Dependency lock, manifest, patch, install script and package-manager options.
- Compiler/SDK/runtime/ABI/CPU target and relevant flags/environment.
- Absolute workspace path and declared path-remapping behavior.
- Clock, randomness, network response, environment secret and undeclared file.

Require hits only where output/effect equivalence is justified. Validate actual
outputs against a trusted execution on controlled deterministic fixtures.
Unmodeled effects must reject cacheability. Exercise remote cache poisoning,
wrong tenant namespace, cancelled builds and concurrent identical misses.
Separate preparation keys from action keys and their result manifests.

Compare isolated warm incremental builds, local artifact cache and remote
artifact cache. Record Rust incremental/sccache tradeoffs instead of applying
one global setting. Include full integration/smoke tests that intentionally
cannot reuse an ordinary cached result.

Exit: sound hit/miss matrix, measurable time/cost improvement and an accepted
Cloud repository contract. If conversion burden is excessive, narrow the
supported fast profile or change its action producer; preserve correctness.

## E4. Does warmed execution state earn its complexity?

Compare plain CLI launch, persistent engine plus warm artifact/environment
cache, and persistent serialized build service. Attribute compiler/JVM/tool
initialization separately from actual compilation and network time.

Only if initialization remains material, evaluate process/VM snapshots on a
qualified host or external backend. Include reconnection, refreshed identity,
credentials, time/entropy and process side effects after restore. Current Fly
guests have no nested KVM path established by this work.

Exit: an ADR either deferring memory snapshots or selecting a bounded execution
profile with quantified benefit, host requirements and adoption/license path.

## E5. Filesystem semantics and durability

Exercise rename-over-file/directory, atomic editor saves, truncate, append,
hardlinks, relative/absolute symlinks, executable modes, sparse/large files,
mmap writes, file locks, concurrent readers and held open descriptors.
Test watch overflow/loss and final sealed-tree reconciliation. Confirm ordinary
Git, package managers, compilers, language servers and debuggers work.

Crash the engine or filesystem service after command acceptance, during copy-up,
after local checkpoint, during replication and after root publication. Reconcile
accepted/not-started, started/uncertain and completed commands. Ensure retries
return original receipts and do not blindly duplicate external effects.
Verify acknowledged replicated roots after loss of the original worker volume.
Measure local-only draft loss bounds rather than claiming zero RPO.

On Mac add Xcode, notifications, Unicode/case collisions and unsupported-volume
fallback. Preserve canonical names/modes; refuse incompatible materialization.
Qualify FSKit on each supported OS/API profile or keep it experimental.

Exit: documented semantics/capabilities, deterministic recovery and no silent
source corruption. This gate is mandatory before production workspace use.

## E6. Live communication, decisions and prompt efficiency

For Claude Channels, Codex app-server and supported ACP agents, cover idle,
busy, disconnected, resumed and policy-disabled sessions. Send events with
stable IDs and topic versions; test duplicates, reordered deliveries, expiry,
queue pressure and restart replay.

Verify injection separately from transport. For Codex compare idle `turn/start`,
active `turn/steer` with the expected turn ID, and passive `thread/inject_items`:
passive history changes alone must not count as a woken/processing agent.
Race turn completion against delivery, send multiple events concurrently, and
inject authorization/schema errors and uncertain transport outcomes. Ensure
retries reconcile state and retain event IDs instead of blindly starting turns.

For Claude test custom preview channel activation in interactive/PTY versus
`-p`/SDK profiles, approved-channel capabilities, organization-policy refusal
and missing servers. Measure busy-session grouping/next-turn processing and
verify explicit pending/degraded state when native delivery is unavailable.
Never use PTY activity as a substitute for per-event acknowledgment.

Exercise discover/send/async ask/reply/inbox/ack/await through common tools.
Agents must continue independent work while a request is outstanding; explicit
waits expose deadlines and wait cycles. Verify informational batching, tracked
blockers, and invalidating updates that immediately block stale integration.
Read a referenced immutable peer checkpoint before merge and verify access
scope, source/contract versions and provenance without replaying the peer's chat.

Exercise peer `steer`, `interrupt`, ordered interrupt-then-steer and `resume`
against actively running Codex and Claude sessions. These are required managed
profile capabilities, not a queue-only approximation. Confirm the target turn
actually stops, source edits/conversation survive, the replacement instruction
is applied in order, and the task remains interrupted/paused until continued.
An interrupt or turn-end hook must not mark the objective complete or merge it.

Race controls against turn completion, active tool calls, background child
processes, permission dialogs, stale owner/turn IDs and user stop requests.
For Claude test Ctrl+C's different working/idle behavior: no blind replay may
clear a user draft or exit an idle session. Verify pending/uncertain outcomes,
deduplication and reconciliation instead of repeated cancellation. Test allowed
peer scopes, denied cross-project controls and competing steers without routing
each operation through an LLM or asking the user again for already delegated
authority. Interrupting must not imply rollback of external side effects.

Track separately: durable event, adapter write, explicit model/tool acknowledgment,
and verified behavior. Unrelated PTY output cannot count as acknowledgment.
An acknowledgment followed by incorrect code must fail the relevant gate.
Crash before/after notification or reply delivery and before engine receipt
commit. No inbox file or reply record may disappear before its durable boundary.
Replay out-of-order acknowledgments and verify a missing earlier event cannot
be hidden by advancing the recipient cursor.

Run two contradictory proposals against the same expected topic version.
Confirm authority/provenance and exactly one valid acceptance transition.
Supersede a decision during a worker turn and during integration tests. The
final integration transaction must validate the decision/policy version vector.
Test a consumer affected by an interface it has not yet edited.

Compare static briefing plus retrieved decisions, relevant deltas, and full
context replay. Measure token usage, actual cache usage, distractions, correct
adoption and completed changes. Keep tool schemas/bootstrap prefixes stable
where possible. Use failure categories from MAST to classify mistakes.

Exit: durable catch-up and detectable drift across real supported backends.
Retire live coordination-file consumers only after their replacement passes.

## E7. VCS and Git compatibility

Compare correctly ignored/fsmonitor-enabled current jj, upgraded jj with
colocated child workspaces, and a pinned library/custom-working-copy prototype.
Measure launch, checkpoint, tree import, conflict creation/resolution, branch
movement, source export and undo independently.

Agent tool cases include `git status/diff/log`, commits, branches, rebase,
submodules and worktree discovery. Verify private HEAD/index, branch namespaces
and controlled shared object/ref writes. Test stale workspaces and a failed
import. An ordinary integration checkout must recover without a custom mount.

Exit: choose the smallest VCS extension that meets semantics/performance. A new
VCS proposal needs a separately reviewed conflict/history/interchange/recovery
specification and cost estimate.

## E8. Ownership, integration, offline work and handoff

Race two schedulers, integration requests and surface commands. Partition an old
owner, grant a new epoch, then resume the old process. Receivers must reject its
stale writes and privileged deliveries. Test expired leases, duplicate receipts,
project-authority crash/restart and unavailable metadata storage.

Explicitly quiesce legacy controllers before migration; they cannot be assumed
to obey new epochs. Exercise rollback with existing jj workspaces and run records.
Test exact-head and relevant-decision/policy changes during gate execution.

Copy an agent and confirm independent identities. Move one with replicated
checkpoint/cursors and source-epoch revocation. Test source shutdown uncertainty,
partial transfer, missing blobs, disconnected Mac work and conflicting offline
decisions. Do not report exclusive move while the old owner can still publish
through uncontrolled credentials.

Exit: one effective project integration authority and recoverable source work
through disconnects, migration and rollback.

## E9. Capacity, scheduling and cost

Stage workloads at 10/100/1,000 views and separately at the number of real agents
the allocated CPU/RAM/provider quota can support. Actual 1,000-agent validation
requires its real resources and quota; a synthetic socket test is not that proof.

Vary cache warmth, queue bursts, build concurrency, project affinity and machine
failure. Measure first-build latency, accepted changes/hour, integration queue,
token/message overhead, physical cache growth, CPU/RAM and cost per accepted
change. Verify fairness and bounded queues; avoid cache locality causing starvation.

Exit: published capacity envelope with named workload and cost, plus evidence
that resource admission remains truthful under pressure.

## E10. Product migration and release readiness

Qualify Mac/TUI/web/phone/CLI as projections over the same owner. Surface close,
reconnect and stale screens must not double-start work or lose accepted commands.
Keep completion, local integration, push and deployment statuses distinct.

Run upgrade and rollback drills, restore published roots on a replacement worker,
export source without a daemon and import legacy sessions. Verify installation,
Git/tool packaging, native signing/release paths and documented platform limits.

Exit: opt-in production rollout with an explicit rollback owner, then staged
default migration. Ship through the repository's release/deployment rules only
when implementation and delivery are authorized.

## E11. Simplified commands and removal of obsolete machinery

Inventory every public/hidden CLI and Rudder slash handler, aliases, completion,
keybinding, prompt/skill invocation and Cloud/Mac/automation caller. Use current
dispatch as evidence; document stale guidance and commands already retired.
Assign keep/merge/automate/remove, canonical replacement, owner and compatibility
expiry. Include generated coordination-file readers/writers in that inventory.

Exercise the ordinary user journeys without relying on legacy vocabulary:
start one task; choose local/Cloud and visible checkout scope; select an agent;
send a follow-up; steer/interrupt/resume it; continue/copy/move a conversation;
review/check/integrate work; inspect usage; authenticate and diagnose failure.
Respect explicit plan-only intent and select decomposition automatically. Do not require `/nudge` to communicate or
recover scheduler state. Ordinary messaging must not restart intentionally
stopped, failed or orphaned runs as a hidden side effect.

Verify the initial small-help/palette budget from PLAN section 16, canonical
command discoverability, deterministic engine actions and cross-surface parity.
Native Codex/Claude slash commands must still reach their own backend in its
composer/terminal. Distinguish unknown Rudder commands, backend pass-through
and ordinary task text so removal does not silently reinterpret old syntax.

Run migrated automation against stable JSON/exit-code contracts. Compatibility
aliases have finite retirement dates; no removed command remains in suggestions,
help, generated completion, prompts or documentation after its window. Require
explicit disposition for each internal callback before deleting its handler.

Demonstrate the journal/tools replacement for retired generated fleet files
across restart/offline/catch-up. Delete obsolete fan-out code only after readers
and active legacy sessions have migrated. Preserve user-authored repo guidance,
source/history exports and session history.

Exit: the smaller human surface and complete typed machine API both pass the
same core workflows; obsolete code paths are retired rather than merely hidden.
This is a launch gate, not optional post-release UX work.

## E12. Automatic task routing and planning ownership

Label a corpus of real and synthetic requests: discussion, plan-only, a narrow
fix, a broad rewrite, urgent peer follow-up, an explicit implementation/shipping
request and adversarial/ambiguous instructions. Include multi-turn corrections,
negation and tasks that mention planning without requesting it. Measure route
quality separately from plan quality and implementation quality.

Compare deterministic routing with the optional Decisions classifier under a
fixed policy. Report classification latency, false decomposition, missed useful
parallelism, cost and the number of model calls before task acceptance. Exercise
no key, offline, rate limit, malformed/unknown answers, deadline and unavailable
model. None may leave the task indefinitely pending or change authorization.

Drive real TUI/CLI/Cloud task ingress into the chosen route. Discussion and
plan-only requests launch no implementation fleet. A routine follow-up stays
with the selected owner. Verify visible checkout/location choice, existing
approval policy and budget, same owner through delivery, and no duplicate
launch after retry/reconnect. The classifier may select a route; it cannot
grant publish permission or mutate an accepted project contract.

Sunset `/gam` without starting a replacement infinite review loop. Preserve
readability of existing paired sessions and any shared transport consumers.
Retain `/plan` only under the finite compatibility policy; new users complete
the planning journey without knowing its name.

Exit: task intent and ownership survive every route, optional classification
adds bounded cost, and the automatic path passes supported surface/backend cases.

## E13. Private preview readiness and Cloud terminal responsiveness

Test Tailscale with a fresh enrollment, persisted authorized state without a new
key, expired login, stopped daemon, transient startup failure, tailnet ACL denial,
HTTPS enabled/disabled, and IPv4/IPv6 loopback listeners. Confirm readiness from
actual daemon state. No offline/stale identity may advertise a working preview.

Start a dev server in one managed project and open it from a real laptop with
Tailscale. Verify process/workspace attribution, private URL and content, then
exit/restart the server and reconnect the laptop. Test identical ports across
different machines/projects and deny unrelated/control listeners. Confirm
Serve route removal/recovery and that no Funnel/public service was created.
Keep command/status checks asynchronous under slow or hung Tailscale commands.

For terminal transport, use deterministic clocks and a slow/failing WebSocket
to exercise output coalescing, bounded queued bytes, pause/resume, output before
exit, stale callbacks, disconnect mid-codepoint, split escapes and paste/input
boundaries. Verify both home and sail/workspace paths. Drop the connection while
typing; confirm reconnect ownership, no stale Ctrl+C replay and full repaint.

Then measure a real laptop/Cloud session with continuous output and a slow
observer. Record key-to-local-paint, authoritative echo, rollback count, first
attach/reconnect and memory bounds. Preserve terminal size, selection, scrollback
and backend prompts. A byte-transport unit test alone does not qualify complete
screen-snapshot/cursor recovery or prove an end-to-end latency target.

Exit: reachable project previews become discoverable without manual network
setup on each boot, text remains responsive under load, and the failure cases
produce recoverable visible state without lost authoritative commands.

## Decision records and first backlog

Every experiment result records hypothesis, exact setup, raw measurements,
correctness failures, source versions, adoption/license constraints, conclusion
and remaining uncertainty. An ADR names the decision, alternatives, owner,
evidence, reversal/migration path and next review trigger.

The first implementation backlog after accepting the architecture is:

1. E0 instrumentation and reproducible manifests.
2. E1/E2 two-worker prepared-environment spike with disposable views.
3. E3 one representative hermetic Cloud build conversion.
4. E5 filesystem/recovery fixture corpus and native Mac baseline.
5. E6 real backend acknowledgment/supersession probes.
6. E11 command/caller inventory and simplified task/agent workflows.
7. E12 automatic planning and E13 private-access/terminal work on the current
   architecture, with exact shipped behavior separated from the full rewrite.
8. ADRs selecting initial materializer, build profile, VCS bridge and authority
   storage before broad daemon/surface migration.

This backlog specifies follow-on work; this research change does not provision
workers, install filesystems, convert projects or migrate running agents.


## E14. Agent efficiency without quality regression

Compare the current launch policy with bounded output, earlier compaction and
lower default reasoning effort on the same fixed repository tasks. Pin source,
model/backend versions, hardware, task inputs and warm/cold cache state. Retain
failure and review results, not just successful samples.

Measure tokens and cost per accepted change; wall clock and model-step p50/p95;
mean/p95 context; coordination/wait share; repeated reads after compaction;
failed checks, review rejections and release attempts. Test Codex and Claude
adapters independently and retain explicit user settings.

The alpha.17 launch policy (effort `high`, compaction at 150K, tool output at
6K) shipped before this gate at the user's direction, so it is an unproven
candidate, not a measured result. Compare it with the earlier settings (no
compaction limit or output cap, effort from the user's Codex config). Keep a
value only when tokens and wall clock improve without increased failed checks,
review rejection or re-read rate; otherwise retune or revert it in the one
policy rather than adding a second profile. A session correlation or one
synthetic test does not pass this gate. Status-only verification has its own
regression checks and does not depend on changing model settings. The alpha.18
wait-and-read prompt rule is measured the same way: wait-only requests, clock
checks and carried read tokens, with the same failure and review gates.
