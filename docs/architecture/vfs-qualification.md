# Rudder logical VFS qualification

Status: legacy and explicit `paged_ranges_v2` Linux profiles passed root and
UID 1500 mounted canaries. Range, checkpoint, failure-recovery, live observation
and RunnableRoot build-input qualification also have focused library tests.
Production boot provisioning, default launch and native macOS
verification remain separate gates. This is a filesystem materializer over
Rudder CAS. jj remains the history authority.

## Ownership and persistence

`engine/src/vfs.rs` owns a shared append-only SQLite directory index and one
private SQLite namespace/backing directory per view. Legacy preparation walks
verified directory metadata once. Forking creates a private namespace DB and retains the
prepared root using `LocalStore::pin_existing`; it neither walks inherited
entries nor hydrates their files. Retention JSON commits still scale with the
number of retention records. The index is shared, never copied into a fork.

Inodes are allocated per view, independently of content hashes. Identical files
and identical empty directories have separate identities. Inherited lookup
allocates only touched inodes. A cross-parent directory rename updates parent
dirents and the directory's parent pointer in one SQLite FULL transaction. It
does not enumerate descendants. Original inherited paths remain separate from
current parents, so lazily loaded hardlink groups survive ancestor rename.

In the legacy profile, private file creation/copy-up syncs the backing and containing directory before
publishing its DB pointer. Ordinary writes have normal fsync semantics;
namespace acknowledgement does not promise fsync of every written content byte.
Inode handles survive rename, replacement and unlink. Zero-link backing files
are retained; there is no private-file/index sweeper yet. A missing recorded
backing prevents reopening the view. A partial fork is published only after its
DB is complete; durable input reservations prevent a retry from changing its
root, topology or owner.

The DB is local filesystem authority, separate from Rudder's project journal,
owner assignment and scheduling. Opening checks the recorded owner epoch and
holds a process lock. It is not a fleet lease or a way to assert a new epoch.
Metadata databases/private directories are trusted local state; same-UID access
outside the mount is not an isolation boundary.

## Checkpoints and hardlinks

`VfsCheckpoint` contains both a Directory ref and a hardlink-topology Blob ref.
The topology format is canonical JSON:

```json
{"format_version":1,"groups":[{"paths_hex":[["61"],["62"]]}]}
```

Groups/paths are sorted by raw component bytes, globally unique and contain at
least two regular-file paths. Paths cannot traverse symlinks and each group
must have identical file refs and modes. Equal hashes alone never create a link.
`validate_snapshot` checks this without creating an index or changing retention.
It bounds topology to 64 MiB, groups to 100,000, paths to one million, depth to
1,024, and each legacy directory metadata object to 128 MiB. The current mixed
format validator rereads legacy metadata for repeated hardlink paths; bounded
memoization is required before default use with very large legacy groups.
Caller retains roots
through validation. Checkpoint-v3's typed references retain both objects during
remote publication; transferring only the Directory loses topology.

`prepare_from_quiesced_path` imports actual `(device,inode)` hardlink groups once.
Its caller must stop source writers. A Directory-v1 import alone cannot retain
inherited hardlink identity. Symlink/directory hardlinks are explicitly rejected.
Modes and raw filename/link bytes persist; Directory-v1 does not encode original
ownership, xattrs or timestamps across a new fork.

Legacy checkpoints reconcile dirty directory paths and private file bytes into CAS,
reusing retained blobs through `publish_extending`. Topology export currently
walks visible metadata. This is not an incremental or constant-cost checkpoint.
Topology is pinned before directory publication transfers staging retention;
the DB receipt follows both roots. Seal/destroy are idempotent logical states,
retain recovery roots/private bytes and reject subsequent mutations. Destruction
is not physical erasure or scalable reclamation.

## Opt-in paged directories and private range CoW

The existing `prepare`, `fork`, `prepare_snapshot`, CLI and native macOS paths
keep their existing interpretation. New callers explicitly select
`prepare_paged`/`fork_paged` or
`prepare_paged_snapshot`/`fork_paged_snapshot`. The view descriptor and namespace
format are version 2 with `data_plane: "paged_ranges_v2"`; an old binary rejects
the new namespace. Preparing a previously retained root checks its header and
repins it. A new root still needs complete publication validation once. A
Directory-v2 root does not need the old all-entry SQLite index. Hardlink
topology still has a one-time bounded path/group preparation step.

The new path reads Directory-v1 and paged Directory-v2, and raw Blob or chunked
File data. It preserves the entry's logical length; a File ref's `size_bytes`
is its encoded descriptor length. Legacy directory conversion reads that
immediate directory once when its first change requires a paged root, retaining
child refs. It is not a free migration of legacy metadata. Explicit staged
streaming filesystem import/export and actual hardlink collection live in
`engine/src/tree.rs`; callers retain both returned refs before ending staging.
No default importer or checkout is migrated by this VFS API.
This resolver currently reads locally retained CAS objects. It does not fetch
missing chunks from S3 on page fault, and preparation does not admit an
incomplete local closure merely because a remote receipt exists. Cold-worker
lazy remote admission/retention remains a separate integration gate.

An inherited-file write creates an empty private per-inode log and appends only
the touched 64 KiB blocks. Every immutable block record has a length, offset
and raw SHA256 pointer in the private SQLite DB. The log is synced before a
FULL SQLite transaction commits those pointers, including ordinary range
`write` calls. Partial or failed transactions leave unreferenced log tails;
later appends never overwrite them. Missing, truncated or corrupt referenced
records fail closed; there is no fallback to the inherited copy. This is local
volume durability, with an explicit small-write/fsync cost and 64 KiB record
amplification. It is not a claim that the new profile makes cold compilation
faster. There is no private log compactor or sweeper yet.

File length and a truncation fence are metadata, separate from log length.
Shrinking and extending a file cannot resurrect the truncated inherited tail.
Sparse logical ranges remain zeros, and sealing shares zero subtrees by size
rather than allocating a logical-sized buffer. Reads are bounded to 16 MiB;
private block buffers are 64 KiB and encoded CAS chunks are at most 4 MiB.
Aligned unchanged File subtrees/chunks are reused at checkpoint. A changed
large legacy Blob needs a one-time streamed conversion because it has no
existing chunk refs; it still never causes whole-file copy-up.

Verified immutable Blob FDs are shared among views of one in-process manager.
The cache admits at most 128 FDs, including borrowed cache entries, and never
evicts a borrowed Arc to open a replacement past that bound. If every slot is
busy, a new admission returns `EAGAIN`. There is no user-space whole-file byte
cache. First touch verifies a Blob's complete raw hash; a large legacy Blob
can therefore remain expensive. Cached FDs assume immutable CAS files are not
changed externally, consistent with retained-root publication. Different
helper processes do not share this FD cache.

Private dirty dirents drive paged path updates. Terminal capture does not walk
untouched inherited independent files or create their inodes. Hardlink export
visits declared inherited groups and scans hydrated/private inode metadata for
linked inodes, preserving
stable inode identity after moves. Only after both CAS roots are retained does
the DB receipt rebase file/chunk and directory roots and clear the captured
dirty maps. A failed receipt preserves private blocks for retry. Total
publication is **not** currently proportional only to dirty paths:
`LocalStore::publish_extending` still traverses retained metadata closures.
It skips retained raw data hashing. Retention commits also scale with pin count.
`opendir` still snapshots a whole directory in memory; a stable paginated FUSE
directory-handle contract is an explicit remaining gate.

## Live source observation contract

`VfsView::observe_source(&ImportPolicy, request_nonce, &ObservationBudget)` is
available only on an owning paged/range view. It can run while mounted or while
logical handles are open. It never signals writers, unmounts, replaces the live
root, clears dirty state or creates a quiesced checkpoint. A separate owner
cannot reopen the mounted namespace DB to emulate this API. A future scoped
engine RPC must invoke the owner's view; native App mutation/PTY ownership
remains on its existing loop.

The returned `VfsObservation` contains the view and exact owner epoch, begin/end
`DirtyCursor`s, immutable source Directory ref, UUID request nonce, an owned
retention pin ID, explicit `LiveBestEffort` consistency and
`exclusion_policy_digest`. Each cursor binds view, owner, a persistent UUID
namespace and monotonic acknowledged-mutation generation. Cache-only lookups do
not advance it. Same begin/end generation is **not** a writer barrier: kernel
dirty mmap pages might not have reached FUSE. Only strict unmount and actual
writer teardown can authorize the existing terminal checkpoint.

Subtree exclusions are canonical raw-byte paths, sorted with duplicates and
redundant descendants removed. They omit prepared/scratch/VCS-local paths
before content reads. Their digest identifies only these exclusions. It is
not the jj tracking/projection-policy Blob: tracked paths, ignore/forced/sparse
rules, size semantics and conflicts must still be validated and applied by the
VCS provider. Mounted checkout/root replacement remains unsupported.

The observation reserves its nonce/policy/cursor before capture, publishes its
owned pin before writing a complete DB receipt, and replays the same immutable
result after later mutations. Receipt failure can recover the already pinned
original result. A pending capture with no published pin cannot silently retry
against a later generation; it reports uncertainty and requires a new nonce.
`release_observation(nonce)` durably retires the nonce before removing only its
recorded owned pin. Failures can leak retention; they do not unpin a user root.

`dirty_since(cursor, limit)` returns bounded events, continuation/current
cursors, `complete`, `has_more` and `cursor_lost`. The last 16,384 acknowledged
mutations are retained. Owner/view/namespace mismatch, a future cursor or lost
prefix is explicitly lost, never an empty clean result. Interior gaps or
malformed generations fail. Consumers must obtain a full retained observation
or report error after loss.

Default capture budgets are 100,000 changed entries, 256 MiB of content
admission and 128 MiB of metadata reads/writes. Immutable Blob verification is
charged once per capture; private reads and new encoded chunks count again.
The 30-second deadline is cooperative between bounded capture operations. It
cannot preempt a stalled local filesystem syscall, and does not bound the
separate retained-metadata publication traversal. These are capture work
limits, not an RSS or end-to-end latency guarantee.

The focused library suite passes five observation tests and eight substantive
range tests plus the subprocess child entry. It covers a 208 MiB logical File
with a 64 KiB first-write log, reuse of 51/52 chunks, 1 TiB sparse sealing,
truncate/extend fences, hardlink/open-unlink restoration, a 25,000-entry edit
and rename without untouched inode hydration, checksum tamper, process abort,
failed block-pointer commit, failed post-publication receipt and recovery,
nonce/policy fencing, retention/GC and cursor loss. The resource test verifies
shared bounded FD admission. These are not kernel mmap/fsync proofs.

`engine/tests/vfs_mount.rs` builds a fresh driver without mounting the active
host. Its existing mode exercises legacy behavior. Setting
`RUDDER_VFS_DATA_PLANE=paged_ranges_v2` selects paged fixtures and the new APIs
for the same root/UID1500 matrix, plus actual 208 MiB range writes, sibling
kernel mounts, cross-chunk mmap/msync, live observations with open handles,
checkpoint/remount and unchanged-chunk evidence. The optimized opt-in run
passed as documented below; the older mounted evidence remains specific to
its original legacy profile.

## RunnableRoot source and build-input qualification

`workspace::stage_source_complement` accepts caller-owned staging, a complete
view Directory, WorkspaceProfile and ProjectionBudget. It preserves Directory-v1
encoding and updates paged roots through only the affected radix paths. It
stages new metadata, preserves File logical lengths/modes and retains untouched
subtree refs. It does not start/end staging, publish a root, copy payload data,
change the default backend or assert a writer barrier. The producer must retain
its result through the eventual typed SourceV2/RunnableRoot publication.

`workspace::runnable_build_input_view` requires an already retained RunnableRoot
and the trusted expected SourceV2 projection-policy ref. It acquires a new
owned root pin through `pin_existing`, checks canonical manifests/profile/policy,
requires ancestry-free SourceV2/EnvironmentV2, proves exact source-complement
Directory identity, derives effective/base prepared data-tree fingerprints,
and validates the full declared hardlink topology. Missing paths, mode/content
changes, symlink text and File logical lengths enter prepared identity. Paths
through non-directories or symlinks are rejected; final symlinks are represented
without expansion. Equivalent trees encoded differently do not acquire an
implicit identity conversion; producers use the same explicit projection path.

The result is `QualifiedRunnableBuildInput`. Keep it alive while using its
borrowed `.input()`; `.release()`/Drop removes only its own temporary pin and
failed cleanup conservatively leaks retention. Cloning BuildInputView alone
does not clone that lease. Errors release the helper pin and leave caller pins
intact. The current root remains readable across GC after the caller releases
its original pin, until the qualified result is released.

The SourceV2 policy ref is an independent opaque canonical policy binding. This
adapter does not reinterpret it as WorkspaceProfile or jj tracking/ignore rules.
Exact source complement means all view bytes outside declared partitions, not
an assertion of jj tracked-tree equivalence. Quiesced metadata is not proof that
writers stopped; the trusted execution owner supplies the actual terminal
barrier. A LiveBestEffort observation is never promoted to that proof.

EnvironmentV2 does not encode a baseline hardlink topology. Prepared fingerprint
equality therefore proves data-tree equality, not equal inode topology. The
RunnableRoot topology is retained separately in BuildInputView/action identity;
a frontend must preserve/qualify it or reject unsupported linked inputs. This
adapter does not itself qualify hermeticity, materialization or compilation.
Legacy Checkpoint-v2/v3 build-input behavior remains separate and unchanged.

Projection budgets default to 4,096 partition paths, depth 256, 100,000 topology
paths, one million namespace steps, 128 MiB of admitted metadata reads and
16 MiB of newly encoded metadata. The encoded metadata cache is bounded to
32 MiB and parsed root cache to 1,024 roots; legacy parsing has additional
bounded transient memory. The 30-second deadline is cooperative. These are
admission/work limits, not a total RSS or preemptive I/O timeout guarantee.
Evidence records counters and separate pin/manifests/source/fingerprint/topology
stage times.

Eight focused tests and the 14-test build-index regression suite pass. A local
debug fixture with 25,000 source entries and two exclusions read four metadata
objects/1,707 bytes and encoded four objects/1,142 bytes for source projection.
RunnableRoot qualification read nine metadata objects/2,883 bytes, with the same
four metadata outputs and no legacy entry scan or payload reads. The recorded
samples took 3 ms for source path projection and 6 ms total qualification; these
are one-host measurements, not latency guarantees. Other cases cover 208 MiB
chunked File identity, mixed V1/V2 paths, raw names, private prepared changes,
invalid source/policy/topology, budgets, retention cleanup and GC survival.

## Paged/range mounted evidence

Public evidence is archived at
[`examples/vfs/evidence/2026-10-30-paged-ranges`](../../examples/vfs/evidence/2026-10-30-paged-ranges).
The first debug run timed out after 240 seconds during the uninstrumented
100,000-entry phase. It is retained as a failed run, not treated as a pass.
That phase included fixture construction/publication and trials before emitting
the mounted marker, so it did not isolate fork or header preparation cost.

The subsequent frozen release driver used the repository thin-LTO release
profile and rustc `opt-level=3`. It passed the complete root and UID/GID 1500
matrix, with no excluded entries, on Fly.io `rudder-workers-exla`, Machine
`813212b92535d8`, instance `01M66Q10TJSFSWDF1K38M94JR4`, `iad`, two shared
CPUs, 2 GiB, kernel `6.12.114-fly`. The Debian bookworm-slim image digest was
`sha256:a3bba7dc7dbcc1f37c2856c51fc9a7fac96608f50cb87b1b4e4d8b8c85ae988e`.
There were no production services, secrets or volumes. Deletion and absence
were verified at `2026-10-30T15:32:54.859Z`. The frozen driver SHA256 was
`a3e912699db1a01e1f7986993bbaaa9cae98b91e282fd76a5502f740a417a078`.

| UID | Inherited entries | Retained-header prepare | Fork median | Rename syscall median |
| ---: | ---: | ---: | ---: | ---: |
| 0 | 1 | 7.19 ms | 15.51 ms | 5.59 ms |
| 0 | 100 | 1.67 ms | 15.81 ms | 7.38 ms |
| 0 | 10,000 | 2.22 ms | 21.33 ms | 6.55 ms |
| 0 | 100,000 | 2.12 ms | 12.34 ms | 5.27 ms |
| 1500 | 1 | 2.13 ms | 13.11 ms | 5.24 ms |
| 1500 | 100 | 6.43 ms | 68.04 ms | 8.44 ms |
| 1500 | 10,000 | 1.68 ms | 15.04 ms | 6.43 ms |
| 1500 | 100,000 | 2.44 ms | 14.24 ms | 5.53 ms |

Fork/rename medians are five samples per count/UID from one shared VM. Each
inherited-directory rename left exactly three cached inodes. This is source-tree
size independence for these operations, not a fleet percentile or constant
retention-record cost. Preparation above starts from an already published root.
Fresh 100,000-entry fixture build took 5.90 s root/6.49 s UID1500; its initial
publication took 4.76 s/4.63 s. The enclosing parent fixture also required
publication. Import/publication costs were not hidden inside the millisecond
fork result.

Both UIDs also passed the independent 218,103,808-byte (208 MiB) File matrix.
A seven-byte write created 65,536 private bytes; a later cross-chunk mmap/msync
edit brought that to 131,072 private bytes. The sealed File reused 50/52 original
chunk refs, restored both changes after remount and preserved sibling isolation
through real kernel mounts. Mounted live observation with an open file handle,
strict busy-unmount, detached-descendant reaping, hardlink/raw-name/mode/atomic
save checks and server-abort recovery passed. Device access was delegated only
inside the disposable VM. None of this proves default product launch,
end-to-end dirty-only publication or missing-chunk remote hydration.

## Linux mount and shutdown contract

Linux uses exact `fuser = 0.18.0`, `default-features = false`, no extra features
or libfuse package dependency. Its MSRV is Rust 1.85. It requests cached
write-through, without `FUSE_WRITEBACK_CACHE`; shared mmap remains enabled.
Namespace mutation is serialized by the VFS DB mutex. This is independent of
the native App's single-thread scheduling/PTY ownership.

The direct mount API supports root in a new private mount namespace, or a
non-root helper in a new user namespace with exactly its own UID/GID mapped to
themselves, then a new recursively private mount namespace. Its guard is minted
in a fresh single-thread helper and bound to the current namespace identities
and UID. It opens `/dev/fuse`, mounts directly and uses `Session::from_fd`;
it never searches for or invokes a setuid mount helper.
Only an empty mountpoint under this manager's `mounts/<view UUID>` is accepted.
No permission change, setuid installation or host mount is performed on the
active worker. Agent/process isolation still requires the workspace helper's
PID namespace and writer-drain lifecycle; FUSE does not create a sandbox.

Live checkpoints are rejected. Shutdown syncs the mount, performs normal
`umount2(flags=0)`, then joins the FUSE server. Busy handles return `EBUSY` and
keep the checkpoint guard closed. A descendant can clone its mount namespace,
so the final server join has a ten-second deadline. A timeout retains the fence
until the server confirms shutdown. fuser's convenience unmount uses lazy detach;
it is deliberately insufficient to authorize a checkpoint. Error/drop cleanup
never converts an uncertain or lazy detach into a consistency proof.

The tested boot profile sets `/dev/fuse` to `root:1500`, mode `0660`, only inside
the disposable VM. The server runs as UID/GID 1500 with maps `1500 1500 1`; it
gains no initial-user-namespace mount authority. A fresh exec after dropping
root was necessary in the fixture; an in-process UID drop returned `EACCES`
while configuring namespace maps. No permissions changed on the active worker.
Production boot provisioning still needs wiring and review before default use.

This is not a per-run credential or filesystem isolation boundary. Another
process with UID 1500 has the same host-file permissions as the engine,
including authority DBs, peer credentials and writable CAS outside this mount.
Production launch needs a restricted root with selected runtime and own-view
binds, scoped credentials, and authority state outside that root. Denial of
different-UID root files and initial namespace access does not prove same-UID
isolation. The per-account Fly VM boundary is separate again.

Native macOS keeps its existing native/APFS
path; this work does not install macFUSE or claim Mac qualification.

## Evidence

Core tests use disposable paths under `RUDDER_TEST_TMPDIR`, `/workspace` when
present, or the platform temp directory. They cover fork/sibling
isolation, independent equal-content inodes, raw bytes, link topology and
ancestor rename, no descendant lookup on inherited rename, replacement/unlink
with live handles, mode/timestamp behavior, missing private data, wrong owner,
SQL failure rollback, retained roots after GC, and fsync/namespace recovery after
process abort. The final local run passed 15 substantive tests; one optional
logical scaling test is skipped in normal runs because real mounted scaling is
qualified separately. The standalone driver compilation test passed too.
`vfs_mount.rs` is Linux-only and compiles the fresh driver without
mounting on the active host.

Final mounted canary: Fly.io `rudder-workers-exla`, Machine
`836e60b7731758`, instance `01M66BVDR6TJRJHRR2SBS490B5`, `iad`, two shared CPUs,
2 GiB, kernel `6.12.114-fly`.
Image: Debian bookworm-slim
`sha256:a3bba7dc7dbcc1f37c2856c51fc9a7fac96608f50cb87b1b4e4d8b8c85ae988e`.
No services, volumes or app secrets. VM destroyed and absence verified at
`2026-10-30T12:18:26Z`. Root and actual UID/GID 1500 runs both passed. The
non-root profile used only `1500 1500 1` mappings; missing device permission
failed closed before provisioning. The initial PID 1 mount namespace remained
unchanged. A detached `setsid` descendant holding a file caused busy-unmount
rejection; after killing and reaping it, normal shutdown succeeded. FUSE-server
abort preserved acknowledged namespace changes and fsynced data.

| Inherited entries | Prepare once | Fork median | Rename syscall median | Cached inodes after rename |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 5.3 ms | 41.5 ms | 9.02 ms | 3 |
| 100 | 29.5 ms | 43.5 ms | 5.39 ms | 3 |
| 10,000 | 2.48 s | 10.5 ms | 5.29 ms | 3 |
| 100,000 | 26.56 s | 42.8 ms | 6.08 ms | 3 |

These are medians of five UID 1500 samples per size from a shared VM, not
population percentiles or latency guarantees. Across those samples, fork took
9.95–46.54 ms and rename 4.80–12.98 ms. Shared CPU scheduling and retention-record
count affect timings. The fixed three-inode result and operation structure,
rather than a claim of identical wall time, establish no descendant traversal.
Inputs were already in local CAS; one-time preparation/indexing is explicitly
separate from fork. Rename uses `rename(2)`, never `mv` or a recursive
copy fallback. Functional checks include mmap/msync/fsync, private writes, raw
names/symlinks, chmod, atomic saves, runtime and restored hardlinks, live-file
unlink, strict busy-unmount rejection and recovery after FUSE-server abort.
The final driver binary SHA256 is
`61b9edc4a8303e812685ef4c985d57363e84401b8ea32b441280550fedebee33`.
Its report records exact matching hashes of all VFS source/test/CLI files.
Raw report and scripts are handed to the parent for the evidence archive at
`/tmp/rudder-materializer-research/custom-vfs-rootless-final-report.json`.
Native CLI smoke at `2026-10-30T12:21:31.311876Z` passed real hardlink import,
one-inode fork, checkpoint roundtrip, seal/destroy and unchanged input, then
removed its fixture. Native check/build also passed with existing warnings.

## Opt-in remote partial roots and bounded directory cursors

`VfsManager::open_remote` requires a fresh protected `RemoteSession` and the
exact expected `RunnableRoot`. Startup reads bounded root/source/environment,
profile/topology and root-directory metadata, then `fork_remote` creates a
private view without a descendant walk. Inherited paths resolve through opaque
membership proofs rooted in that session; arbitrary local CAS presence grants
no membership. Empty inherited hardlink topology is qualified; runtime links
work, and nonempty inherited topology requires a separately qualified bulk path.
The private LocalStore never acquires a complete pin for a partial remote tree.

The original root and UID 1500 mounted range qualification is archived at
[`2026-10-30-remote-ranges`](../../examples/vfs/evidence/2026-10-30-remote-ranges).
It fetched only two requested 4 MiB chunks from a logical 208 MiB file, preserved
private 64 KiB CoW and sibling isolation, and strictly discarded an invalidated
mount without a checkpoint. That earlier profile capped whole-directory
listings; the new cursor integration below is separate qualification work.

Remote namespace format 3 adds `read_directory_page(inode, handle, cookie,
max_entries)`. The Linux adapter consumes at most 256 entries per page and
preserves cookies when its reply buffer stops midway through a page. An open
cursor freezes its immutable base and private mutation overrides at the first
readdir, including `..`; seek and rewind use that snapshot. Close and reopen
observe the current namespace. Ordinary local namespace 1/2 handles retain
their previous eager snapshot behavior. Remote namespace 2 is explicitly
unsupported by this new path until a migration is supplied; old clients fail
closed on namespace 3.

The pager merges raw byte names from bounded radix pages and an SQL snapshot
of private mutation witnesses. It never constructs a whole inherited listing,
fetches File descriptors/Blobs to list names, or turns declared entry hints into
attributes. A durable origin-to-inode reservation shares the namespace's
AUTOINCREMENT allocator with creates. Equal-content files and equal empty
directories remain distinct; actual lookup/getattr validates the immutable
child and materializes that same reserved inode. Inherited origins remain stable
through directory moves. Runtime link/unlink identity keeps its existing rules.

Defaults are 32 live initialized cursors, 131,072 cookies per cursor, 524,288
cookies overall, 65,536 private overrides per cursor, 131,072 overrides and
64 MiB of logical override bytes overall. The reconstructible cursor SQLite DB
has a 128 MiB `max_page_count` cap, DELETE journaling and no WAL. Its rollback
journal is bounded by the DB's touched pages plus journal headers, so transient
physical usage can approach twice the DB cap. Cursor state uses synchronous OFF:
process restart discards it under the exclusive view owner lock. It is never
claimed as durable source data or authority.

Persistent inode reservations have separate limits of 524,288 identities and
128 MiB of logical identity/name bytes; SQL/index overhead is additional. These
identities are not swept or reused. Reservation batches commit with namespace
SQLite FULL before cursor commit/response. The namespace WAL is truncated after
each new reservation batch, preventing listing-only accumulation across pages.
There is no cross-DB atomicity claim: a cursor commit failure can leave bounded
unused durable identities, but never an acknowledged cursor pointing to an
undurable identity. Quota failure rolls back the cursor transaction and preserves
prior cookies. Close reclaims cursor quotas; restart clears reconstructible cursor
files only. Metadata request/page budgets and deadlines return explicit errors,
never a false empty/complete listing.

Focused tests cover over 100,000 entries, raw prefix/terminator ordering, tiny
pages and midway resume, rename/unlink/link/create during an open cursor,
wrong/unknown cookies, close/reopen, physical DB and logical quota rollback,
child-summary mismatch before attributes, revocation plus cleanup, and real
`_exit` immediately after an acknowledged listing. The release library sample
listed 100,101 inherited entries in 13,268 ms, using 392 namespace reservation
commits and 392 ephemeral cursor commits, 1,114 metadata requests, no File/Blob
fetches and a 5,251,072-byte cursor DB. Scanning accounted for 10.711 s, namespace
FULL commits 0.869 s, identity SQL 0.530 s, cookie SQL 0.546 s, WAL checkpoint
0.288 s and cursor commit 0.044 s. This is one generated, repeated-reference
fixture on the development host, not a mounted or production S3 latency claim.
The subsequent [mounted cursor qualification](../../examples/vfs/evidence/2026-10-30-remote-readdir/README.md)
passed as root and UID 1500 with the final helper source. Both listed 100,101
entries through real `getdents64`, including tiny buffers, returned-cookie seeks,
rewind after namespace changes and close/reopen. First listing took 25.579 s and
24.564 s, with no File/Blob fetches and a 5,464,064-byte cursor database. These
are separate cold mounted samples, not a production S3 latency claim. All 182
qualified source inputs matched the checkout; the disposable machine was
deleted and absence verified at `2026-10-30T20:16:11Z`.

Remote checkpoint, seal and live source observation still fail explicitly with
`EOPNOTSUPP` before staging/publication. The typed partial-delta retention layer
must be integrated with a bounded VFS write collector before these become
available. This pager adds no default agent launch, VCS or native macOS switch.

## Candidate findings and primary references

- Current [fuse-overlayfs source at 9afb0e41](https://github.com/containers/fuse-overlayfs/blob/9afb0e41dae1d5ea782ff1aa2806729f746d60fc/src/overlay.rs)
  still returns `EXDEV` for inherited/merged directory rename. Released 1.18
  returned `EXDEV` in a separate root Fly canary for 1/100/10,000 children.
  Rejecting the 10,000-child operation took 251 ms; no upper entries were copied.
- [Nydus lock at a95114c5](https://github.com/dragonflyoss/nydus/blob/a95114c5a69b3068a05a43305f8aa863d1d9ac3b/Cargo.lock)
  pins `fuse-backend-rs 0.14.0`. The official
  [crate source](https://static.crates.io/crates/fuse-backend-rs/fuse-backend-rs-0.14.0.crate)
  has unconditional `EXDEV` in `overlayfs/sync_io.rs::rename` and lossy UTF-8
  conversions in lookup/create/symlink. Its writable overlay is unsuitable for
  this contract; RAFS remains relevant for immutable prepared inputs.
- [Buildbarn's directory implementation at 1fbffdc4](https://github.com/buildbarn/bb-remote-execution/blob/1fbffdc417797f6afdb7f67b1f0d5cc27352db43/pkg/filesystem/virtual/in_memory_prepopulated_directory.go)
  moves directory-child references under locks, but its directory metadata is
  explicitly in-memory. Its immutable CAS leaves and pool-backed mutable files
  are useful references, not a persistent full-environment fork engine.
- [fuser 0.18 crate](https://static.crates.io/crates/fuser/fuser-0.18.0.crate)
  supplies the checked feature/MSRV, Session and unmount behavior.
- [Linux FUSE I/O modes](https://docs.kernel.org/filesystems/fuse/fuse-io.html)
  distinguish cached write-through/shared mmap from writeback acknowledgement.
- [Linux user namespace rules](https://man7.org/linux/man-pages/man7/user_namespaces.7.html)
  cover capability scope, self mappings, and proc-map ownership after credential
  changes. [Linux 6.12 FUSE registration](https://raw.githubusercontent.com/torvalds/linux/v6.12/fs/fuse/inode.c)
  marks FUSE with `FS_USERNS_MOUNT`. Device access remains a separate prerequisite.
- [Fly Machines API](https://docs.fly.io/machines/api/machines-resource)
  documents isolated Machine configuration and lifecycle. Machine exec has a
  shorter effective synchronous timeout here; canaries use bounded background
  execution with short polling, finally destruction and absence verification.

Open gates: reviewed non-root worker boot provisioning, restricted agent root
and credential/CAS isolation, PID namespace/agent launch
integration, real package manager/compiler workloads, broader POSIX/tooling
stress and Mac CI. Whole-file CAS/copy-up is not block CoW or lazy remote chunk
hydration. No claim of completion for those gates follows from this canary.
