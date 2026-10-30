# RunnableRoot, paged directories and chunked-file build evidence

The `optimized-frozen` run compiled the exact 160-file source snapshot archived
here. `source-hashes.json`, `provenance.json` and `SHA256SUMS` bind its inputs,
binary and raw results. This is an unshipped development qualification, separate
from the alpha.7 product delivery.

The workload uses the pinned real Buck2 frontend and a fresh native gRPC REAPI
cache. Its prepared environment has Directory v2/File v1 content and an
EnvironmentRoot v2 identity. Each build receives a retained RunnableRoot with
SourceRoot v2, exact source-complement policy and prepared-input fingerprints.
The Rust archive is checked and the produced JavaScript actually runs under
the prepared Node runtime, returning 42.

| Frozen optimized case | Whole request | Buck2 counter | SDK materialization/verification |
|---|---:|---:|---:|
| Fresh graph, empty REAPI cache | 6,322 ms | 1,670 ms | 2,594 ms |
| Same graph, sample 1 | 3,261 ms | 115 ms | 1,416 ms |
| Same graph, sample 2 | 3,872 ms | 164 ms | 1,981 ms |
| Same graph, sample 3 | 3,642 ms | 164 ms | 1,648 ms |
| Fresh graph, REAPI hits | 5,180 ms | 417 ms | 1,624 ms |
| Rust source edit | 4,812 ms | 967 ms | 1,681 ms |

Prepared-index reuse took 15 ms. Exact input qualification took 4–5 ms and
read ten metadata objects; request totals also include RunnableRoot publication.
The cold case executed two compiler actions. Same-graph cases reused two DICE
actions. A new graph obtained two REAPI hits; editing Rust executed one action
and retained the TypeScript hit. Output identities and native action digests
were checked rather than inferred from command success.

“Fresh” describes the graph/action cache. This run reused an existing local CAS,
preparation index and SDK materialization. It did not flush the OS page cache,
download a toolchain or launch an agent. The separate earlier optimized run
measured 26.708 s for its first request and 3.198–3.620 s warm. The debug run
took 37–63 s per request under concurrent development load. All are retained;
do not combine them into a single cold-start or production latency claim.

Stage receipts expose repeated SDK verification, frontend hashing/capture,
Python driver/isolation probes and publication outside Buck2's own counter.
The next VFS input adapter must remove eager copies while preserving byte and
ownership validation. These samples do not qualify arbitrary Cargo/build
scripts, nonempty hardlink inputs, Rust executable linking, remote execution,
cross-engine cache hits or shared divergent-branch DICE state.

The source archive uses an explicit source-only allowlist. An ignored runtime
transcript was excluded during the pre-publication archive audit; it was not a
compiler input. Compiled source bytes, binary hashes and runtime results are
unchanged.
