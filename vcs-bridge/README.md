# Pinned Rudder jj bridge

This separate crate uses **jj-cli/jj-lib =0.40.0** through their public APIs.
It does not replace the installed `jj`, mutate a user's main history, or enable
a VFS execution profile. Default features exclude jj and its CLI.

```sh
cargo test -p rudder-vcs-bridge --features local-development
cargo run -p rudder-vcs-bridge --features cli -- rudder-capabilities
```

`local-development` additionally exposes `rudder-init-local`. Its distinct
`rudder-local-development-v1` type delegates to LocalWorkingCopy and scans
files. It exists for compatibility qualification. The production
`rudder-vfs-v1` factory uses an injected `WorkspaceView`, never that scanner.
The CLI does not activate an unconfigured production provider.

The [architecture note](../docs/architecture/vcs-bridge.md) records contracts,
evidence and remaining gates. The earlier standalone scanner prototype is
preserved separately under [evidence](../examples/vcs/evidence/2026-10-30/README.md).

## Private metadata and history

`prepare_metadata_template` accepts an already quiesced, trusted private
colocated repository containing only authorized history. It copies VCS data,
strips Git/jj config and hooks, rejects external Git targets/alternates and
symlinks, and records file checksums. It never enumerates source/dependencies.
It is not an arbitrary-main-checkout snapshot or untrusted metadata importer.

`VerifiedMetadataTemplate::open_retained` links that manifest to a currently
retained CAS Directory, checks all shared object files against the manifest,
and requires an engine-owned read-only object mount. Verification/preparation
is performed once per cached seed; metadata forks copy only private VCS state.
The engine keeps the seed pin and mount alive through live and recoverable
attempt metadata. This is not an O(1) metadata-fork or cold-fleet benchmark.
The explicit development constructor reports the weaker integrity-only mode.
macOS read-only mount execution is not yet qualified.

`fork_private_attempt` additionally creates a fresh WC change ID and private
operation using jj-lib, preserving the exact baseline's commit/change identity
and ancestry. It resets only Rudder's private WC metadata and never calls
snapshot, checkout or `jj workspace add`. A repeated initializer reconciles its
existing tagged private operation. Launch requires `rudder-attempt-ready.json`,
not merely the existence of copied metadata. APFS/OverlayFS metadata CoW remains
a later optimization; the current copy is proportional to VCS metadata files.

## Observations and seals

`LiveObservation` carries owner/view, cursor namespace and generation range,
request nonce, immutable visible Directory, retained pin and explicit
`LiveBestEffort` consistency. `from_vfs` checks the actual VFS cursor scope.
The VFS exclusion digest and full jj projection-policy Blob are different.
Generation equality does not prove mmap/page-cache quiescence. The provider
must apply tracked/ignored/force/sparse/size/conflict rules and handle cursor
loss with a complete retained observation or an error.

`SealedSourceProof` version 1 carries a Checkpoint or explicit RunnableRoot,
versioned SourceRoot, optional EnvironmentRoot, exact merged-tree terms/labels
and attempt/profile/seed/projection binding. Hashing it proves identity, not
the existence of a barrier or source-to-jj byte equality. The engine verifies
actual stored recovery source/environment and available view/profile/topology,
including RunnableRoot child versions and SourceV2 policy. This first gate
rejects an unproven source-subset derivation.

`project_source` now maps actual immutable Directory v1/v2 entries to pinned jj
trees, including files, executable bits, UTF-8 paths/targets and file conflicts.
It evaluates current ignore/auto-track/force/size options. It traverses metadata
and reuses unchanged leaf values from an owned receipt; it is not an O(changes)
namespace implementation. `materialize_source` prepares the reverse immutable
CAS tree, retaining exact conflict markers and labels. No source walk occurs.

`VfsWorkspaceView` owns an actual VfsView clone, a trusted attempt-authority
check and engine-private projection receipts. It observes through the owning
VFS API, never by reopening a mounted DB. `bind_prepared_working_copy` verifies
an actual nonempty private WC commit before switching its type. Lost baseline
readiness/adoption receipts reconcile the same persisted nonce. `finish`
fsyncs WC state before adopting the new pin and releasing the old one. These
are host-side APIs; a guest cannot assert that it fsynced and release pins.
Scoped RPC/native caller qualification remains separate work.

`verify_versioned_source` recomputes all CAS→Git mappings with leaf-cache reuse
disabled. It returns an opaque `VerifiedSourceProjection` only after pinning a
canonical `source-jj-projection-v1` ActionResult. This is mapping evidence, not
proof of a writer barrier or test execution. The caller includes it in accepted
intent evidence before `prepare_verified_operation`. Prepared record format 2
binds that receipt and the exact private jj tag; legacy format 1 keeps its
canonical encoding. The receipt belongs to history/journal retention and is
never added as a strong edge from RunnableRoot.

The workspace's strict source-complement gate is unchanged. A jj-selected
subset is not automatically a valid WorkspaceProfile source root. Explicit
trusted derivation remains required for that case. Tracked source inside an
environment/scratch exclusion is rejected before observation. Non-UTF-8 jj
names/targets, symlink `.gitignore`, non-file conflicts, submodules and live
checkout fail explicitly. Reverse jj reads have a bounded full-blob profile;
forward normal-file writes stream, including large legacy Blob/File objects.

Production checkout/sparse capabilities default to Unsupported. A same-tree
metadata transition needs no materialization only with a current observed
cursor; metadata reset requires a fresh source snapshot first. Returned source
proof is checked before any pending checkout nonce can complete. For qualified
external mutations,
a durable private nonce precedes the call; an uncertain result cannot be erased
by `finish` or replayed. Explicit pending-checkout recovery remains a gate before
enabling live checkout.

## Integration intent

The engine's journal owns `begin_vcs_intent`, retained inputs and source/owner/
decision fences. Only after its durable Accepted receipt may the bridge prepare
a jj transaction. `prepare_operation` preserves canonical ancestry and sets a
private `rudder-integration` bookmark, producing an unpublished operation tagged
with intent/base/commit/proof identities. A staged immutable record is attached
and pinned before publication. CAS and SQLite do not share a transaction.

`JournalJjPublisher` is the trusted synchronous adapter for a background helper.
The caller holds the engine-private integration-repository lock. Never execute
publication or a long DB write wait on the native App actor. Journal Started is
committed before external work; a second IMMEDIATE transaction holds the final
owner/source-attempt/head/full-decision gate across exact op-head update and
receipt commit. Inspection never auto-merges/rebases divergent operation heads.
Legacy integration cannot bypass uncertain VCS effects or steal reserved keys.

Crash-left Started becomes Uncertain through explicit startup recovery. Inspect
the same operation; never replay it. An exact already-Published operation can be
adopted historically by the current owner while base/decisions still match.
Published with changed verification gates stays Uncertain and blocks integration.
An explicit current-owner reverify/adopt or compensating-operation workflow is
still required for that case; it must inspect actual published content and use
fresh evidence. Pins remain conservatively retained, including after a failed
cross-store boundary. Orphan/recovery retention cleanup needs engine policy.

## Qualification scope

Rust tests exercise the actual registered WC with a test provider, raw identity
validation, sanitized private forks, real jj unpublished/published operations,
and actual jj/journal normal/lost-receipt paths with legacy empty and verified
nonempty source trees. Further fixtures cover modes, symlinks, tracking rules,
long-marker partial conflicts, file/directory replacements, corrupt CAS reads,
streamed files and owning-VFS pin adoption, including a lost adoption receipt.
They do not prove kernel quiescence, distribution or macOS mounts.
Engine tests add a real subprocess exit after effect/before DB commit, current
source/owner/version fences, actual recovery-root field checks and a decision
writer racing the final publication gate. The CLI fixture runs real native
commands/conflict/header/legacy-ID round trips in the scanner profile.

The explicit Linux RO fixture is ignored by ordinary tests because it mounts
only inside a caller-created user/mount namespace. It was separately run with
an original-namespace witness, `EROFS` probe, two independent fresh WC changes
and zero provider snapshot/checkout calls. It never mounts over a checkout.
All sizeable Cargo targets and disposable fixtures should be on `/workspace`.
