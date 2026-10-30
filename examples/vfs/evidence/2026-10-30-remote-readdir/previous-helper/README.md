# Paged readdir mounted proof with the previous frozen helper

Root and UID 1500 both passed on owned disposable Fly Machine
`85192db2361d98`, instance `01M676TZB0VGDGG17866PM30RX`, in `iad`.
The machine was deleted and absence verified at
`2026-10-30T20:10:24.640Z`. It had two shared CPUs, 2 GiB RAM, no services,
no volumes and no production environment or credentials.

Runtime: Linux `6.12.114-fly`, Node `22.23.3`, `node:22-bookworm-slim`,
resolved image digest
`sha256:efd0ab5780c2d9ab1f0f869571a00d5edb17793bff4cce4a2792e3eb0ffc7562`.

| Measured first listing | Root | UID 1500 |
| --- | ---: | ---: |
| Entries | 100,101 | 100,101 |
| Listing time | 25,428 ms | 24,189 ms |
| getdents64 calls, including EOF | 985 | 985 |
| Directory metadata requests | 1,114 | 1,114 |
| File descriptor or Blob fetches during listing | 0 | 0 |
| Cursor SQLite bytes after listing | 5,464,064 | 5,464,064 |
| Root construction and private fork | 53 ms | 81 ms |
| Large-file payload bytes in the later range matrix | 8,388,608 | 8,388,608 |
| Final private CoW bytes | 131,072 | 131,072 |
| Complete LocalStore pins | 0 | 0 |

The driver uses actual `getdents64`, two initial 512-byte buffers, subsequent
32 KiB buffers, and `lseek` on returned cookies. It verifies byte ordering,
complete records, a mid-page seek, rewind with the same open snapshot after
rename/unlink/create, and the current namespace after close/reopen. It also
asserts that names do not materialize child attributes and that closing handles
releases all cursor/cookie/override quotas. Recorded stage counters describe
the first open listing; the later close cleanup is an explicit assertion.

Both runs also passed mmap/msync/fsync, 64 KiB private CoW blocks, sibling
isolation, inherited directory rename, runtime hardlinks, an unlinked open FD,
strict `EBUSY` unmount and retry, remount, `ESTALE` after invalidation and strict
discard. Checkpoint and source observation remained `EOPNOTSUPP`; discard did
not manufacture a checkpoint proof. The 208 MiB logical file fetched only two
4 MiB chunks. This generated fixture repeats references and measures one
release-build sample over the persistent Node helper and loopback authenticated
HTTP fixture. It does not measure production S3 or inter-worker latency.

UID 1500 used a fresh executable and private user/mount namespaces. PID 1's
mount namespace was inaccessible to that UID; the outer root script verified
that it remained unchanged and both owned fixture directories were removed.
Device group 1500/mode `0660` was provisioned only inside this disposable VM.
No setuid helper was installed.

The exact Rust/support envelope contains 178 files with embedded SHA-256
`c9e0e30ed75f8c325c4b34f8f0ddab8bdab41643197e150e66c69ecff0b6f16f`.
The qualifier SHA-256 is
`5d8256b3ffa8aae7719cc88eb4ea2bab9f6dbd811c968c144f139811394d27af`.
The 182-file source-only archive adds four separately hashed helper inputs;
its SHA-256 is
`42a885ae171146b579f6a49dea224c767f2881e1d461c047f2265b03d679b108`.
The explicit source allowlist was applied before reading/hashing/copying any
source content, and every archive member and content hash was audited.
Hidden/runtime/session/cache files, compiled drivers and payload archives are
excluded. The existing remote-range evidence was not modified.

This proof is exact for these archived inputs. `src/fabric-remote.ts` was being
revised independently after bundling, so it does not establish qualification of
the later helper. `current-source-match.json` preserves that point-in-time
comparison; the 178 Rust/support inputs still matched. A later current-helper
run belongs in the parent directory after its owner's handoff and checks.
No VFS source correction was needed for this run.

The parent `build.py`, `controller.mjs` and `guest-canary.sh` contain the
reproducible driver flow. The private Fly 6PN fixture listener allowed only
this new machine's source IP and nonce and closed during cleanup.
