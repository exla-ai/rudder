# Remote paged-readdir mounted qualification

Verified root and UID 1500 on owned disposable Fly.io Machine
`2862596f5d5208`, instance `01M6775RMZ4HDNAQ8C3TM39MBE`, in `iad`.
The machine was deleted and absence verified at
`2026-10-30T20:16:06.690Z`; a second inventory confirmed absence at
`2026-10-30T20:16:11.086571Z`. It had two shared CPUs, 2 GiB RAM, no services,
no volumes and no production environment or credentials. No deployment occurred.

Runtime: Linux `6.12.114-fly`, Node `22.23.3`, `node:22-bookworm-slim`,
resolved image digest
`sha256:efd0ab5780c2d9ab1f0f869571a00d5edb17793bff4cce4a2792e3eb0ffc7562`.

| Measured first listing | Root | UID 1500 |
| --- | ---: | ---: |
| Entries | 100,101 | 100,101 |
| Listing time | 25,579 ms | 24,564 ms |
| getdents64 calls, including EOF | 985 | 985 |
| Directory metadata requests | 1,114 | 1,114 |
| File descriptor or Blob fetches during listing | 0 | 0 |
| Cursor SQLite bytes after listing | 5,464,064 | 5,464,064 |
| Namespace reservation commits | 982 | 982 |
| Cursor commits | 985 | 985 |
| Root construction and private fork | 70 ms | 115 ms |
| Large-file payload bytes in the later range matrix | 8,388,608 | 8,388,608 |
| Final private CoW bytes | 131,072 | 131,072 |
| Complete LocalStore pins | 0 | 0 |

The driver calls real `getdents64` with two initial 512-byte buffers followed
by 32 KiB buffers. It checks byte ordering and complete records, seeks with
`lseek` to a returned mid-page cookie, then renames/unlinks/creates while the
same directory handle stays open. Rewind retains the old snapshot; close and
reopen observe the new namespace. Closing handles releases all cursor, cookie
and private override quotas. The stage counters in `fly-mounted.json` describe
the first open scan; the later cleanup is a separate successful assertion.
The scan reserves identities without materializing child attributes.

Both runs also passed the existing range matrix: mmap/msync/fsync with actual
content checks, 64 KiB private CoW blocks, sibling isolation, inherited directory
rename, runtime hardlinks, an unlinked open FD, strict `EBUSY` unmount and retry,
remount, `ESTALE` on a new lookup after invalidation and strict discard.
Checkpoint and source observation returned `EOPNOTSUPP`. Discard produced no
checkpoint proof, the local CAS acquired no complete root pins and the 208 MiB
logical file fetched only its two requested 4 MiB chunks. Each run made two
fresh root-receipt requests and 1,126 source-object requests in total.

The actual data path used the persistent Node helper, loopback authenticated
HTTP fixture, protected remote-root session, partial object cache, private
SQLite namespace and Linux FUSE adapter. The fixture contains generated public
data and repeated references. These are individual release-build samples, not
production S3 or inter-worker latency measurements or arbitrary SDK compression
claims. Nonempty inherited hardlink topology, remote checkpoint/observation,
VCS integration, build-cache reuse and a default launch switch remain outside
this proof. The opt-in pager does not enable native macOS mounts.

UID 1500 entered a fresh executable before creating private user/mount
namespaces. It could not inspect PID 1's mount namespace; the outer root script
verified that namespace remained unchanged and both owned fixture directories
were removed. Device group 1500/mode `0660` existed only inside this disposable
VM. No setuid helper was installed. The fixture listener bound only to private
Fly 6PN, admitted only this new machine's source IP and nonce, verified the
compressed payload hash before execution, and closed during cleanup.

The final helper was frozen after its owner's timeout/admission fix and checks.
`current-source-match.json` confirms all 182 qualified source files matched the
checkout after the mounted run. The Rust/support envelope remained unchanged,
so this run reused the exact previously qualified release driver after checking
both its SHA-256 and embedded source-manifest hash. It did not wait on the
unrelated full workspace Cargo test lock. `build.log` records that reuse.

| Provenance | SHA-256 |
| --- | --- |
| 178-file Rust/support envelope embedded in driver | `c9e0e30ed75f8c325c4b34f8f0ddab8bdab41643197e150e66c69ecff0b6f16f` |
| Exact compiled driver | `5d8256b3ffa8aae7719cc88eb4ea2bab9f6dbd811c968c144f139811394d27af` |
| Four-input final helper manifest | `f6e80f739f999281d4886348cf07bda31bab31d1aa8c85227efb1b3e62626757` |
| Combined 182-file qualified source envelope | `8fbf969b91e1150e839999aac68d2a354d16af8fb1f6def14e9bbf6671baed06` |
| Exact source-only archive | `eb6e74f27baa2a3631ec0918b76fa5ae80c312f6bbc32720a61deb0791489c00` |

`source-sha256.json` is the original Rust/support build envelope;
`helper-input-sha256.json` names the four helper hashes, and
`qualified-source-sha256.json` combines the exact 182 qualified file hashes.
`source.tar.gz` contains those regular source/support files. The explicit
allowlist is applied before reading, hashing or copying source content; member
names/types are checked before reading archived file contents, and every
content hash matches. No hidden/runtime/session/cache files, compiled drivers
or executable payload archives are published. The source archive supports the
qualified engine target; it is not a complete fixture bundle for all native
dashboard tests/platforms. No original session transcript was read.

`previous-helper/` preserves the earlier successful root/UID 1500 run with its
own exact source archive and cleanup proof. That helper preceded the parallel
timeout fix. Its Rust/support envelope and driver are identical. No VFS Rust
correction was needed for either run. The preexisting
`2026-10-30-remote-ranges` evidence remains unchanged.

To reproduce, run `build.py REPOSITORY PRIVATE_CACHE` from this directory's
scripts, then set `RUDDER_QUALIFICATION_STATE` to the generated `current.json`
and run `controller.mjs`. `FLYCTL` may point to the local Fly CLI; the default
qualification app is `rudder-workers-exla` and must contain no app secrets.
Use only a fresh owned disposable canary, never a production machine. After a
successful cleanup, `package.py RUN_DIRECTORY` audits and packages the proof.
`SHA256SUMS` covers this evidence tree, including the previous-helper proof.
