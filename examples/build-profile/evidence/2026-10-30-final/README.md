# Final qualification, 2026-10-30

The final release-engine workload passed in 96.34 seconds, verified at 2026-10-30T11:58:45.790000Z.
This is a third fresh local CAS/index, graph and native REAPI cache run, after
adding first-use source-symlink rejection and checking the requested compiler
versions/platform before reusing a prepared-tool snapshot. It also verifies
that repeated attempts with an unsupported source symlink never reach the
frontend or create an action-index success.

| Case | Whole build (ms) | Frontend (ms) | Executed | REAPI hits | Graph reused |
|---|---:|---:|---:|---:|---:|
| cold-native-cache | 24222 | 1370 | 2 | 0 | 0 |
| warm-same-graph | 2460 | 114 | 0 | 0 | 2 |
| fresh-graph-reapi-hit | 2651 | 365 | 0 | 2 | 0 |
| source-edit-preparation-hit | 3277 | 718 | 1 | 1 | 0 |
| lockfile-invalidation | 4268 | 1580 | 2 | 0 | 0 |
| compiler-byte-invalidation | 16885 | 1324 | 2 | 0 | 0 |
| platform-contract-invalidation | 4329 | 1423 | 2 | 0 | 0 |
| build-policy-invalidation | 4138 | 1369 | 2 | 0 | 0 |
| coalesced-miss-0 | 3648 | 769 | 1 | 1 | 0 |
| coalesced-miss-1 | 4532 | 314 | 0 | 2 | 0 |

Cold environment import/publication took 17529 ms;
the warm prepared-index lookup took 2 ms.
The initial tool snapshot had already been acquired; verifying/reusing that
owned snapshot took 0.172 seconds before this run.
The [baseline](../2026-10-30-baseline/README.md) records its initial acquisition.

Whole-build timing includes construction/validation of a genuine partitioned
CheckpointV2, materialization/verification, the uncached namespace probe,
frontend execution/cache lookup, output import and durable result retention.
It excludes the separately reported cold preparation and caller cleanup.
Frontend timing covers the Buck2 build command only. No OS page-cache flush
was performed. The host was shared with other engineering work; these are
single observations, not a controlled statistical benchmark.

The workload verifies full output roots and real Rust archive/TypeScript
execution, fresh-graph native cache hits with matching action digests, source
edits reusing preparation, lockfile/compiler-byte/platform-contract/policy
invalidation, concurrent miss coalescing, dirty prepared input rejection,
consistent source-symlink rejection, inaccessible host files/network and all
seven declared effect classes. No tool/source validation was disabled for the
warm path. A compiler-change case materializes another complete tool closure.

[summary.json](summary.json) and `frontend/<case>/` contain the raw engine run
records, frontend reports and compressed event logs. `prepared_output` is the
historical summary-field name for the compiled artifact output directory;
preparation identity is separately retained in each run's checkpoint.
Source checksums describe the checkout at archival; the executable checksum
identifies the exact test binary. The runner/driver checksums match the scripts
bundled in this run. The cache log is the actual localhost bazel-remote service.
No fixture process is left running after success.

This qualifies Linux x86_64, glibc 2.36 and kernel `6.12.114-fly`, pinned Buck2
2026-10-15 and bazel-remote 2.6.4. The workload uses compile-only fixed rules,
not Cargo build scripts/npm application scripts or a full Rudder repo build.
It does not claim remote execution, Buildbarn qualification, cross-Bazel/Buck2
action hits, a shared divergent warm DICE graph or zero-copy VFS operation.
