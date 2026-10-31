# Execution workspace ownership

The helper reserves and reconciles its execution before constructing services
or a workspace. The fresh Linux helper enters its private USER/MNT namespace
first; the App loop never performs these transitions. A completed record returns
historical output without invoking either factory. An uncertain prior execution
requires reconciliation.

`NewExecution` borrows the held execution lock and exact request. The one-shot
`HelperWorkspaceFactory` returns an `OwnedHelperWorkspace` containing the actual
manager, view and any additional source or transport owners. These owners remain
alive through application/RPC/FUSE drain, checkpoint retention, record fsync and
the authenticated shutdown receipt. Dropping an owner does not prove transport
drain or release an uncertain retained object set.

With no workspace factory, the helper opens the existing local view. A remote
workspace has a distinct checkpoint capability and currently fails before
layout, home seeding, mounting or backend startup. It cannot become a complete
local checkpoint through the factory interface.

## Reservation and recovery compatibility

New reservations fsync a private immutable `lock-identity.json` sidecar before
factory effects. It binds the canonical request digest, execution/operation/view
and owner identities, actual directory and lock device/inode, and a random
nonce stored in the held lock. Admission rechecks the held FD, named lock,
ownership, mode, link count, nonce and sidecar around factories and record saves.
The sidecar supplements the actual exclusive lock; it provides neither liveness
nor current project/run authority.

ExecutionSpec versions 1–3 and Record1 canonical fields remain unchanged. An old
reservation without the sidecar, a missing/replaced lock or a backup copy whose
device/inode identities changed is historical recovery-only. The helper never
repairs it into a new reservation or reruns its effects. Original records,
checkpoints and pins remain intact. `read_execution_recovery` performs a bounded
read without opening a lock, service, view, remote session or process. Old
helpers do not honor this sidecar, so this is not a physical migration fence.

## Remote source selection

`RemoteLaunchRequest` binds the expected root and attempt, reserved observation
namespace, profile, projection policy, source and accepted seed identity. A
fresh admitted `RemoteSession` must match those inputs. Metadata validation is
bounded by object count, bytes and a total startup deadline. The initial profile
requires a source-only workspace, no composed environment or inherited hardlink
groups, and the exact source directory as its runnable view.

Validation reads root metadata, policy, profile and the initial directory shard.
It does not hydrate file contents, fetch the seed or create a complete pin.
Expected source/seed identities are not evidence of current accepted history;
trusted host ingress must establish that separately before backend authority.

The opaque selection owns its actual resolver/session/cache/source chain. It
reserves the observation namespace in immutable fork bytes and writes remote
binding2 containing the full selection. Generic binding1 bytes remain unchanged.
Reopen compares the saved bytes with the actual fresh resolver; old parsers
reject binding2 before workspace operations. Namespace3 and partial-capture
namespace4 remain distinct from the binding version.

## Network preparation

The network module validates an explicit offline or bounded HTTPS CONNECT
qualification profile. Matching its IDs is configuration validation, not an
owner grant. Linux primitives retain actual child namespace FDs and verify
UID/GID1500, thread/process identity, mappings, namespace ownership, loopback and
all five capability sets. Parent namespace witnesses close before the child
owner is returned. A standalone qualifier keeps destructive transitions out of
the test harness and reports unsupported kernels explicitly.

These primitives are not yet wired into execution. There is no DNS, proxy,
provider authentication, packet forwarding or general egress qualification in
this module. Remote execution still needs joint versioned admission, controlled
networking and quiescent partial finalization before activation.
