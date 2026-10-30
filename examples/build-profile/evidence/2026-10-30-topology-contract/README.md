# Topology-aware action contract qualification

This reruns the release-engine real Rust/TypeScript Buck2/REAPI workload after
checkpoint v3 topology entered both Rudder and native hidden action inputs.
The Rust unit suite separately validates schema3 and rejects forged link groups.
The actual compilation matrix uses schema2 (`input_topology: null`).

Concurrent development/builds were active; compare the archived samples rather
than treating one timing as a platform-wide latency guarantee.

| Case | Whole build (ms) | Frontend (ms) | Executed | Remote hits |
|---|---:|---:|---:|---:|
| cold-native-cache | 17651 | 1769 | 2 | 0 |
| warm-same-graph | 3560 | 165 | 0 | 0 |
| fresh-graph-reapi-hit | 3216 | 467 | 0 | 2 |
| source-edit-preparation-hit | 5287 | 1621 | 1 | 1 |
| lockfile-invalidation | 4303 | 1529 | 2 | 0 |
| compiler-byte-invalidation | 21665 | 1469 | 2 | 0 |
| platform-contract-invalidation | 4277 | 1327 | 2 | 0 |
| build-policy-invalidation | 4159 | 1471 | 2 | 0 |
| coalesced-miss-0 | 4872 | 366 | 0 | 2 |
| coalesced-miss-1 | 3899 | 717 | 1 | 1 |

Verified Rust archive and TypeScript result (`42`). Source edits reuse the
prepared environment; compiler, installer, platform and policy changes miss.
No large CAS/tool/binary fixtures are included. Tool identities, declared inputs,
output roots and native action digests remain in the run receipts and reports.
The engine test-binary manifest records the executable used for this run. This
working-tree qualification predates the final release source revision.
