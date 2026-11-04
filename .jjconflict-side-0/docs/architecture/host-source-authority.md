# Host source authorization and finalization

The engine keeps source access separate from guest tools. Graceful shutdown
can withdraw guest mail, model control and workspace RPC ingress while retaining
the source access needed to drain writers, capture changes and publish retained
checkpoint data. Owner, run or source-grant revocation still refuses further
authorization. Closing guest ingress is not source revocation.

This is an implementation contract. The endpoint and ownership primitives are
tested; the new managed launch and shutdown composition is still being built.
They do not yet change the default execution path.

## Independent evidence

| Object | What it establishes |
|---|---|
| `EngineSourceLease` | Actual process-local Fabric ownership and held checkout lock custody; retirement is absorbing |
| Host-source Journal result | Current capability, owner, run epoch, execution and exact binding from one read transaction, with its original expiry |
| Authenticated endpoint exchange | An actual request/reply under the endpoint key, purpose, instance, nonce, serial and complete binding |
| Controller process witness | Actual retained kernel process custody from the authenticated helper handoff; the adapter verifies the retained controller witness |
| `RemoteSession` | A newly registered source session under the live source authority; saved responses cannot reconstruct it |
| Finalization constraint | A fixed host clock deadline limiting all phases; it supplies no execution or drain permission |
| Checkpoint DATA receipt | Retention/publication of exact objects; it supplies no accepted source, jj history or runnable launch grant |
| Shutdown packet | Provisional completion data until the parent joins the remaining checks |

The host capability uses operation `source:host:authorize`, purpose
`remote_source_only_v1`, and a domain-separated scope over the canonical complete
`PartialBinding`. Guest partial-workspace and complete-observation capabilities
cannot substitute for it. Host credentials stay outside guest files,
environment, ordinary descriptors and shared CAS.

## Endpoint contract

Requests and replies use RustCrypto HMAC-SHA256 with separate domains. Requests
send no raw credential. Replies bind the original request MAC as well as the
exact binding, endpoint instance, nonce, serial, result and remaining lifetime.
The client also requires independently held issuer process/custody checks;
knowledge of a connection key alone does not establish engine ownership.

The server samples wall time after the Journal query and conservatively computes
the capability's remaining nanoseconds. The client caps freshness at its
original sent boottime plus the smaller of that interval and three seconds.
Late replies cannot start a new window. Clock failure, expiry, denial, changed
custody and callback panic fence the endpoint. This bounded cache is not instant
database revocation; the finalization owner must actively invalidate the client
and actual source session on known retirement and request fresh authorization
before publication effects.

Two callback workers share four queued/active admissions by default. A token
bucket permits64 immediate requests and refills16 requests per second, so the
27 fresh finalization checks do not require pacing sleeps. Requests also have
byte and I/O limits. A blocked callback retains its worker, socket,
admission and resource owner. Cancellation does not release them. One fixed join
coordinator performs actual worker joins, including thread-local destructors.
The endpoint settlement receipt excludes the coordinator's own final teardown
and retains its handle. No endpoint pathname is automatically removed; explicit
owner-serialized recovery handles abandoned paths and replacements.

## Shutdown join

The new managed family must preserve this order:

1. Authenticate the exact stop request and original total deadline.
2. Withdraw guest admissions and stop the owned process tree.
3. Join actual network, RPC, FUSE and kernel-writer settlement.
4. Freeze and capture the same view while source authorization remains current.
5. Acknowledge exact data/evidence retention and fsync the execution record.
6. Validate current source/frozen custody and send the provisional receipt.
7. The parent observes successful helper exit and performs fresh issuer/custody
   authorization before acknowledging completion and retiring source resources.

All phases consume one deadline, initially capped at thirty seconds. Queue and
transport time count. A late or failed phase withholds completion and preserves
already acknowledged data. No timeout is evidence that work stopped.

Legacy helpers return the agent exit status. The new helper contract must
separately report successful finalization; otherwise an agent's nonzero exit
cannot be distinguished from source loss after a packet was sent. Existing
execution specifications and managed bindings retain their original wire and
exit semantics. The new family requires its own qualified dispatch.


## Next managed source protocol

The local Spec5/Binding4 family selects offline remote source execution with a
protected context2. The context reference remains format1; its private payload
selects context2 and the separate host source endpoint. Context1 remains strict
for the legacy family. Neither payload is a launch grant.

Guest RPC ingress and host source authorization have separate actual lifetimes.
Guest denial or broker rotation retires ingress. Host denial retires source and
its registered session. A cached root still checks the host client's true expiry
on every use; the outer cache window cannot extend a short grant.

StopV4 starts one 30-second parent BOOTTIME budget before queue admission. The
process-local queue carries the same cancellation state. The wire carries its
constraint, which an authenticated helper adopts without restarting time.
Nonblocking framed I/O, idle control waits, RPC close and freeze use the original
deadline. Filesystem operations are fenced before and after; these checks do
not make filesystem I/O interruptible or roll back late durable effects.

A source shutdown packet remains provisional. The parent must join the exact
receipt with successful exit observed from its actual owned helper, the retained
initial child custody, and a fresh host issuer query off the App loop. Agent exit
status remains separate record DATA. Adoption checks original budget, real grant
expiry, bounded freshness and local owner retirement. The query is a point in
time; an atomic final effect with Journal revocation requires a query-and-effect
transaction, not a stronger claim about this local receipt.

The generic and legacy helpers refuse Runtime5. The Native managed helper has
an explicit Rust-only entry for Spec5, a live Binding4 controller and the owned
source factory. It checks those requirements before namespace or workspace
effects, then requires separate source/ingress services and an owned workspace
factory. Selecting a version in serialized DATA cannot supply those objects.

Binding4 readiness has passed an actual private USER/NET/pidfd check on a fresh
Fly canary; see the [readiness proof](../../examples/execution/evidence/2026-10-31-readiness4/README.md).
Its source/FUSE execution composition passed on a disposable Fly worker; see
the [source5 proof](../../examples/execution/evidence/2026-10-31-source5/README.md).
Preparation runs
off the App loop with an actual source lease and held Journal file. It retains
separate guest and source grants, charges pending activation to the original
startup budget, and checks pinned runtime bytes throughout preparation. The
source grant also covers execution and the finalization reserve. Helper custody
becomes current only after actual descriptor transfer; a provisional shutdown
packet does not retire a still-live helper's source access.

Spec5 also seals the original startup constraint into its execution profile and
managed binding. Helper preparation, namespace setup, actual exec and parent
readiness consume the same suspend-aware deadline and original wall-clock bound.
The exec callback checks an immutable snapshot with clock syscalls only. No
handoff starts a fresh allowance. Startup checks end at backend admission;
execution and finalization keep their own budgets. Spec4 omits the field and
retains its wire shape. This constraint supplies no source or process authority.

The first source5 case verifies inherited HTTPS reads, mapped writes in the
partial checkpoint, StopV4, successful helper exit, fresh issuer join and settled
source callbacks with local retention. The subsequent
[publication case](../../examples/execution/evidence/2026-10-31-source5-publication/README.md)
uses the actual captured DATA and normal transfer RPC, reconciles lost local and
remote replies, and verifies the 14-object closure through HTTPS after producer
destruction. The original independent read failed; its successful readback
continuation is recorded separately. The capture remains `live_best_effort`.
The default Native App launch has not adopted this family. Default migration,
connected models and accepted jj history remain separate gates.
