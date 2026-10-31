# Remote managed launch and quiescent partial checkpoints

Status: implementation in progress, 2026-10-31. The alpha.12 candidate implements
the held-state workspace factory, strict remote selection and reserved namespaces;
see [the implemented ownership contract](execution-workspace.md). Remote guest
activation, quiescent finalization and the proposed format floors below remain
unimplemented. They compose the existing
[partial capture](partial-retention.md) and [managed launch](managed-launch.md)
contracts toward [PLAN §§4–6 and 8](../../PLAN.md). Existing qualification does
not establish remote managed launch, remote source acceptance or fleet latency.

## First supported profile

- Linux worker UID/GID1500, explicit managed launch and one reserved attempt.
- A freshly authorized replicated RunnableRoot; source-only WorkspaceProfile,
  no prepared/scratch/VCS-local partitions, no environment and empty topology.
- SourceV2.directory must equal RunnableRoot.view_root. Private VCS metadata
  stays outside the view; its authorized seed may remain completely retained.
- Nonempty inherited hardlinks, partitioned views and environment composition
  return Unsupported. A source-exclusion capture cannot substitute for a full
  runnable view; wider profiles need explicit source/view derivation evidence.
- No eager fallback, inherited closure scan or complete local workspace pin.

## Reservation and the pre-mount factory

Native preparation reserves owner/run epoch, attempt, request, execution,
operation, view and observation namespace UUIDs before granting authority. The
remote assignment binds provider/tenant, immutable root ID and typed root,
profile, policy and accepted seed/source identity. Credentials use a separate
host-only protected or sealed payload, never guest environment or shared CAS.
Preparation does not start a RemoteSession that later crosses the namespace.

The implemented hook is in execution/linux.rs: after the fresh private USER/MNT
transition, state::preflight reserves or reconciles the held execution before
services or workspace factories. HelperServices carries a trusted one-shot
HelperWorkspaceFactory returning OwnedHelperWorkspace with the actual manager,
view and resource owners. None preserves the local implementation. The post-RPC
HelperApplication::start callback is too late for workspace construction.

The production remote factory still needs to create NodeRemoteSource,
RemoteCache, RemoteRootRegistry and a fresh admitted session. Implemented
validate_selection checks its actual manifest/profile/policy/topology against
the sealed expectation under startup budgets. The opaque result opens its exact
bound view through a reserved-namespace fork. The reserved UUID is included in
immutable fork-request bytes and observation_state; reopening mismatched inputs
fails closed. This selection does not establish accepted history or seed custody.

OwnedHelperWorkspace.resources retains source/cache/registry/session through
FUSE drain, capture retention and execution-record fsync, outside the guest and
its PID namespace. Protect remote/cache/credential/authority roots from guest
binds and close their descriptors before guest execution.

CurrentAttemptAuthority::validate must remain bounded in-memory work. Existing
BrokerAuthority performs broker I/O and cannot implement it directly. A host
monitor refreshes an expiring grant bound to the exact root and attempt; expired,
unavailable or revoked authority denies further byte exposure and invalidates
the session. Invalidating interest does not free admission or payload ownership
until the actual transport settles. Polling continues through final receipt
publication. The implemented workspace monitor now spans checkpoint retention,
record fsync and the authenticated receipt; remote-grant refresh still needs its
own current-authority composition.

Graceful stop withdraws exact guest network admission, streams and helpers; it
retains the exact workspace/remote-root finalization authority and refresh
monitor through PID/VCS/RPC/FUSE drain, capture retention and any required transfer-owned
clone acknowledgments, execution-record fsync and authenticated shutdown receipt
publication. Guest network withdrawal alone must not invalidate that source
grant. Actual owner/run revocation invalidates RemoteSession and withholds
checkpoint proof while preserving acknowledged data. The proposed
[execution network profile](execution-network-profile.md) uses this same lifetime
split; guest network authority cannot substitute for source finalization authority.

All source/cache startup, capture, transfer, private jj and journal I/O runs on
bounded workers or the dedicated helper. App alone adopts typed results and owns
PTYs; callbacks and late preparation cannot select another attempt.

## Drain proof and the partial checkpoint artifact

PartialCaptureReceipt v1 remains LiveBestEffort, including matching cursors.
Introduce QuiescedPartialCheckpoint v1, wrapping the exact capture receipt plus
stop request, observation binding, execution digest and durable drain-record
identity. Only trusted executor finalization can construct its proof; a guest
boolean, fsync assertion, CLI exit or saved PID cannot authorize it.

The finalization order is mandatory:

1. Stop new effects for the exact attempt while retaining finalization authority.
   Receive the authenticated parent stop; observe owned backend pidfd exit and
   private PID-init reap, including detached children and kernel I/O teardown.
2. Close actual VCS ingress, settle handlers/children and retain the canonical
   CommandService::quiesce writer fence through partial ACK and record fsync.
3. Close observation RPC and successfully await all in-flight effects.
4. syncfs, normal unmount and FUSE join must succeed. Busy mounts, drain timeout
   and discard withhold checkpoint proof; discard remains cleanup only.
5. Freeze logical writes under the owning view lock and durably bind finalization
   to one nonce/cursor/head. Capture producer refs and authorized boundaries,
   commit object-set retention, then fsync the execution record.
6. Recheck current authority and send the matching authenticated shutdown receipt.
   App requires the exact receipt and observed physical exit before Paused.

ExecutionRecord v2 distinguishes CompleteLocal(VfsCheckpoint) from
PartialRemote(QuiescedPartialCheckpoint). Preserve the v1 complete encoding;
partial refs cannot fill the complete checkpoint field or grant a complete pin.

A durable candidate never recaptures later bytes under its nonce. Before data
ACK, expired staging or missing exact bytes yields unavailable/uncertain and
requires reconciliation or a new nonce. After ACK, cancellation, revocation,
head failure or lost reply preserves retention. Crash recovery reads exact
records; it grants no old session or writer authority. Do not clear dirty
baseline or rebind the synthesized root. Recovery under another owner requires
explicit authorization, not reopening the old attempt as current.

## Transfer, publication and history acceptance

After proved drain, stage canonical SourceV2 and RunnableRoot v1 descriptors and
an immutable publication plan. Retain wrapper/proof objects as an object set.
TransferSession::pin_partial_checkpoint clones the exact capture and wrapper
units under transfer-owned retention, beginning staging before reading producer
receipts. Validate every binding; never call pin_existing or retained_closure.

Add uploadPartialCheckpoint using the retained inventory and RunnableRoot as
publication target. Verify the immutable base publication and use a durable
publication ID bound to the frozen intent. Existing Cloud closure validation
must verify the entire resulting graph, including inherited objects, before a
ReplicatedRootReceipt. A directory-only upload receipt is insufficient. Lost
publication replies reconcile the same immutable root; disconnect drops only
transfer-owned retention.

RpcWorkspaceView, LiveObservation.retention and journal VCS intent retention
require complete local pins. Add explicit partial provider/projection and
replicated-retention branches; keep old remote observe/checkpoint/adopt calls
gated. Reuse authorized baseline mapping evidence for unchanged subtrees; changed
paths use producer objects and opaque remote boundaries under explicit budgets.
Casting receipts or hydrating the inherited SDK does not satisfy this contract.

A trusted off-App acceptance worker verifies fresh scoped replication of the
exact RunnableRoot and the actual private jj operation, commit/change/parents,
tree, projection policy, profile and attempt under the canonical history lock.
Existing owner/run/topic/head CAS then commits accepted history. Network
validation precedes the short journal transaction; serialized receipts alone
grant no authority. Retain the current capture head through upload/history
failure. Release generation-exact units only after publication and acceptance
reconciliation; the next launch uses a new view and freshly admitted session.

## Proposed compatibility and implementation boundary

The proposed outer ExecutionSpec version is greater than3 and must be allocated
jointly with the execution network profile; neither proposal independently
reserves4. ManagedExecutionBinding3, ExecutionRecord2, WorkspaceAssignment2, a
separate partial RPC/provider protocol1, QuiescedPartialCheckpoint1 and remote
namespace5 remain proposals. The proposed remote namespace floor gates new
finalization semantics; local namespaces1/2 and live PartialCaptureReceipt1 stay
unchanged.

Journal schema5 now implements only the opt-in managed-project writer floor and
historical preparation/recovery receipt. It does not implement partial provider
or history acceptance authority. Future acceptance requires a separately
allocated journal schema/writer capability; opening schema5 or reading its
receipt grants no launch or acceptance authority. Existing SourceV2/RunnableRoot1
and individual-object retention schema2 need no reinterpretation.

Minimal code groups: execution types/Linux/state/handoff; VFS remote/capture/
observation/RPC; native preparation/services/broker/App adoption; bridge partial
provider/projection/history; transfer service/TS uploader; journal acceptance/
schema. Implement behind explicit capabilities before activation.

Activation also requires quiescent legacy owners and a durable writer floor.
Managed records need their own namespace and must never project launchable
legacy graph.workspace/jj metadata. launchNode, mergeNodeIntoIntegration, direct
workspaceDiff and board paths must retire, engine-route or check durable
capability before effects. Missing owner.json and per-run guards do not fence
these paths or older native clients. Full managed-source acceptance stays closed.

## Required qualification

| Case | Required result |
| --- | --- |
| Fresh factory, stale assignment, wrong root/scope/profile | Exact admission or denial before mount/guest effects; bounded metadata only |
| Grant expiry/revocation, stalled transport | Byte exposure denied; no early capacity release or checkpoint ACK |
| Original FD/mmap, detached child, surviving VCS writer | Final bytes only after actual process/VCS/RPC/FUSE drain; otherwise uncertain |
| Busy unmount, RPC timeout, abrupt helper death | No strong receipt; acknowledged data retained |
| Crash before/after candidate and data ACK, lost shutdown reply | Immutable nonce replay or definite unavailable; no silent recapture |
| Transfer GC/producer retirement/disconnect | Exact cloned units survive; producer/current head and unrelated pins preserved |
| Missing inherited object, lost publication reply, changed history head | No acceptance; immutable publication reconciles; retention remains |
| Large inherited tree with a small dirty delta and exhausted budgets | No unchanged payload hydration or complete local pins; bounded explicit failure |
| Released binaries, missing owner projection, direct scheduler/board calls | Compatibility refusal before effects and protected paths preserved |
| Fresh worker restore after accepted publication | New authorized session, exact runnable bytes and validated history; no local-completeness claim |
