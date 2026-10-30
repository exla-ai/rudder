# Rudder v4 architecture research and evidence

Research date: 2026-10-30. Supports [PLAN.md](../../PLAN.md) and
[the experiment specification](experiments.md). These are engineering findings
and design inferences, not Rudder performance measurements or implementation.

## 1. Method and evidence boundaries

Reviewed relevant full-text design/evaluation sections of the papers below,
official current documentation, pinned source implementations, and Rudder's
current ownership/storage/transport code. Source claims and recommended Rudder
designs are separated. Paper microbenchmarks are not target-product benchmarks.
Abstracts, research proposals, marketed capabilities and production interfaces
are not interchangeable evidence.

The prior plan is preserved in Git at
[91579cf](https://github.com/exla-ai/rudder/blob/91579cf631b1d9d78afbbd786a6dc1f8120cc1ca/PLAN.md).
The checkout was initially four commits behind it; the empty working change
was rebased onto that existing main revision before editing documentation.
The initial research phase changed no product code or infrastructure.
Subsequent implementation, releases and measured experiments are tracked in
[the implementation record](implementation.md).

### Current Rudder evidence

| Observation | Relevant code/reference | Architectural consequence |
|---|---|---|
| Native `App` serializes mutations; workers return channel messages | [main.rs](../../native/src/main.rs), [engineering reference](../../AGENTS.md) sections 7/14 | Preserve one owner while extracting a persistent engine |
| Board can be projector or scheduler | [board/daemon.ts](../../src/board/daemon.ts) | Migration must not start a second scheduler |
| Cloud home owns per-project dashboard processes and rereads run state | [home.mjs](../../cloud/worker/home.mjs) | Replace file polling and dashboard-dependent execution with engine projections |
| Machine boot waits on a bounded Claude updater | [entrypoint.sh](../../cloud/worker/entrypoint.sh) | Move updates off readiness's critical path |
| `/gam` already implements Claude Channels and Codex app-server | [gam_link.rs](../../native/src/gam_link.rs) | Extract existing transports instead of assuming every backend needs terminal injection |
| Claude pending acknowledgment can clear on newer PTY output | [gam_link.rs](../../native/src/gam_link.rs) around `pending_ack` | Introduce event-ID acknowledgments and separate delivery/adoption evidence |
| Async workers carry `attemptId` checks | [run-manager.ts](../../src/run-manager.ts) | Useful predecessor to run-owner epochs |
| Directory locks may proceed unlocked after timeout | [rudder-md.ts](../../src/rudder-md.ts) | Replace best-effort locking at distributed correctness boundaries |
| Source workspace/integration paths exist in Rust and TS | [jj.ts](../../src/jj.ts), [gitio.rs](../../native/src/gitio.rs) | Consolidate through one VCS contract incrementally |
| Local Mac pull intentionally copies while Cloud continues | [LocalAgents.swift](../../mac/App/LocalAgents.swift), [Mac README](../../mac/README.md) | Copy and exclusive move require different protocols |
| Current Mac deployment target is 14.0 | [project.yml](../../mac/project.yml) | New filesystem APIs need a compatible fallback |
| Current slash help/suggestions and task dispatch expose many overlapping actions | [models.rs](../../native/src/models.rs), [main.rs](../../native/src/main.rs) | Inventory real behavior and callers before a pre-launch vocabulary reduction |
| `/nudge` combines broadcast with resuming some inactive workers | [main.rs](../../native/src/main.rs), `nudge_workers`/`/nudge` dispatch | Replace with scoped send/steer and explicit resume; keep scheduler recovery out of ordinary messages |
| CLI dispatch and help contain overlapping start, status, Cloud and legacy aliases | [main.ts](../../src/main.ts), [cloud.ts](../../src/cloud.ts) | Use a small canonical command registry and migrate automation before removal |

The current session queried the Linux guest: `6.12.114-fly`, `/dev/fuse`
present, `/dev/kvm` absent, OverlayFS/ext4/EROFS/FUSE listed, btrfs absent,
Landlock version query `ENOSYS`. This establishes this guest's capabilities,
not every future Fly kernel or the permissions needed for a specific mount.
The prior plan's disk/transcript numbers require fresh workload reproduction.

The current native dispatch starts an isolated worker for plain task input;
older engineering guidance describes a main-checkout default. Native `/run`
and `/ask` are already retired, while current `/sync` concerns Cloud/config/
default-branch synchronization. Simplification must follow audited source and
explicit future behavior, not count stale command descriptions as new removals.

## 2. Storage and workspace implementations

### BranchFS: useful interface, current eager snapshot

Reviewed [multikernel/branchfs at a4b6592](https://github.com/multikernel/branchfs/tree/a4b6592d31aacb4d2f94d0c80f43d47d38063fa9).
`create_branch` snapshots the inherited visible tree recursively; regular files
use `fs::copy`. The accompanying benchmark explicitly expects creation to
scale with the visible tree. This is not evidence of a constant-cost persistent
metadata-root fork on ext4, nor of a distributed content store. Good API ideas
do not remove the cost of this implementation.

Sources: [creation](https://github.com/multikernel/branchfs/blob/a4b6592d31aacb4d2f94d0c80f43d47d38063fa9/src/branch.rs#L466),
[recursive snapshot](https://github.com/multikernel/branchfs/blob/a4b6592d31aacb4d2f94d0c80f43d47d38063fa9/src/branch.rs#L710),
[copy implementation](https://github.com/multikernel/branchfs/blob/a4b6592d31aacb4d2f94d0c80f43d47d38063fa9/src/storage.rs#L20),
[benchmark](https://github.com/multikernel/branchfs/blob/a4b6592d31aacb4d2f94d0c80f43d47d38063fa9/bench/README.md#L44).

The related [BranchContext library](https://github.com/multikernel/branching)
illustrates speculative execution patterns. Its branch/commit/abort ergonomics
are relevant to `/swarm`; winner selection does not replace multi-change source
integration or cache validity.

### Nydus: immutable prepared layers and lazy content

RAFS separates filesystem metadata from chunked content. A view can become
available after metadata loads, while file data is fetched and cached as needed.
Integrity checking is configurable; require verified reads in Rudder's candidate
configuration. Prefetch addresses working-set latency. That suits prepared
dependencies/toolchains; frequent source mutation needs a writable view and
publication protocol. Its on-disk image format differs from REAPI/Git objects.
[Official RAFS design](https://nydus.dev/docs/concepts/rafs/).

The daemon documentation at
[f4d231c](https://github.com/dragonflyoss/nydus/blob/f4d231c40f253b4e3d8184000b3946d427a1b1b8/docs/nydusd.md#mount-writable-overlay-fs)
also describes a userspace writable overlay with upper/work directories.
Benchmark that and kernel OverlayFS before writing another overlay engine.
Measure image generation/publication, metadata loading, cold reads, and cache
reuse, not only mount time. [Image builder](https://github.com/dragonflyoss/nydus/blob/f4d231c40f253b4e3d8184000b3946d427a1b1b8/docs/nydus-image.md).

### Buildbarn: cross-platform CAS and virtual-file primitives

`bb_clientd` provides a local caching REAPI proxy and CAS-backed filesystem,
using Linux FUSE and macOS NFSv4. It also implements Bazel output integration.
This is a strong protocol/component reference for sharing build and workspace
content. It is not already a durable Rudder checkout: CAS leaves reject writes,
and writable scratch needs additional persistence/recovery. Copy-up is explicit.
[Pinned README, 4b20a85](https://github.com/buildbarn/bb-clientd/blob/4b20a85af5675feca9e9af0bca1a7f9db150be42/README.md).

Source inspected:
[CAS directory](https://github.com/buildbarn/bb-clientd/blob/4b20a85af5675feca9e9af0bca1a7f9db150be42/pkg/filesystem/virtual/cas_directory.go),
[immutable file implementation, 1fbffdc](https://github.com/buildbarn/bb-remote-execution/blob/1fbffdc417797f6afdb7f67b1f0d5cc27352db43/pkg/filesystem/virtual/blob_access_cas_file_factory.go).
Adopt components behind Rudder's view contract; qualify mounts and durability
instead of equating a lazy output filesystem with a complete writable workspace.

### JuiceFS: data-sharing clones with metadata work

Clone retains references to shared object-storage data and copies metadata.
Writes redirect to new storage. File clones are atomic; directory clones lack
atomicity if their source changes, and simultaneous clones of the same source
have documented constraints. Large trees still have per-entry metadata costs.
It is a useful complete distributed filesystem comparison rather than proof of
an O(1) directory fork. [Community clone contract](https://juicefs.com/docs/community/guide/clone/),
[cache documentation](https://juicefs.com/docs/community/guide/cache/).

### OverlayBD: block layers requiring a qualified host

OverlayBD offers lazy block images, writable layers and live snapshots while
preserving a normal filesystem above them. Its standalone routes require
TCMU/configfs or UBLK devices/kernel facilities and privileged setup. Those
requirements have not been established inside current Fly guests. It becomes
more attractive if Rudder controls suitable worker hosts.
[Standalone requirements at b8e9cc6](https://github.com/containerd/overlaybd/blob/b8e9cc61ac4090fa939a3416727fcafeb8401e0f/docs/standalone-usage.md),
[live snapshots](https://github.com/containerd/overlaybd/blob/b8e9cc61ac4090fa939a3416727fcafeb8401e0f/docs/live-snapshot.md).

### BuildBuddy: execution-state caching reference

The January 2026 design caches chunked disk and RAM, lazily restores them, and
uploads changed chunks. Preserving warmed build processes can remove costs
that an artifact cache leaves behind. Its hypervisor-level memory machinery
requires host control; it cannot be assumed inside this Fly guest.
[Fast runners at scale](https://www.buildbuddy.io/blog/fast-runners-at-scale/).

The inspected CoW implementation is under `enterprise/`. Its production use is
subject to the enterprise agreement/subscription terms; do not assume the root
repository's license permits adopting that code as a product dependency.
[Pinned implementation](https://github.com/buildbuddy-io/buildbuddy/blob/d84e5dfd97439eeefbbf247681de47f3960adc11/enterprise/server/remote_execution/copy_on_write/copy_on_write.go),
[applicable license](https://github.com/buildbuddy-io/buildbuddy/blob/d84e5dfd97439eeefbbf247681de47f3960adc11/enterprise/LICENSE).

### AgentFS: portable deltas with unmeasured build costs

AgentFS supports real coding CLIs and copy-on-write sessions with SQLite-backed
filesystem state and auditing. It is labeled beta. Linux uses FUSE/user
namespaces, with experimental ptrace support. It is worth comparing for small
source deltas, but putting heavy dependency/build I/O through it has no measured
Rudder benefit yet. [Guide](https://docs.turso.tech/agentfs/guides/sandbox),
[status](https://docs.turso.tech/agentfs/introduction),
[manual](https://github.com/tursodatabase/agentfs/blob/main/MANUAL.md),
[storage specification](https://github.com/tursodatabase/agentfs/blob/main/SPEC.md).

## 3. Build and cache correctness

REAPI defines immutable blobs/trees and an action-to-result boundary. Bazel's
hermetic model requires explicit inputs, toolchains, command and environment;
a dependency lockfile alone cannot establish action equivalence. This is the
recommended Cloud contract because the user accepts opinionated tooling.
[Hermeticity](https://bazel.build/basics/hermeticity),
[remote caching](https://bazel.build/remote/caching),
[REAPI at c2c07e3](https://github.com/bazelbuild/remote-apis/blob/c2c07e3d718d3d490a4958344d9c3e47de0bfaf3/build/bazel/remote/execution/v2/remote_execution.proto).

The later ideal-architecture discussion prefers Buck2 as the Cloud engine
candidate while preserving Bazel interoperability. Buck2 offers an incremental
graph and deferred materialization, with REAPI support; different workspace
states do not automatically share concurrent warm analysis. This integration
needs explicit branch-aware design and measurements.
[Execution model](https://buck2.build/docs/concepts/architecture/),
[remote execution](https://buck2.build/docs/users/remote_execution/),
[state/concurrency contract at a75b902](https://github.com/meta/buck2/blob/a75b9021cc7d0ef51c7c86fa4d237cef1dc02f6f/app/buck2_concurrency/src/lib.rs).

Nix derivations can produce pinned environment/toolchain closures. They do not
automatically turn an existing arbitrary shell workflow into declared build
actions. Evaluate environment production independently of action scheduling.
[Nix derivations](https://nix.dev/manual/nix/2.28/language/derivations).

Language caches have narrower contracts. For example, Rust sccache has
incremental/linking limitations; disabling incremental builds may hurt a warm
single-agent loop even while improving remote hits. Measure both paths.
[sccache Rust documentation](https://github.com/mozilla/sccache/blob/main/docs/Rust.md).

Research inference: immutable environments, result caches and warmed compiler
processes remove different costs. Their benefit must be measured independently
and in combination. One physical data service can host several representations
without making their digest formats or validity conditions identical.

## 4. Source control and native macOS

jj 0.46.0 (October 7, 2026) adds colocated child workspaces using Git worktrees
and raises Git's minimum to 2.42. Rudder pins 0.40.0 today. Both expose working-
copy factories, permitting an extension experiment before maintaining a fork.
Stock jj must register a custom factory to read that working-copy type.
[Release](https://github.com/jj-vcs/jj/releases/tag/v0.46.0),
[working-copy interfaces](https://github.com/jj-vcs/jj/blob/v0.46.0/lib/src/working_copy.rs),
[factory loading](https://github.com/jj-vcs/jj/blob/v0.46.0/lib/src/workspace.rs).

jj's change graph/conflict representation and operation history solve a
different problem from distributing dependencies. Preserve that investment
unless a concrete semantic requirement defeats the adapter/library approach.
[Architecture](https://docs.jj-vcs.dev/latest/technical/architecture/),
[conflicts](https://docs.jj-vcs.dev/latest/conflicts/),
[filesystem monitor](https://docs.jj-vcs.dev/latest/config/#filesystem-monitor).

Git worktrees have private HEAD/index but share common refs/object storage.
Isolated agent metadata needs an explicit write policy and branch namespace.
[Git worktree details](https://git-scm.com/docs/git-worktree#_details),
[repository layout](https://git-scm.com/docs/gitrepository-layout).

EdenFS demonstrates lazy repository views with dirty tracking and platform
adapters. Its source describes Linux FUSE and macOS NFSv3. Sapling's published
Git-mode support and public packaging impose material adoption costs; this is
a design reference and potential experiment, not a drop-in replacement.
[EdenFS overview](https://github.com/meta/sapling/blob/main/eden/fs/docs/Overview.md),
[Git compatibility](https://sapling-scm.com/docs/git/git_support_modes/).

Apple's APFS clone APIs share data only on the same filesystem. Recursive
directory copying still visits individual entries; best-effort cloning can fall
back to real copies. Verify the cloned state and record fallback costs.
[clonefile](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2),
[copyfile](https://github.com/apple-oss-distributions/copyfile/blob/main/copyfile.3).

FSKit is a potential user-space Mac VFS path, with framework availability from
15.4 and path/generic URL resources from 26.0. Existing Mac 14 support needs a
fallback. API existence is not evidence of compiler/Xcode performance.
[FSKit](https://developer.apple.com/documentation/fskit),
[path resource](https://developer.apple.com/documentation/fskit/fspathurlresource),
[generic resource](https://developer.apple.com/documentation/fskit/fsgenericurlresource).

Apple's Linux virtualization/container tooling is an optional Cloud-parity
profile. It does not make Linux and Darwin artifacts interchangeable.
[Virtualization.framework](https://developer.apple.com/documentation/virtualization),
[Containerization](https://github.com/apple/containerization).

## 5. Fly and execution-platform constraints

Machines and Sprites use Firecracker. That outer VM boundary does not isolate
agents within one guest from its kernel. Adding another KVM-dependent runtime
inside this guest is not an established deployment path.
[Fly explanation](https://fly.io/learn/firecracker-vs-gvisor/),
[Firecracker requirements](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md).

Fly Volumes are host-local and attach to a single Machine; replication/shared
fleet storage is not automatic. Plan local caches plus a remote data service.
Suspend/resume of the same Machine also does not establish reusable clone APIs.
[Volume contract](https://docs.fly.io/volumes/overview),
[suspend/resume](https://docs.fly.io/reference/suspend-resume).

Sprites' published storage design uses immutable object-store chunks and local
NVMe caches, with checkpoints changing metadata. It is a strong engineering
reference for Rudder's desired storage shape. Newer SBD documentation/product
material describes another object-backed block layer, with differing rollout
wording; access must be qualified. Neither establishes a publicly embeddable
component for ordinary Rudder Machines.
[Design article](https://fly.io/blog/design-and-implementation/),
[product/SBD description](https://fly.io/sprites/),
[maintenance](https://docs.fly.io/sprites/sprite-maintenance).

The live public Sprite OpenAPI inspected has no fork/clone route and creation
accepts name/URL settings, not an inherited checkpoint. This is a qualification
gap, not proof that no private capability exists. Warm/cold process continuity
also differs. [OpenAPI](https://docs.fly.io/sprites/api/openapi.json),
[lifecycle](https://docs.fly.io/sprites/concepts/lifecycle).

E2B documents live forks of disk/memory/process state and fan-out from one
capture, but the source pauses and active streams disconnect. It is a useful
external worker comparison, not a library running inside the current Fly VM.
[Fork contract](https://docs.e2b.dev/sandbox/fork).

Runtime qualifications remain independent: OpenShell's current support matrix
requires Landlock; gVisor's default Directfs avoids some Gofer round trips, so
complete read audit is not obtained merely by adopting its Gofer.
[OpenShell requirements](https://docs.nvidia.com/openshell/latest/about/support-matrix),
[gVisor filesystem](https://gvisor.dev/docs/user_guide/filesystem/).

## 6. Communication and coordination protocols

Claude Channels documentation separates writing a notification from agent
processing; notifications lack acknowledgments and busy agents may process
queued messages later. Use a durable Rudder event journal, explicit acknowledgment
tools and final compliance checks. [Channels reference](https://code.claude.com/docs/en/channels-reference).
Run-scoped authenticated capabilities and provenance remain necessary even if
a system-prompt convention helps the agent recognize authorized messages.

The preview development-channel bypass requires an interactive session and is
ignored under `-p`/Agent SDK. This is not a blanket restriction on approved
Channels. Qualify activation per execution profile instead of assuming a channel
exists whenever its server process starts.
[Custom-channel activation](https://code.claude.com/docs/en/channels-reference#test-during-the-research-preview).

Codex app-server exposes thread/turn lifecycle, structured events and active-turn
steering with expected turn identity. Rudder already uses it. Keep capability
and version tests around the adapter as transports evolve.
[Official app-server documentation](https://learn.chatgpt.com/docs/app-server).

For the injection mapping, distinguish passive history append from a request
to process a message. Preserve peer/tool provenance, serialize outbound run
mutations and reconcile turn races before retrying. Existing `/gam` event
handling is an extraction seam, not a delivery/deduplication specification:
its PTY-based acknowledgment and early inbox/reply deletion need replacement.

Active interruption is a separate control contract: Codex documents
`turn/interrupt` with an interrupted turn outcome, while Claude's interactive
Ctrl+C behavior depends on whether work is active. Channels alone are not a
confirmed interruption API. Current Rudder has interruption-specific receipts
and hook paths; the new engine must not equate them with objective completion.
[Codex control](https://learn.chatgpt.com/docs/app-server),
[Claude interactive mode](https://code.claude.com/docs/en/interactive-mode),
[current signals](../../native/src/signals.rs).

ACP specifies agent sessions, prompts, updates, cancellation and negotiated
capabilities. It can reduce adapter duplication but does not guarantee identical
resume or in-turn steering semantics for every backend.
[Protocol](https://agentclientprotocol.com/protocol/v1/overview),
[session setup](https://agentclientprotocol.com/protocol/v1/session-setup),
[Rust library](https://agentclientprotocol.com/libraries/rust).

NATS JetStream is a reference for durable consumers, redelivery, acknowledgment
and backpressure. At-least-once message delivery still needs idempotent effects;
it does not make an agent's arbitrary actions exactly-once.
[Delivery guarantees](https://docs.nats.io/learn/jetstream/delivery-and-acknowledgment).

Prompt caching benefits from stable prefixes. Model/CLI-managed caching must
be measured directly; replacing context files with channel events does not
automatically reduce token cost. [Claude prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching).

### Automatic routing and the Decisions API

The current [OpenAI guide](https://developers.openai.com/api/docs/guides/decisions)
and [create reference](https://developers.openai.com/api/reference/resources/decisions/methods/create)
describe typed predicate/choice/score evaluation through `POST /v1/decisions`;
`gpt-6-luna` is the currently supported model. Applications supply choices and
receive answers, not a generated DAG or tool execution. That makes it a routing
candidate before Rudder's existing planner. Provider speed claims do not prove
Rudder task-acceptance latency. Keep the adapter optional, deadline-bounded and
separate from accepted project decisions. Validate routing quality on Rudder
requests, preserve explicit read-only intent, redact credentials and retain
deterministic behavior without OpenAI API authentication.

### Private connectivity

[Tailscale Serve](https://tailscale.com/docs/features/tailscale-serve) exposes
services inside the tailnet under network access policy. It is distinct from
public Funnel. [Userspace mode](https://tailscale.com/docs/concepts/userspace-networking)
supports unprivileged container networking; outbound application connectivity
requires its documented proxy configuration rather than an assumed kernel
interface. Rudder already uses a persisted userspace daemon for Cloud previews.
Improving its readiness and project attribution is an incremental product
change; it does not establish a direct Cloud-terminal transport or a Mac runner.

## 7. Papers and architectural lessons

These sources support mechanisms and experiments, not predicted Rudder speedups.

| Paper and primary source | Relevant finding | Rudder inference and limit |
|---|---|---|
| [Slacker, FAST 2016](https://www.usenix.org/conference/fast16/technical-sessions/presentation/harter), [full text](https://research.cs.wisc.edu/adsl/Publications/slacker-fast16.pdf) | Clone-backed storage and lazy fetching can avoid eager image-transfer work | Cache prepared environments and fetch their working sets; measure complete builds because startup traces are not compile traces |
| [SOCK, ATC 2018](https://www.usenix.org/system/files/conference/atc18/atc18-oakes.pdf) | Preinitialized process trees amortize runtime/import cost | Useful warmed-runtime reference; arbitrary independent CLI `exec` does not inherit the same savings |
| [SAND, ATC 2018](https://www.usenix.org/system/files/conference/atc18/atc18-akkus.pdf) | Related functions share setup and use hierarchical messaging | Group related workloads near prepared state; its function sandbox assumptions do not transfer unchanged to coding CLIs |
| [SEUSS, EuroSys 2020](https://www.cs.bu.edu/fac/jappavoo/Resources/Papers/seuss.pdf) | Execution snapshots and page sharing bypass initialized paths | Compare persistent build services and snapshots; a unikernel research prototype is not a drop-in runtime |
| [Fork, Explore, Commit, 2026](https://arxiv.org/html/2602.08199v2) | Branch contexts connect speculative filesystem/process state and winner selection | Apply branch lifecycle ideas to experiments; proposed Linux syscall is not an available API, and current BranchFS source differs from paper creation claims |
| [Build Systems a la Carte, ICFP 2018](https://www.microsoft.com/en-us/research/uploads/prod/2018/03/build-systems.pdf) | Separates scheduling from rebuild/reuse strategies, including constructive traces | Keep scheduling, change integration and action validity distinct; reuse requires known dependencies and suitable determinism |
| [SWE-agent, NeurIPS 2024](https://arxiv.org/html/2405.15793v3) | Agent-computer interface design affects agent behavior and outcomes | Provide concise structured tools and feedback; preserve real CLI workflows rather than assume a filesystem alone improves reasoning |
| [Why Do Multi-Agent LLM Systems Fail?, 2025](https://arxiv.org/html/2503.13657v3) | Failure taxonomy includes system design, inter-agent misalignment and verification | Evaluate provenance, decision drift, termination and verification; a bus alone does not establish alignment |
| [Towards a Science of Scaling Agent Systems, 2025](https://arxiv.org/html/2512.08296v1) | Gains and coordination overhead vary with task/architecture | Optimize accepted changes per budget; more agents and more messages are not universal improvements |
| [Chubby, OSDI 2006](https://www.usenix.org/legacy/events/osdi06/tech/full_papers/burrows/burrows.pdf) | Sequencers let resource servers reject stale lock holders | Carry owner epochs to receivers; lease timeout alone cannot fence an old writer |
| [Raft, extended paper](https://raft.github.io/raft.pdf) | Replicated ordering requires a defined consensus protocol | Reuse established transactional/replicated infrastructure; do not invent consensus as part of the filesystem project |

For any future replicated watch/lease service, understand revision recovery and
compaction rather than treating a watch stream as permanent history.
[etcd guarantees](https://etcd.io/docs/v3.6/learning/api_guarantees/).

## 8. What changed from the earlier plan

| Earlier assumption | Revised position |
|---|---|
| Lockfile hash identifies deps and builds | Environment identity includes installer/platform inputs; actions include source and full dependency closure |
| One SQLite daemon store supplies fleet truth | Local journals plus one transactional project authority, with explicit offline reconciliation |
| Native btrfs/Landlock/cgroup v2 assumed on fleet | Existing Fly kernel/delegation gates first; platform backends advertise capabilities |
| Channel mid-turn delivery is guaranteed | Durable journal/replay plus explicit acknowledgment and separate verification |
| Prompt nonce authenticates control | Authenticated run capability and provenance; nonce is behavioral context |
| Observed touched files ratify shared decisions | Delegated authority and topic versions; files are only one relevance signal |
| fsnotify is delta ground truth | Journal plus consistent seal/reconciliation and recovery tests |
| APFS/btrfs/BranchFS implies instant tree forks | Measure metadata work, data copying and publication separately |
| jj disappears from execution without a tooling contract | Preserve real per-agent Git behavior and define a pinned jj integration bridge |
| Delete coordination files on a calendar milestone | Retire them after every consumer has an explicit durable delivery/fallback path |

## 9. Evidence that is still missing

- Measured FUSE/overlay permission and semantics on actual provisioned Fly workers.
- Native Mac benchmarks and Xcode/tool compatibility; Linux research cannot prove them.
- Backend comparison on real dependency-heavy repos with concurrent editing/builds.
- Cloud build-profile conversion effort and cache correctness across supported languages.
- Publication/GC/crash testing, tenant cache boundaries and checkpoint durability costs.
- Real backend acknowledgment/adoption behavior after disconnect and busy turns.
- License/version/packaging qualification before adopting each exact dependency.

These missing facts are the phase-0/early-phase experiments, not prerequisites
for writing down the logical architecture. No claimed Rudder speedup is measured
by this document.
