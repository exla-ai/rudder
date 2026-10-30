# Bounded remote partial capture

The explicit host API collects new immutable objects and same-session opaque
remote boundaries, retains the reachable partial graph, then commits an owning
view's nonce/head. It keeps the mutable base and dirty maps unchanged.
`PartialCaptureReceipt` version 1 is local data retention and `LiveBestEffort`;
it is neither a complete pin, writer barrier, accepted source, RunnableRoot nor
replicated publication receipt.

`VfsView::capture_partial_source[_cancellable]` takes a canonical nonce, exact
begin `DirtyCursor`, expected previous capture head, exclusion policy and work/
delta budgets. The result binds view/owner, exact remote root and attempt,
begin/end cursor, policy digest, nonce, historical head sequence and
`PartialTreeReceipt`. `inspect_partial_capture`, `partial_capture_head` and
`release_partial_capture` reconcile results and generation-exact retirement.
The current head is protected from release.

The owning SQLite journal reserves the request and staging lease, FULL-commits
the immutable candidate before object-set retention, then commits receipt and
compare-and-set head after the object-set ACK. A late cancellation/fence can
withhold the reply while preserving those records and retained bytes. Retry
replays the original candidate; it never recaptures later content under that
nonce. If an unacknowledged candidate loses its lease/bytes, it becomes
unavailable and requires a new nonce. There is no cross-database atomicity claim.

Activation permanently upgrades a valid remote namespace from 3 to 4 in the
same FULL transaction as journal creation. The descriptor then exposes
`partial_capture_format:1`. Alpha9 namespace3 clients refuse version4 on reopen;
the existing exclusive owner lock still governs live ownership. Local namespace
versions are unchanged. Missing/unknown/corrupt journals and disappeared heads
fail closed, with no silent recreation. The complete observation RPC, complete
observation and checkpoint APIs stay explicitly `EOPNOTSUPP` for remote views.

Capture records are capped at 128 total / 16 live, 4 MiB per candidate and
64 MiB of encoded journal fields. Producer defaults cap 16,000 new references,
4,096 remote boundaries, 100,000 changed entries, 256 MiB content work, 128 MiB
producer/replay metadata work and 30 seconds. Delta validation has a separate
64 MiB metadata budget. Proof replay shares bounded verified prefixes and never
loads a Blob ancestor. Producers record references at staged puts; neither
capture nor retention inventories the whole CAS or hydrates unchanged file contents.
Nonempty runtime hardlink topology remains an explicit unsupported capture gate.

Mounted proof passed root and UID 1500 on owned disposable Fly Machine
`857723db2965d8`, instance `01M67BPFD1NR6RDENF1NGTQBYD`, in `iad`.
The machine was deleted and absence verified at `2026-10-30T21:36:58.710Z`,
then independently confirmed at `2026-10-30T21:38:57.181099Z`.
It had two shared CPUs, 2 GiB RAM, no services, no volumes, no app secrets and no
production environment. No active home was accessed or restarted.

The actual worker image was `registry.fly.io/rudder-workers-exla:4.0.0-alpha.9`,
asserted digest
`939c5593c5ed1232577d575f7ff382448c466a9033874c94a1a684c4ec5d599a`,
revision `11f1f75e5a18555f6c7e257aacc6b4097f425c5e`. Its init was overridden with
sleep, so this capture run did not repeat the separate real-bootstrap proof.
Runtime was Linux `6.12.114-fly` and Node `22.23.3`. Device ownership/mode was
verified as `1500:1500:0600`; data fixtures lived under `/workspace` and were
removed. Each case used fresh private namespaces; the outer PID1 mount namespace
remained unchanged. No setuid helper was installed.

| Mounted measurement | Root | UID 1500 |
| --- | ---: | ---: |
| Initial listing entries | 100,101 | 100,101 |
| Initial listing time | 25,156 ms | 24,935 ms |
| Listing File/Blob fetches | 0 | 0 |
| Capture through data ACK, before intentionally cancelled head reply | 711 ms | 724 ms |
| Reachable new objects | 19 | 19 |
| Authorized remote boundaries | 48 | 48 |
| New Blob bytes for two dirty 4 MiB chunks | 8,388,608 | 8,388,608 |
| Private log bytes before / after later FD write | 131,072 / 196,608 | 131,072 / 196,608 |
| Original large-file source payload requests / bytes | 2 / 8,388,608 | 2 / 8,388,608 |
| Complete LocalStore pins | 0 | 0 |

The producer-object count excludes the canonical plan Blob, which joins the
same retained object set.

The real mounted file retained both a 7-byte write and actual mmap/msync/fsync
bytes. Cancellation after the object-set ACK left no capture head. With the
same actual FD still open, a later write advanced generation 11 to 12. After
GC, the original nonce retained `changed` and `MMAP`, while a new head retained
`later!!`. Releasing the old generation and collecting again preserved the new
head; releasing the current head failed. Base/latest stayed unchanged, complete
RPC/observation/checkpoint returned errno95, and no new complete pin appeared.
The busy strict-unmount/retry, remount, `ESTALE`, strict discard, sibling
isolation, runtime-link and open-unlink matrix also passed. A saved data receipt
remained readable after revocation without restoring live view authority.

This fixture uses generated public data, repeated references and the persistent
Node helper over authenticated loopback HTTP. Its 208 MiB logical file does not
represent 208 MiB of distinct chunks. These are individual samples, not S3,
cross-worker latency, fleet consensus, quiescence, default-agent or macOS proof.
Publication/accepted-source integration and the pre-mount factory remain
separate consumers; this work does not enable the complete observation format.

Frozen release checks passed 27 cases: 11 partial capture, 14 existing complete
RPC, one repeated-child validator and one directory alias regression. The
capture cases include four real process-death phases, GC, lost replies before
and after head commit, revocation after retention, cursor/policy/head mismatch,
producer/content/metadata/boundary quota exhaustion, safe failed-candidate
retirement, stale-generation release, exclusions and moved immutable origins.
The one-dirty-chunk 100,101-entry / 208 MiB release sample took 507 ms locally,
retaining nine new objects and 33 boundaries with one payload fetch and no pin.

The first debug run exceeded the unchanged 30-second deadline because duplicate
parent-child checks and repeated parent/child decoding did redundant work.
The fixes deduplicate per parent while still checking every matching logical
length. The malformed final alias and branch alias tests still reject bad data.
`initial-default-deadline.log` is preserved byte-for-byte, SHA-256
`855f39a56213002ea4c5a9ab8d3326cc869b1c4d4b346d7d572d689da9d3a9ae`.
That batch also passed all five existing observation and nine range tests, and
28 of 29 remote tests. A subsequent debug capture run passed 11/11; final frozen
release logs include all optimizations. Successful test/compile logs are gzip
encoded without changing their decompressed bytes; `raw-log-sha256.json` records
their original hashes. Build/fixture/profile differences mean
these timings are not a controlled before/after benchmark.

| Provenance | SHA-256 |
| --- | --- |
| Rust/support build envelope embedded in driver | `36bf0f6eb38f8cb2cf7da083234f6011fa2ef679cd5706efebecd33a5141a749` |
| Exact compiled driver | `0998d201b7615d7f79604a9bc15dd60e95089b3942389ff736ffe0a16fcc0519` |
| Combined 189-file source envelope | `9d90829531cf4214768ddc3ae18a8fe358871b139136b7d4c354b3687cf2b9de` |
| Exact source-only archive | `0ce908b36f362cbda54448c6e628f2ccc0c3faac10a4bbf80b3194548823ba53` |

The archive contains 185 allowlisted Rust/support inputs plus four separately
hashed helper inputs. The name policy is applied before reading/hashing/copying
source, and every regular archive member/content hash was audited. Hidden,
runtime, session and cache directories, compiled drivers and executable payload
archives are excluded. No original session transcript was read. The old range
and paged-readdir evidence was not changed.

`capture-source-match.json` confirms all 34 selected capture/retention/transport
implementation and test files still match the frozen input. The separate full
checkout comparison records concurrent execution/native/VCS changes and does
not claim those changes were qualified. `build.py`, `controller.mjs`,
`guest-canary.sh` and `package.py` preserve the flow. Build with a separate
`RUDDER_QUALIFICATION_TARGET_DIR`, use only a fresh nonce-owned disposable VM,
and assert the image digest before executing the payload. `SHA256SUMS` covers
the final evidence. This implementation remains unshipped.
