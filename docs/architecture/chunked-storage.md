# Chunked content and next storage contracts

This is the next implementation after the frozen alpha4 checkpoint. The
`engine::content` core adds a bounded file representation and explicit staging
adapter. It does not switch the default tree importer, enable Cloud transfer of
new kinds, provide remote hydration, or replace writable VFS backing files.

## File representation

`ObjectRef` keeps its existing meaning: kind, raw SHA256, and **encoded object
size**. New `file` objects are canonical JSON descriptors:

```rust
FileNode {
    format_version: u32, // 1
    layout: String,      // "fixed_4mib_fanout256_v1"
    level: u8,
    logical_size: u64,
    children: Vec<ObjectRef>,
}
```

Children at level zero are raw `blob` chunks; higher levels reference `file`
descriptors. Chunks are exactly 4 MiB except the last. Fanout is at most 256 and
each descriptor is at most 64 KiB. The node level is minimal for its logical
span. A branch's children cover consecutive full-capacity spans except its last
child, whose own level is minimal for the remaining span. There are no padded
single-child branches. Small subtree leaves remain composable, while the
streaming importer chooses a raw Blob for whole files of at most 4 MiB.

The 319,346,888-byte executable-sized case contains 77 chunks; the final chunk
is 579,784 bytes. Import keeps one 4 MiB data buffer and a bounded frontier.
Range reads return at most 16 MiB and fetch only intersecting chunks and their
descriptor paths. Export streams without a whole-file Vec.

`file_info` reads root metadata and reports logical length. It does not prove
the availability or retention of the whole tree. `validate_file_tree` checks all
descriptor levels and spans, memoizing repeated subtrees; ordinary complete
store publication must additionally verify all referenced chunk bytes. The pure
`validate_file_child` helper lets the store check cross-object semantics while
already holding its GC lock, without recursively acquiring that lock.

Writes use `StagedContentStore`, borrowing a caller-created lease. Every chunk
and descriptor write goes through `receive_staged`. The caller publishes the
complete root or abandons its own staging after failure. The bare LocalStore
content adapter is read-only. Active staging currently blocks all local GC;
this is conservative local retention, not a concurrent fleet GC design.

## Identities and accounting

A file descriptor digest is not the reconstructed file's raw SHA256. Chunk
digests hash raw chunks. Both legacy blobs and chunk trees can represent equal
file bytes. Whole-file digest evidence for vendor pins or REAPI must be computed
and qualified independently; requiring it in each new descriptor would force a
whole-file scan after a small CoW write.

Equal content does not imply equal inode identity. Explicit hardlink topology
decides which directory entries share a mutable inode. Their writes must share
one dirty-chunk map. Unrelated files with equal bytes stay independent.

Keep these counters separate:

- Unique reachable typed-object bytes, including metadata: the current remote
  receipt's `total_bytes` meaning.
- Successfully transferred payload bytes, distinct from all network overhead.
- Logical file bytes and logical namespace bytes, including repeated content.
- Logical inode bytes, counting explicit hardlinks once.

Deduplicated logical size can exceed reachable storage size by orders of
magnitude. New formats need separate logical-size, expanded-entry and dirty
storage quotas, in addition to object/edge/depth/transfer bounds. Logical lengths
must survive JSON without rounding; consumers must not confuse them with the
small descriptor's `ObjectRef.size_bytes`.

## Directory pages and lazy hydration

The companion directory core uses directory format 2 and typed
`directory_shard` edges. Bounded ordered byte-radix pages avoid flat directory
metadata and avoid shifting every later page after one insertion. File entries
carry logical length separately from their Blob/File reference. Publication
checks that declared lengths and page summaries match the referenced content.
Unknown metadata fields and hidden child references remain invalid.

Future resolvers must obtain a whole bounded chunk, verify it, and only then
expose bytes to a filesystem or executable mapping. A partial HTTP Range read
cannot establish a chunk hash without an additional proof. Cache hydration runs
outside the App loop and namespace/store transactions; delayed lookup results
must recheck owner/generation/tombstones before being applied.

A partial local cache is not a complete local pin. Persist a separate account-
scoped remote-base binding to an actually retained remote root. Cache presence
can be evicted; a locally complete publication retains its stronger existing
meaning. Do not put incomplete closures into ordinary local pins or weaken GC
validation to accommodate missing descendants.

The current VFS indexes inherited directories during preparation and then forks
without walking them. A remote lazy implementation must replace eager indexing
with demand-populated immutable page indexes. Its fork should retain a prepared
root binding and create private namespace state without scanning inherited
entries. The existing flat hardlink JSON topology is still a separate eager
preparation cost and needs its own indexed version before broad lazy-root claims.

## Kernel page sharing is a separate property

CAS deduplication shares stored bytes; it does not prove shared resident mapped
pages. Separate per-view FUSE inodes/address spaces can each cache the same
compiler/runtime bytes even when all reads originate from one CAS digest.

The restricted execution design's read-only bind of one host prepared
materialization can preserve the same underlying kernel inodes across worker
namespaces. Keep immutable runtime projections distinct from private writable
workspace semantics. Do not hardlink a writable view into the CAS or infer
cross-view RSS savings from digest deduplication.

FUSE passthrough is a possible later optimization, not an available guarantee.
It needs explicit qualification of the deployed Fly kernel, FUSE device/library
support, capability requirements and rootless mount profile. Measure mapped-page
sharing and multi-worker RSS separately from CAS bytes and launch latency.

## Runnable roots versus history retention

Existing CheckpointV3 `parents` are strong storage edges, and SourceManifest
parents can retain ancestry too. Traversing a new checkpoint therefore includes
history, eventually exhausting current closure budgets even if its live view is
small. Do not silently reinterpret or drop those edges for existing formats.

The explicit development path now implements a separately versioned
`RunnableRoot` v1 contract in `engine::snapshot` and the Cloud validator. Its
strong closure
contains only the current directory/content, profile, topology and ancestry-free
semantic source/environment descriptors. Project history records identify these
view roots and their parents through durable history authority, with explicit
retention policy. History pins/receipts retain old views independently of what a
worker needs to launch. Merely changing the outer checkpoint while retaining an
old SourceManifest ancestry chain does not solve the growth problem.
Audit transitive installer/provenance/build-evidence references too, so an old
checkpoint cannot pull history back into the runnable closure through another
edge. Historical evidence remains explicitly retained by history authority;
opaque blob payloads must not become a hidden, unretained substitute for edges.

The root binds quiesced consistency, current view Directory, SourceRoot v2,
optional EnvironmentRoot v2, profile/topology blobs and a u64 knowledge cursor.
It has no parents or durability fields. SourceRoot v2 binds only its directory
and projection-policy blob. EnvironmentRoot v2 admits only Blob/File/Directory
installer inputs. Publication reads the actual source/environment descriptors
and requires version 2, including when children already have retention proofs.
Durability is a separate local/remote publication receipt for the same root
identity. This format does not prove that its producer actually stopped writers;
trusted view sealing remains required. Existing formats are unchanged.

Publication must order CAS retention and journal/history adoption so a crash can
leave conservative orphan pins but cannot acknowledge history whose view is
unretained. Reconciliation owns orphan cleanup. Existing V3 roots, ancestor
durability and pins remain valid; conversion creates new roots and new history
records rather than overwriting prior receipts or changing GC semantics.

## Rollout and qualification

New readers preserve existing object hashes and accept old formats. Old readers
reject new features. Negotiate kinds, layouts, directory versions and limits
before writing. Keep the current complete-transfer limits until resumable
verification and retention authority justify increasing them.

Qualify streaming import/export and corruption rejection first, then verified
lazy range reads and directory lookups, then dirty-chunk CoW and indexed
topology/history. Default imports remain unchanged until all consumers, Cloud
validators, build identities and VFS adapters agree on the new representations.
The build frontend must continue validating actual inputs; a descriptor hash
must never be substituted for a REAPI raw-file digest.

The core's local executable-sized test passed on Linux x86_64 with Rust 1.99.1
in release mode on 2026-10-30. It generated a real 319,346,888-byte source under
`/workspace/home/.cache`, imported/published its complete local closure, checked
boundary reads, and streamed an export whose raw SHA256 matched the source.
One observed run reported:

```json
{"chunks":77,"descriptor_bytes":10420,"export_ms":802,"import_and_publish_ms":5318,"last_chunk_bytes":579784,"logical_bytes":319346888,"maximum_chunk_bytes":4194304,"maximum_export_write":4194304,"maximum_import_read":4194304}
```

These are local core measurements, not Cloud transfer, end-to-end startup or RSS
measurements. Short reads touched at most two chunks; a maximum-size read stayed
within the 16 MiB response bound. Separate synthetic frontier tests covered up
to 65,537 chunks without allocating their logical contents and checked that
repeated descriptors are read only once during structural validation.

```sh
cargo test -p rudder-engine --test content_backend
cargo test -p rudder-engine --lib content::tests::
cargo test -p rudder-engine --release --test content_backend -- --nocapture
```
