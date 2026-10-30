# Rudder logical VFS qualification

Status: implemented Linux adapter, root and UID 1500 mounted canaries qualified;
production boot provisioning and native macOS verification remain separate gates. This is a
filesystem materializer over Rudder CAS. jj remains the history authority.

## Ownership and persistence

`engine/src/vfs.rs` owns a shared append-only SQLite directory index and one
private SQLite namespace/backing directory per view. Preparing walks verified
directory metadata once. Forking creates an empty namespace DB and retains the
prepared root using `LocalStore::pin_existing`; it neither walks inherited
entries nor hydrates their files. Retention JSON commits still scale with the
number of retention records. The index is shared, never copied into a fork.

Inodes are allocated per view, independently of content hashes. Identical files
and identical empty directories have separate identities. Inherited lookup
allocates only touched inodes. A cross-parent directory rename updates parent
dirents and the directory's parent pointer in one SQLite FULL transaction. It
does not enumerate descendants. Original inherited paths remain separate from
current parents, so lazily loaded hardlink groups survive ancestor rename.

Private file creation/copy-up syncs the backing and containing directory before
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
1,024, and cached verified directory metadata to 128 MiB. Caller retains roots
through validation. Checkpoint-v3's typed references retain both objects during
remote publication; transferring only the Directory loses topology.

`prepare_from_quiesced_path` imports actual `(device,inode)` hardlink groups once.
Its caller must stop source writers. A Directory-v1 import alone cannot retain
inherited hardlink identity. Symlink/directory hardlinks are explicitly rejected.
Modes and raw filename/link bytes persist; Directory-v1 does not encode original
ownership, xattrs or timestamps across a new fork.

Checkpoints reconcile dirty directory paths and private file bytes into CAS,
reusing retained blobs through `publish_extending`. Topology export currently
walks visible metadata. This is not an incremental or constant-cost checkpoint.
Topology is pinned before directory publication transfers staging retention;
the DB receipt follows both roots. Seal/destroy are idempotent logical states,
retain recovery roots/private bytes and reject subsequent mutations. Destruction
is not physical erasure or scalable reclamation.

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
