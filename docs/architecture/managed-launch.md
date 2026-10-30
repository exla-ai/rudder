# Managed local agent launch

This is the next composition after the [explicit native VCS
qualification](../../examples/vcs/evidence/2026-10-30/native-v3/README.md).
That proof used local workspace roots, real native broker authorization and
debug-built executables, with a shell/jj fixture. It did not qualify actual
Codex/Claude execution, normal App launch, provider credentials or lazy remote
roots. Default launch remains unchanged. The following
composition is implemented in the working tree and remains explicitly inactive
until its remaining activation and provider gates pass.

## One owner, one attempt

The App remains the only native state/PTy owner and scheduling authority. Heavy
preparation runs outside its loop and returns typed results. No background task
may launch a replacement agent, mutate App or choose a new run/attempt identity.

The explicit host preparation API is:

```rust
struct ManagedLaunchFence {
    owner: OwnerFence,
    run_id: String,
    run_epoch: u64,
    attempt_id: Uuid,
    request_id: Uuid,
}

fn prepare_local(
    request: ManagedLocalLaunchRequest,
    cancelled: Arc<AtomicBool>,
) -> Result<PreparedManagedLaunch>;
```

The request selects an already retained local RunnableRoot, authorized private
VCS seed/baseline, backend execution profile and exactly one credential profile.
It carries the reserved fence, executable identities and source/profile policy.
It is not a request to scan an arbitrary checkout, use another run's metadata or
download a remote root through a local fallback.

`PreparedManagedLaunch` owns the preparation leases/pins, protected context,
bounded in-memory execution spec, transport mapping and cleanup responsibility.
The persistable `WorkspaceAssignment` contains only noncredential identities:
format version, owner/run/attempt, execution/operation/view/namespace,
source/profile/seed references, exact observation binding and protected-context
reference. Credentials and raw secret-bearing command environments are not
Debug formatted, placed in run JSON/shared CAS, or returned in notices.

1. App reserves the UUID and calls `observe_run_attempt` once, before preparation.
   A visible pending launch is distinct from a live terminal.
2. Background preparation forks the local view/private metadata, checks image
   companions and source policy, prepares guest-only configuration and selects
   credential material. It checks cancellation and current fences between bounded
   operations; it never changes scheduler/run status.
3. App adopts a result only while owner, run epoch, attempt, request, scope and
   pending-launch state still match. Cancelled, removed or replaced runs cannot
   spawn from a late result. Unused results retire only their own resources.
4. The exact workspace assignment is privately persisted before granting its
   authority. App issues the matching capability, starts the scoped broker and
   gives the actual PTY the one-use sealed-spec handoff. No second UUID is minted
   by later command augmentation.
5. The helper enters its fresh namespace, validates broker/context, opens the
   local view, binds owning-view RPC, starts the deferred VCS application, checks
   the guest projection and launches the selected runtime.

The existing wrapper remains for legacy runs. A new
`prepare_agent_tools_for_attempt(..., fence, Option<WorkspaceAssignment>)` uses
the explicit reserved identity. It must not treat a command's environment,
prompt, cwd or a guest's JSON as proof of that reservation. Existing saved
conversations retain their execution profile; managed local launch is explicit.

## Capabilities survive rotation without widening

Current peer identities grant project communication/control operations. Managed
workspace access additionally requires `workspace:observe` and the exact
`workspace:<SHA256(canonical ObservationBinding)>` scope. The binding includes
execution/operation UUIDs, owner, view, namespace, profile and exclusions; the
broker also checks current run epoch and actual execution instance.

Inventory and credential rotation consume the same validated assignment. They
must preserve that exact workspace scope while the matching attempt owns it,
and omit it for legacy, replaced or retired attempts. They cannot infer a missing
binding or attach the newest view to an old process. A token rotation may change
only credentials within the same binding; published projections and broker
authentication must remain consistent with the existing bounded rotation path.

Stopping new peer effects and retaining enough workspace authority to drain are
separate operations. Authority needed for the final checkpoint remains live
until its matched receipt, then is retired. Actual revocation/epoch loss fails
closed and yields an orphan/uncertain outcome rather than a checkpoint.

## Guest runtime and credentials

The guest receives the source view, declared prepared/scratch partitions,
read-only image runtime, selected agent's private home/environment and its own
broker/service endpoints. It receives no engine `.jj`, global CAS/journal,
credential vault, all-profile HOME, peer identity or sibling workspace.

Credential selection is explicit and occurs outside App. Claude receives only
the selected API key in its bounded sealed environment. The selected Codex key
stays on the host for authenticated app-server bootstrap with ephemeral storage;
it is absent from guest home files, environment and the execution spec. The
helper clears the inherited environment. The source
profile selector/lease may be persisted privately; credential values may not.
Qualification uses existing configured accounts without new sign-in, account or
paid-plan changes, and avoids refresh-token reuse that could invalidate an
active user session. Reports contain only redacted identities/provenance.

MCP settings point to guest paths and the selected run's broker proxy. No MCP
process in the guest opens the journal. Host and guest transport paths are
different fields, not string replacements over arbitrary argv. The existing
private execution tmp/home can back scoped guest paths, but a writable guest
socket pathname alone is not authenticated backend identity. Trusted helper
readiness must bind control transport to the actual owned backend process/
attempt, using kernel process identity where applicable. Repo/tool output cannot
create a runtime binding or certify an interrupt.

Codex app-server and its remote TUI must use the same observed thread/server.
Claude uses an explicitly selected qualified SDK/Channels profile and supported
typed controls. Busy SDK steering requires cancellation/interrupt confirmation
and the matching terminal result before replacement peer input. It cannot enqueue
new work while the old outcome is unknown. Preview Channels consent is never
auto-answered. Capability negotiation remains observed, not assumed from a
configured version string.

## Authenticated shutdown before checkpoint

Current abrupt terminal teardown closes link/broker and kills the helper. It
cannot certify a checkpoint and must retain that honest behavior. A managed
graceful path needs an authenticated parent-owned control channel, separate from
guest observation/MCP and user stdin:

```text
StopAndCheckpoint {
  request_id, execution_id, owner_epoch, run_epoch, attempt_id, reason
}

ShutdownReceipt {
  same request/fence,
  process-drain evidence,
  execution-record identity,
  retained checkpoint reference OR explicit uncertain/orphan outcome
}
```

The channel is derived from the owned launch/handoff, with peer PID/UID/lifetime
checks or a private inherited control FD. No guest-selected path, model text,
peer fsync boolean or persisted PID authorizes it. Duplicates reconcile the same
request/record; they do not terminate a later attempt or start another backend.

1. App stops admitting new run effects, keeps broker/workspace authority alive,
   and requests shutdown for the exact owned attempt. Qualified typed backend
   interrupt is a separate fact from physical process exit.
2. Trusted helper/PID init stops its actual backend processes and drains all
   namespace writers, including detached children and kernel I/O. Owned backend
   readiness/controls must remain attributable to their actual process.
3. The VCS application closes ingress under the real spawn gate, settles actual
   handlers/children and acquires its canonical writer fence.
4. With that guard held, helper settles observation RPC, strictly unmounts and
   drains FUSE, publishes the retained checkpoint and fsyncs the execution record.
5. App accepts only the matching receipt/record and observed process state. It
   then closes link/broker, clears the live process object and records Paused or
   the appropriate typed task state. Backend completion, verification and
   integration remain separate facts.

A stopped/deleted/replaced run cannot be revived by a late launch/control result.
Timeout, authority loss or inability to prove writer settlement withholds
checkpoint acknowledgment and requires explicit reconciliation. A socket write,
CLI exit, output marker or UI detach is not this receipt. Surface close preserves
the persistent engine and live PTYs; owner shutdown is distinct.

## Qualification gates

The first real-provider gate stays local-root-only on an owned disposable worker.
It uses actual installed Codex and Claude CLIs, selected private credentials,
their negotiated protocols, real PTY/renderer and the same broker/helper/VCS
composition. It must demonstrate:

- Exactly reserved owner/run/attempt through asynchronous preparation, PTY,
  broker grant, runtime binding and repeated inventory/token rotation.
- Real nonempty file edits/status/diff in the scoped view, no materialized source
  beside private jj metadata and no global profile/peer/CAS/journal visibility.
- Two attributed backend conversations: durable peer acceptance, actual delivery
  and explicit recipient acknowledgment remain distinct observations.
- Expected-thread/turn control acceptance, observed matching interrupt/cancel
  receipts, no replacement turn before confirmation, and no replay on ambiguity.
- Actual epoch/revocation denial; stale prepared results, delayed controls and
  stopped runs cannot acquire current authority or start work.
- An original FD changed after observation, then authenticated graceful shutdown,
  all exact pidfd witnesses exited, writer/RPC/FUSE settlement and retained final
  bytes. Abrupt helper death/revocation instead leaves no checkpoint claim.
- Bounded logs/frames/work, cleanup of only owned resources and verified VM
  deletion. No active home, current conversation or global profile is restarted.

The controlled canary may use at most a small stated number of model calls. It
must fail before credentials/inference when required graceful-control/runtime
capabilities are absent, rather than substitute shell fixtures and report success.
Uncertain cases stay explicit; no vendor latency or small corpus becomes a fork,
build-cache, cold-fleet or p99 performance claim.

Lazy remote launch needs a separate typed pre-mount factory **after namespace
entry and before opening/mounting the view**. It must establish a fresh scoped
RemoteSession plus owned source/cache lifetime, protected context and broker
authority. The existing post-RPC VCS application callback is too late. Local
qualification does not authorize eager fallback or claim remote-root support.

## Implemented composition and activation gates

`App::start_managed_local` is an internal explicit API, with no public command,
configuration or default caller yet. It accepts a stopped row without a legacy
workspace/session, retained local roots, an authorized VCS seed and an explicit
credential selection. At most two preparation workers run; their bounded typed
results carry the original fence. Cancellation, panic and stale results settle
without reviving a retired attempt.

A private fsynced reservation precedes preparation effects. The assigned view
is then persisted before any capability grant. The App adopts the exact broker
and sealed handoff and remains the only PTY owner. Backend readiness carries an
actual kernel process witness; guest report files carry candidate summaries,
never readiness, acknowledgment or deployment authority.

`ManagedExecutionHandle` requests authenticated graceful shutdown over a private
parent channel. A checkpoint receipt is bound to the sealed spec, owned helper
PID, exited backend witness and exact request/owner/run/attempt/view. App waits
for both that receipt and PTY exit before recording Paused. Abrupt teardown or
uncertain settlement withholds checkpoint success and source acceptance.

Focused checks cover managed preparation/adoption, protected credential roots,
broker binding, legacy resume guards, control-channel settlement and unchanged
legacy handoff encoding. Real provider execution is still a separate gate.

Before activation, recovery must establish an explicit compatibility boundary.
Alpha.9 native ignores a new run marker, so a marker alone cannot prevent a
legacy restart from reinterpreting a managed run. Managed records need their own
namespace and a durable writer protocol floor, activated only after legacy
owners and children are quiescent. Old-client refusal must be tested against the
actual released binaries. This is cooperative compatibility, not physical
revocation of arbitrary old clients. The alpha.11 working tree adds the private
managed-project [private storage](managed-storage.md), a transactional writer floor and conservative
recovery. Neither preparing that floor nor reading a historical receipt permits
the legacy App scheduler to run the project.

Selected API-key credentials are the first supported profile; OAuth remains
typed unsupported until protected refresh coordination is qualified. Codex
0.166.0 app-server disables environment-key login, so a fresh private home needs
the supported `account/login/start` API and confirmed `account/read` before
starting its TUI/thread. Passing `OPENAI_API_KEY` alone is insufficient. The host
must keep the key out of persisted metadata, notices and diagnostic errors.
Claude setup tokens and saved OAuth access/refresh tokens are distinct inputs;
they cannot be substituted or copied from an active session without a qualified
lifecycle.

Guest networking is another concrete activation gate. The current restricted
helper shares the host network namespace and reports `network_isolated:false`.
Before alpha.11 the normal board served its browser token to loopback HTTP
clients; a raw guest client could satisfy those Host checks. Source review found
that this reached host project controls, despite filesystem/PID isolation. No
live exploit was attempted. The alpha.11 working tree adds
[host-issued board sessions](board-access.md) with a private bootstrap channel.
Default launch needs controlled guest networking and authenticated board access;
pathname-socket protection does not isolate TCP loopback.

Default activation, provider inference/OAuth qualification, managed-source
acceptance, controlled networking and the remote pre-mount factory remain
explicit follow-up work. No current internal API result claims those gates passed.
