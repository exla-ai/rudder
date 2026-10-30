# Baseline qualification, 2026-10-30

This is one release-engine run of the opt-in representative workload on Linux
x86_64, glibc 2.36, kernel `6.12.114-fly`. It is not a full Rudder repository
build and does not measure agent launch or VFS copy-on-write. The frontend
and native REAPI cache started empty; OS page caches were not flushed.

The whole workload passed in 138.22 seconds. The prepared tool snapshot
contains approximately 736 MiB. Snapshot acquisition took 9.210 seconds;
importing and publishing the cold environment took another 33.494 seconds.
The warm prepared-index lookup took 7 ms. Preparation is reported separately
from each build below.

| Case | Whole build (ms) | Frontend (ms) | Executed | REAPI hits | Graph reused |
|---|---:|---:|---:|---:|---:|
| Cold graph/cache | 22473 | 1583 | 2 | 0 | 0 |
| Warm same graph | 4563 | 114 | 0 | 0 | 2 |
| Fresh graph, populated REAPI | 9296 | 473 | 0 | 2 | 0 |
| Rust source edit | 6665 | 665 | 1 | 1 | 0 |
| Lockfile change | 7516 | 1575 | 2 | 0 | 0 |
| Actual compiler-byte change | 20046 | 1469 | 2 | 0 | 0 |
| Platform-contract change | 7520 | 1470 | 2 | 0 | 0 |
| Build-policy change | 7005 | 1523 | 2 | 0 | 0 |

Two concurrent fresh graphs produced one Rust compile in total; their other
three actions hit the native cache. Dirty prepared `node_modules` was rejected
before starting the frontend, undeclared `/etc/passwd` access failed, and all
seven declared effect classes were rejected. The Rust output is a real archive;
the emitted TypeScript JavaScript executed and returned 42.

These baseline numbers exposed a bottleneck: ordinary publication reread and
synced the retained compiler closure for each build receipt. They predate the
explicit retained-root `publish_extending` optimization. The frontend column
must not be presented as whole-build latency. The fresh-graph whole-build
measurement also includes starting a separate Buck2 daemon and its first local
state, rather than preserving a prior graph under a new name.

[summary.json](summary.json) contains all observations, tool identity, action
digests and scope limits. `frontend/<case>/` contains unedited frontend event
logs, reports and engine run records. `reapi-cache.log` is the actual localhost
native REAPI cache log. The source checksums were archived before the retention
optimization. Generated runner/driver checksums match the ones in that build.
The executable compiler bytes and reported actual compiler version are pinned;
the baseline Rust toolchain label used the conservative `prepared-runtime-v2`
fallback, corrected to the actual `rustc` version in subsequent runs.

The service was checksum-pinned bazel-remote 2.6.4 over native gRPC REAPI v2.
Remote execution, Buildbarn qualification, cross-Bazel/Buck2 action hits and
shared divergent warm DICE graphs are not claimed. Absolute paths in the raw
records name disposable owned fixture directories, not required installation
locations. No service is left running by the test.
