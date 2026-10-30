# Helper-owned live workspace observations

The explicit execution v2 profile exposes a protected workspace endpoint from
the process owning the mounted `VfsView`. It does not reopen the view database,
start another scheduler or mount CAS/journal authority into the guest. Default
launch remains v1 until the managed launch/provider path is qualified.

`ExecutionSpec.workspace_observation` carries the exact `ObservationBinding`
(execution/operation UUIDs, owner/view/namespace, profile reference and source
exclusion digest) plus a short engine-selected private endpoint directory. V1
rejects a nonempty configuration; v2 requires one. Omitting the new field keeps
v1 canonical request bytes and existing execution receipts unchanged. Old helpers
reject the new field/version rather than silently running without observation.

The engine registers the execution UUID with `Journal::observe_run_attempt`
BEFORE issuing its immutable capability. The capability must grant
`workspace:observe` and exactly `workspace:<SHA256(canonical binding)>`. The
protected AgentBroker checks current project owner, run epoch, execution instance,
capability revocation and exact scope on every workspace authorization. A valid
agent token or ordinary project scope alone grants no workspace access.

The helper factory runs only after the fresh single-thread user/mount namespace
transition. Native ingress constructs a broker client pinned to the sealed
project/run/owner/run-epoch assignment; it receives no journal path. Broker
authorization has a two-second total transport deadline. A 100ms monitor makes
an observed failure sticky for this execution and cancels cooperative observation
work. Each operation still authenticates freshly. Revocation is observed within
the poll interval plus authorization completion time; this is not an atomic
transaction spanning the broker journal and view database. Publication racing
cancellation remains retained and inspectable, with no invented success receipt.
A temporary inability to prove authority fails closed and requires reconciliation.

After mounting, RPC holds a clone of that actual view. Only `workspace.sock` and
its private `identity.json` are bound read-only at `/run/rudder/workspace`.
Directory/file ownership, modes, links, exact binding and entry names are checked.
The source policy comes from the SHA-bound workspace profile. Requests cannot
choose another view, CAS root, filesystem path or exclusions. Guest environment
supplies the fixed socket/identity paths and exact expected binding. The
workspace capability is separate from the agent MCP credential projection.

Requests have a 16KiB frame limit, responses 256KiB, dirty pages 128 events, and
bounded connection/call/capture/receipt budgets. Explicit request cancellation,
full disconnect, authority loss, service closure and cooperative deadlines stop
work at bounded local checks. Read-half-close does not mean full cancellation.
BSD accepted socket flags are normalized before bounded blocking frame I/O.
Matching begin/end generations remain `LiveBestEffort`; an open file or pending
mmap/kernel write is never declared quiesced by observing a cursor.

Observation UUIDs keep their original retained result across later mutations,
disconnect and restart. Inspect never starts a fresh capture. Release is explicit
and idempotent; it cannot retire another attempt's nonce or an adopted current
root. Scoped receipt tombstones are not evicted to make a duplicate executable.
The profile/interpretation binding has its own durable CAS pin because SQLite
references alone do not retain CAS objects. Binding pins survive restart; trusted
history retirement remains a separate operation (currently at most 64 bindings
per view, with bounded observation/adoption receipts).

Adoption is unsupported in the native helper unless a trusted witness is
explicitly injected. Peer `{fsynced:true}`, a tree hash or a caller-supplied path
is not that witness. A future qualified provider must verify the exact configured
private WC state/projection under its publication guard, fsync state and parent,
and hold the guard through head CAS and old-pin retirement. It must not reacquire
a WC lock held by its caller. A failed cleanup leaves durable retirement intent
and both pins; the original adoption receipt survives retries without reverting
the current head.

Normal completion requires the existing PID-namespace/pidfd writer-drain report,
then closed RPC ingress AND all calls settled, then strict FUSE unmount/drain,
then retained checkpoint publication. Authority loss acknowledges no checkpoint.
If RPC cannot settle, controlled unmount is withheld and the view remains fenced
until helper/process cleanup. Process or transport death is an uncertain outcome,
not writer-drain proof. Uninterruptible kernel/device I/O still needs the outer
VM watchdog and explicit reconciliation.

Local checks:

```sh
cargo test -p rudder-engine --test execution_handoff --test execution_rpc --test vfs_rpc
cargo test -p rudder-engine --lib authority_monitor_tests
```

The existing explicit standalone controller also accepts `rpc-as-user <native>
<private-fixture-root>`. Its actual `TerminalPane`/sealed-FD path tests guest writes
with an open FD, original/foreign nonces, forged workspace identity, unsupported
adoption, a held partial frame during capability revocation, actual run-epoch
rotation and wrong-scope denial before guest effects. Compile it with:

```sh
cargo build -p rudder-native --bin rudder-native
RUDDER_EXECUTION_DRIVER_OUTPUT=/workspace/home/.cache/rudder-execution-rpc-qualification/controller \
  cargo test -p rudder-native --test execution_mount -- --ignored --nocapture
```

These commands do not mount or change device permissions. Real execution belongs
only on an explicitly owned disposable worker with reviewed boot-granted FUSE
access; never grant/change the current host device or restart an active home.
The parent controller must archive exact sources/binary hashes, raw results,
authorization failures and VM deletion proof before claiming mounted qualification.

The controller is the native package's `rudder-execution-qualifier` Cargo target.
Its integration test probes `CARGO_BIN_EXE_rudder-execution-qualifier` and verifies
an exact streamed copy. It never selects a library by modification time or starts
a recursive Cargo build. Non-Linux builds expose only the harmless describe and
unsupported-platform paths. The prior [frozen helper proof](../../examples/execution/evidence/2026-10-30-workspace-rpc/README.md)
retains its original binary/source hashes; the public `/usr/local/bin` guest PATH
addition and pinned-Git compatibility receive separate boot qualification.
