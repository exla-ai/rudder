# Retained-publication qualification, 2026-10-30

This release-engine run passed the complete workload in 108.42 seconds,
verified at 2026-10-30T11:53:16.033Z. It used a fresh CAS, environment index,
Buck2 graph and localhost bazel-remote cache. Tool versions and platform match
the [baseline](../2026-10-30-baseline/README.md). OS page caches were not flushed.

| Case | Whole build (ms) | Frontend (ms) | Executed | REAPI hits | Graph reused |
|---|---:|---:|---:|---:|---:|
| Cold graph/cache | 17725 | 1523 | 2 | 0 | 0 |
| Warm same graph | 2449 | 114 | 0 | 0 | 2 |
| Fresh graph, populated REAPI | 2771 | 365 | 0 | 2 | 0 |
| Rust source edit | 3386 | 716 | 1 | 1 | 0 |
| Lockfile change | 5027 | 2024 | 2 | 0 | 0 |
| Actual compiler-byte change | 23500 | 2275 | 2 | 0 | 0 |
| Platform-contract change | 5288 | 1878 | 2 | 0 | 0 |
| Build-policy change | 7082 | 1868 | 2 | 0 | 0 |

The cold environment import/publication took 25735 ms; a warm prepared-index
lookup took 2 ms. First materialization/validation of the 736 MiB tool closure
took 13835 ms, while the ordinary warm environment validation took 1024 ms.
These costs are included in whole-build timings and excluded from the frontend
column. Cold compiler replacement needs another full tool materialization.

The new explicit retained-root publication path avoids rereading and syncing
old compiler blobs when a result extends a currently pinned preparation/action
receipt. It still validates new objects and current retention authority under
the GC lock. Warm/fresh-graph times improved over the baseline in these samples;
other cases varied, including a slower compiler-change case. This is a single
run on a shared host, not a statistically controlled performance benchmark.

All invalidation, concurrent-miss, dirty-prepared, denied-host-read and effect
policy checks passed. Concurrent fresh graphs executed just one new compile
between them. The emitted TypeScript JavaScript returned 42 and the Rust result
was a real archive.

[summary.json](summary.json) and `frontend/<case>/` contain the unedited engine
observations, native REAPI action digests, event logs and frontend reports.
The immutable action identities include the actual compiler bytes and requested
versions; the Rust version-label fallback used in the baseline was corrected.
Source hashes describe the checkout at archival; the executable hash identifies
the exact test binary. A later final qualification adds consistent first-use
symlink rejection and stronger prepared-tool marker reuse checks; its evidence
is separate. No remote-execution, Buildbarn, cross-engine action-hit, global
warm-DICE or zero-copy-VFS claim is made.
