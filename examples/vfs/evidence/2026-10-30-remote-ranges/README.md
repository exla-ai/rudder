# Remote paged/range VFS qualification

Verified 2026-10-30 at 17:40:27.022 UTC on an owned disposable Fly.io VM,
`080050dbed0758`, instance `01M66YBMX435F3AVYQ1B3Q6VZQ`, in `iad`.
Both root and UID 1500 passed. The VM was deleted and a subsequent machine
inventory verified its absence. It had two shared CPUs, 2 GiB RAM, no services,
no volumes and no production environment or account credentials.

Runtime: Linux `6.12.114-fly`, Node `22.23.3`, image
`node:22-bookworm-slim`, resolved digest
`sha256:efd0ab5780c2d9ab1f0f869571a00d5edb17793bff4cce4a2792e3eb0ffc7562`.

| Measured stage | Root | UID 1500 |
| --- | ---: | ---: |
| Prepare generated 100,000-entry fixture | 2,330 ms | 2,180 ms |
| Remote constructor and private view fork | 55 ms | 49 ms |
| Large-file payload requests | 2 | 2 |
| Large-file payload bytes | 8,388,608 | 8,388,608 |
| Partial cache bytes | 8,429,085 | 8,429,085 |
| Private bytes after 7-byte write | 65,536 | 65,536 |
| Private bytes after second-chunk mmap/msync | 131,072 | 131,072 |
| Complete LocalStore pins | 0 | 0 |

These are individual release-build samples. The file is 208 MiB logically;
only its two requested 4 MiB chunks were fetched. The fixture repeats content
references across files and chunks. Its compression/deduplication ratio is not
a claim about arbitrary SDKs. A separate six-byte nested file was also read.
Each case made 17 object HTTP requests including required metadata and that
small file; it obtained two fresh root receipts for the two sibling views.

The mounted path used the actual persistent Node helper, authenticated
loopback HTTP fixture, protected remote-root session, partial object cache,
private SQLite namespace and Linux FUSE adapter. The HTTP fixture contains
generated public data. This is neither a production S3 test nor a measurement
of network latency between workers.

The matrix passed sibling isolation, actual mmap/msync/fsync contents,
inherited directory rename with stable original lookup paths, moving a child
into a private directory, runtime hardlink inode identity, unlink with an open
FD, busy strict unmount followed by successful retry, remount, exact `ESTALE`
on a new lookup after attempt invalidation, and strict discard. Discard returns
no checkpoint proof. Checkpoint and source observation returned `EOPNOTSUPP`;
the local CAS received no inherited tree and acquired no complete root pins.

UID 1500 entered a fresh executable before creating its user/mount namespaces.
It was denied inspection of PID 1's mount namespace; the outer root controller
verified that namespace remained unchanged. Device group 1500/mode `0660`
was provisioned only inside this disposable VM. No setuid helper was installed.

The first attempt passed the root matrix but failed the UID 1500 qualifier's
in-place credential-change bootstrap. Its VM `080d6e2b333778` was deleted and
absence verified at 17:36:28.589 UTC. `initial-user-bootstrap-failure.json`
preserves that failure. The fixed qualifier drops credentials and re-execs
before entering the user namespace.

The opt-in profile supports empty inherited hardlink topology; a nonempty
topology requires an explicit qualified bulk path. Runtime hardlinks work.
Directory open is lazy; listing is currently capped at 4,096 entries by default
and oversized listings return `EOVERFLOW` until paged kernel readdir exists.
The source-only fixture does not exercise VCS integration, build-cache reuse,
restricted agent execution or a default launch switch. Checkpoint/observation
remain gated pending typed partial-delta capture and retention integration.

The compiled qualifier SHA-256 is
`7175aa9507d25259161a862d669aea8609b57795847378a314941f12d3d0fd1d`.
The original build-envelope SHA-256 embedded in that binary is
`c0f45f10a1fabaee7a5cbaf646a4e1c2dc5c01e93683c08e0a027bb02e8b9c0f`.
The original workspace manifest and Cargo.lock were preserved, and frozen
input hashes were checked after the release build. The controller verified
the transferred payload hash. These original binary and runtime witnesses
have not been changed.

A subsequent member-name audit found that the broad source capture had also
included an unused runtime session file under `native/src/.ledger/`. No session
content was opened during the audit. The published archive and source manifest
were sanitized using a strict source allowlist, and the capture script now
applies that allowlist before reading, hashing or copying any file. The original
runtime files and private qualification snapshots were left untouched.

`source.tar.gz` now contains 175 exact allowlisted source/support files from
the frozen copy, including the Python compile-time inputs and helper TypeScript.
It supports the qualified engine target; it is not a complete source bundle for
every native dashboard test/platform. `source-sha256.json` describes this
sanitized public subset, including helper inputs. Its SHA-256 is
`bfc6be3949b156c4325a6b69c7e22664e515e003b36bd418780ab5092a50bcad`;
it deliberately differs from the original build envelope embedded in the binary.
`fly-mounted.json` names these two provenance hashes separately.

The compiled binary and payload archive are omitted. Hidden/runtime/cache files,
nested earlier benchmark evidence, and non-build documents/scripts outside the
allowlist are omitted; `archive-omissions.json` records classes and counts without
publishing runtime names or hashes. The earlier paged-range source archive and
its manifest were independently checked by member name/key and required no changes.

`helper-input-sha256.json` describes helper bundle inputs. `fly-mounted.json` and
`mounted-output.log` contain the unchanged measured results. `controller.mjs`,
`build.py` and `guest-canary.sh` preserve the driver flow; `build.py` includes the
subsequent source-capture sanitation fix. Use only a newly owned disposable VM.
The controller serves fixtures through private Fly 6PN, restricted to that VM's
source address and nonce, and closes the listener during cleanup.
