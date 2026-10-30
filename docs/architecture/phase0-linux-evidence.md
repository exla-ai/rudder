# Phase 0 Linux workspace evidence

Measured 2026-10-30. This is a bounded, single-host E0/E1 mechanism experiment,
not completion of [E0 or E1](experiments.md#e1-cheap-forks-and-isolated-writes),
a deployed workspace engine, or a distributed prepared-environment result.

A private user/mount namespace on this guest can mount writable OverlayFS views
of an existing prepared tree when upper/work directories use the `/workspace`
ext4 filesystem. Creating ten small views took a median 3.11-3.65 ms per batch
in the tested warm cases. Every initial upper contained zero regular files.
Actual reads, private writes and synthetic rebuilds succeeded. Direct native
reflinking failed on both tested filesystems. These results support continuing
an ext4-backed OverlayFS materializer experiment; they do not select the full
Rudder VFS or demonstrate faster real compilation.

## Provenance and scope

[Exact runner, raw samples and provenance archive](evidence/phase0-linux-2026-10-30.tar.gz).
The archive contains `benchmark.py`, `raw-child.json`, `provenance.json`,
`checks-summary.json`, `repository-inventory.json` and text summaries. The
reproducible synthetic runner uses Python's standard library, existing Node,
`unshare` and the host kernel; no packages were installed for this experiment.

| Property | Observation |
|---|---|
| Measurement window | 2026-10-30T09:57:24.712089+00:00 to 2026-10-30T09:57:34.522231+00:00 |
| Repository revision | `f1403d43b739a4ef61caa7c76ce7cee6fcab0eec` at start and end; working tree clean before the experiment |
| Package version | `4.0.0-alpha.3` at start and end |
| OS/kernel/architecture | Linux `6.12.114-fly`, `x86_64`; reported hostname `8edd225a7d0378` |
| CPU | 4 reported logical CPUs; Intel(R) Xeon(R) Processor @ 2.30GHz |
| Provider placement | Fly guest kernel; region not exposed through `FLY_REGION`; storage/network placement not independently qualified |
| Identity/capabilities | Host uid/gid 1500; effective capabilities `0`; `NoNewPrivs=0`, `Seccomp=0` |
| Filesystems | `/workspace`: `ext4` (`stat -f` calls this family `ext2/ext3`); `/tmp`: `overlayfs` |
| Tools | Node `v22.23.4`; Python 3.11.2; `git version 2.39.5`; `jj 0.40.0-941d0290f5511db1d1cf8e4fe7365ba7d1828ea0` |
| Runner identity | SHA-256 `c8083f1188568f660ec996c6c29e9b03eefd2d6f51a6eddeec2cf480b4b86b3b` |
| Synthetic build script | SHA-256 `5f2f3c610bd8368a8915622a6e8cb1f1365e42c12bb388a8f862541e9db3e417` |
| Load average | Start [0.2998046875, 0.45263671875, 0.93310546875]; end [0.40771484375, 0.47119140625, 0.93408203125] |

All trials used warm local filesystem/page caches after fixture creation and
one successful preparation build. Each read/build command started a new Node
process. No cache was evicted, no dedicated cold worker was provisioned, and no
production process or shared service was restarted. This was a live guest,
not a dedicated performance machine. CPU quota and memory limits were not
available through the sampled cgroup paths.

## Capability probes

`unshare --user --map-root-user --mount --propagation private` succeeded. The
runner refuses to call `mount(2)` unless both namespace IDs differ from the
parent and `findmnt -n -o PROPAGATION /` reports `private`. Host namespace IDs
remained `user:[4026531837]` and `mnt:[4026531841]`.
The benchmark ran as namespace root mapped to the calling host user. This is
namespace-local privilege, not host `CAP_SYS_ADMIN`.

| Probe | Exact result |
|---|---|
| Kernel filesystem list | OverlayFS, ext4, erofs and FUSE present; `/dev/fuse` exists; `/dev/kvm` absent |
| `FICLONE`, `/tmp`, 4,096-byte file | ioctl `0x40049409`: errno 95, `ENOTSUP`, `Operation not supported` |
| `FICLONE`, `/workspace`, 7,168-byte file | Same errno 95 / `ENOTSUP`; no copy fallback was counted as a clone |
| OverlayFS with upper/work under `/tmp` | `mount` exit 32: `wrong fs type, bad option, bad superblock on overlay, missing codepage or helper program, or other error`; no mount established |
| Same private OverlayFS layout under `/workspace` | Success; writing through the view preserved the parent file |

The `/tmp` failure does not imply OverlayFS is unavailable on this guest.
Conversely, the ext4 success does not qualify every Fly worker image, mount
layout or security profile. Repeat the capability probe on the chosen production
worker profile. `cp --reflink=auto` would require checking for silent byte-copy
fallback; native extent reflinking was not available here.

## Fixture and measurement method

Three prepared lower trees were tested sequentially. For input `i`, payload is
the repeated ASCII hex SHA-256 of decimal `i`, truncated to the configured file
size. Paths are `inputs/g{i//250:03d}/f{i:05d}.dat`. The optional blob is
8 MiB of the repeated sequence of bytes 0-255. A small `fixture.json`,
`build.mjs`, and prepared `out/bundle.json` complete each tree. The runner in
the archive fixes all generation details and expected digests.

| Fixture | Input files | Bytes per input | Extra blob bytes | Lower regular files / directories | Lower logical bytes | Lower allocated bytes* |
|---|---:|---:|---:|---:|---:|---:|
| 100-files | 100 | 4,096 | 0 | 103 / 4 | 410,884 | 438,272 |
| 10000-files | 10,000 | 41 | 0 | 10,003 / 43 | 411,286 | 41,476,096 |
| 100-files-plus-8MiB | 100 | 4,096 | 8,388,608 | 104 / 4 | 8,799,499 | 8,826,880 |

*Allocated bytes are summed `st_blocks * 512`, including visible directory
blocks. This is filesystem accounting, not a device-I/O trace. It excludes
inode tables, journal/device overhead and filesystem-cache RAM. Logical payload
size is deliberately almost equal for 100 versus 10,000 input files; file-count
allocation overhead is substantial on this ext4 volume.

For each tree, five sequential batches of one view and five batches of ten
views were created: 30 batches, 165 views total, never more than ten live views.
Cases and view counts were not randomized. All Node commands ran serially;
ten views do not mean ten agents or ten concurrent builds.

Each view has private upper/work directories and a merged mount, with
`userxattr,lowerdir=...,upperdir=...,workdir=...` and `MS_NOSUID|MS_NODEV`.
Creation timing includes four directory creations per view and the direct libc
`mount(2)` call. The batch container directory and user/mount namespace already
exist. Namespace startup, fixture preparation, teardown, jj, agent CLI/model
startup, network, distribution and remote hydration are excluded.

After creation, the first real command is `node build.mjs --read`: it reads the
first inherited input and prepared output through the view. The synthetic
build runs `node build.mjs`, reads every input and the optional blob, computes
SHA-256, writes a temporary output and renames it over `out/bundle.json`.
This is an ordinary Node filesystem read/hash/write command, not TypeScript,
Rust, Buck2 or Bazel compilation and not an action-cache hit.

Every view then changes the first byte of its own first input to a distinct
value and creates its own `private-note.txt`. The first view rebuilds, and its
new digest must differ from the prepared digest. The test verifies the parent
input/output remain unchanged and that a sibling's inherited output remains
unchanged. On trial one for each fixture/view-count pair, the first view is
unmounted and remounted with the same upper/work directories and checked again.

## Timings

Milliseconds; median [minimum, maximum], five samples per row. Creation is the
whole batch, while read/build/rebuild columns time the first view in that batch.
The samples are too few for tail-percentile or fleet-throughput claims.

| Fixture | Views | Create batch | First Node read | First synthetic build | Build after private edit |
|---|---:|---:|---:|---:|---:|
| 100-files | 1 | 0.357 [0.348, 0.491] | 47.668 [45.958, 50.368] | 57.028 [50.727, 66.095] | 56.549 [49.400, 66.149] |
| 100-files | 10 | 3.645 [2.954, 4.150] | 47.201 [45.025, 50.454] | 54.838 [52.783, 60.991] | 51.750 [44.727, 62.805] |
| 10000-files | 1 | 0.364 [0.328, 0.415] | 50.950 [49.425, 53.348] | 207.751 [198.701, 217.147] | 167.030 [156.116, 178.849] |
| 10000-files | 10 | 3.129 [2.746, 3.650] | 45.481 [43.164, 51.017] | 192.300 [181.854, 202.471] | 149.194 [142.775, 167.668] |
| 100-files-plus-8MiB | 1 | 0.345 [0.309, 0.412] | 47.416 [41.996, 60.086] | 70.263 [66.285, 83.505] | 73.908 [66.707, 80.236] |
| 100-files-plus-8MiB | 10 | 3.113 [2.610, 3.745] | 50.733 [45.667, 54.916] | 67.050 [66.233, 75.717] | 66.033 [64.637, 75.114] |

Creation times show no entry-count or inherited-byte penalty within these three
small warm fixtures; that is an observation, not an O(1) proof. Ten-view batch
creation scales with the ten mounts/directories. The first synthetic build is
materially different: visiting 10,000 files takes longer than visiting 100 files
with similar logical payload bytes. Fast mount acceptance does not eliminate
first-use metadata work, tool startup or build work.

The runner took 9.766 seconds wall time including setup
and teardown. Its 93 Node commands (3 preparation, 30 reads, 30 builds,
30 rebuilds) and `findmnt` child used 5.889 user
and 3.001 system CPU-seconds. Maximum child
RSS was 64,828 KiB; this is command-process memory,
not a per-view RAM measurement and does not include all kernel/page-cache memory.

## Isolation, allocation and cleanup

All 30 batches passed the scoped checks; all 165 initial uppers contained zero
regular files and zero logical file bytes. An unchanged synthetic build created
one 110-112-byte private output, with 12,288 accounted upper bytes including
its directories. After the edits, the first view had three private files
(input, note, output), and each other view had two (input and note). Across ten
views, upper/work accounted allocation was 294,912 bytes for each fixture.
The maximum measured lower plus upper/work accounted allocation was
41,771,008 bytes
(39.84 MiB), below the
64 MiB fixture budget; this is not total physical device usage. The largest
stable checkpoint contained 10,024 regular fixture files; atomic output replacement and kernel copy-up can add transient files. Fixtures were
deleted between cases.

Only writes through the merged paths were isolated. The runner could still
open the lower path directly as the same underlying user; the lower was
logically held fixed, not sealed by an adversarial access-control boundary.
Production must pin/seal inherited roots and protect shared writable metadata.
This experiment establishes selected-path copy-up behavior, not a sandbox.

OverlayFS copied up the changed file; it did not provide native extent cloning.
The changed inherited input was only 41 or 4,096 bytes. Copy-up of a changed
large file, dependency/package-manager metadata, Git/jj state, mmap writes,
hardlinks, notifications and lock semantics was not qualified. A remount is
not a crash, cache eviction, fsync durability or a replicated checkpoint.

All six remount checks passed. Raw remount times (ms):
0.358885, 0.310999, 0.351265, 0.330301, 0.297487, 0.374705.
All mounts occurred in the checked private child namespace. That child exited
successfully; the parent namespace IDs were unchanged and, after child exit,
the parent saw no fixture mount. Every disposable `/workspace/.rudder-phase0-*`
data/probe tree created by this experiment was removed. The small `/tmp/rudder-phase0-linux-evidence`
folder is retained only until its linked archive is captured by the release
owner. No product source or runtime configuration was changed.

## Current checkout size inventory, not an E0 performance baseline

Measured 2026-10-30T09:58:43.554007+00:00 at the same revision. Metadata-only inventory of
tracked files and explicitly named build/dependency directories; no other user
directories or credentials were scanned. Directory values are allocated bytes
from `du -sx --block-size=1`; they are not directly comparable to logical source
bytes or a clean-workspace footprint.

| Scope | Bytes | Meaning |
|---|---:|---|
| Tracked regular files (412) | 16,865,376 | Logical sizes; zero missing tracked files |
| `node_modules` | 42,676,224 | Existing allocated directory size |
| `target` | 1,749,291,008 | Existing allocated directory size |
| `dist` | 7,991,296 | Existing allocated directory size |
| `cloud/node_modules` | 112,713,728 | Existing allocated directory size |
| `cloud/dist` | 540,672 | Existing allocated directory size |

This reproduces the general small-source/large-build-tree observation, but
includes existing build caches and multiple build profiles. It is not a matched
comparison of Rudder launch, dependency install or real compilation. The prior
53-test release-terminal run validates behavior using fake model CLIs; its
suite duration is not an E0 workspace/agent latency measurement.

## What this permits and what remains

Continue with a Linux local materializer whose shared lower and private upper
live on a qualified ext4-backed worker volume, inside the machine engine's
owned namespace. Do not rely on native reflink support or an overlay-backed
scratch upper without probing. The result demonstrates that cheap local
private view construction is feasible on this guest while useful work remains
a distinct measured cost.

E0 still needs matched current-Rudder workspace/CLI/dependency/build baselines,
representative repositories, controlled cold states and native Mac results.
E1 still needs larger prepared environments and filesystem/tool semantics,
immutable-root enforcement, metadata/RAM accounting and acceptable behavior
under concurrency. E2 needs two independent workers, content addressing,
lazy transfer and complete first-build accounting. No distributed CAS,
compilation-cache correctness, crash durability or fleet result is established.

## Raw timing samples

All timings in milliseconds, kept at the runner's recorded precision. These
rows are also present with allocation/isolation fields in `raw-child.json` in
the linked archive.

```csv
fixture,views,trial,create_ms,first_read_ms,first_build_ms,edited_build_ms
100-files,1,1,0.490843,45.957600,50.726627,62.066898
100-files,1,2,0.350552,48.713346,66.094837,52.674171
100-files,1,3,0.407934,46.576354,57.028219,56.549013
100-files,1,4,0.347754,47.667610,59.271909,66.148663
100-files,1,5,0.356656,50.367734,56.834132,49.399962
100-files,10,1,3.785880,49.810581,60.990734,44.727497
100-files,10,2,4.150197,46.717849,52.991286,62.804718
100-files,10,3,3.645340,45.024923,55.192786,51.750458
100-files,10,4,3.490777,50.454282,52.782513,52.407404
100-files,10,5,2.953889,47.200533,54.838365,50.404627
10000-files,1,1,0.415117,49.424672,217.147418,167.029967
10000-files,1,2,0.339000,53.348134,198.700981,178.848846
10000-files,1,3,0.381771,53.072595,207.750571,175.494440
10000-files,1,4,0.364066,50.949541,208.648286,156.116385
10000-files,1,5,0.327804,50.852510,206.170922,157.044375
10000-files,10,1,2.772246,43.163700,202.470557,149.193906
10000-files,10,2,2.746386,44.908963,199.282246,149.155486
10000-files,10,3,3.128768,45.481055,192.300234,167.668369
10000-files,10,4,3.209791,51.017060,183.110908,162.388458
10000-files,10,5,3.649693,48.319296,181.854118,142.774506
100-files-plus-8MiB,1,1,0.345464,48.259870,74.955589,75.908232
100-files-plus-8MiB,1,2,0.308514,47.211943,66.483260,68.605236
100-files-plus-8MiB,1,3,0.322096,47.415944,66.284730,80.236310
100-files-plus-8MiB,1,4,0.412077,60.086121,83.505482,73.907809
100-files-plus-8MiB,1,5,0.401916,41.996271,70.263087,66.706652
100-files-plus-8MiB,10,1,3.119614,54.915609,75.717214,68.540312
100-files-plus-8MiB,10,2,2.946818,49.052985,66.963488,75.114023
100-files-plus-8MiB,10,3,3.112980,50.742772,67.049649,66.032923
100-files-plus-8MiB,10,4,3.744744,50.732917,70.197028,64.637165
100-files-plus-8MiB,10,5,2.609836,45.666543,66.232918,66.004966
```
