# Cloud worker boot readiness

The image starts its small, image-owned `root-bootstrap.sh` as root, then execs
normal `setpriv` into UID/GID 1500 before the existing worker entrypoint can
source cache/profile code, update tools, read credentials or start either
supervisor. Real/effective/saved IDs are dropped, supplemental groups and
capability sets cleared, and no-new-privs inherited. No privileged/setuid helper
is added. Root-phase commands use absolute image-owned paths, including a fixed
`/bin/sh` interpreter; user tool directories are not consulted.

`RUDDER_WORKER_MANAGED_EXECUTION=1` explicitly requires managed boot readiness.
The default remains legacy launch. A safe existing `/dev/fuse` is a non-symlink,
single-link character device with major/minor 10:229, initially root- or
UID1500-owned. Boot delegates only that device to 1500:1500 with mode0600 and
verifies the same inode afterward. An absent, wrong, aliased or unexpectedly
owned device is not created or repaired. Managed boot fails before account
scripts; legacy boot still drops privileges and continues without managed
readiness. `RUDDER_WORKER_FUSE_READY` reports the observed result and is not an
authorization capability; the execution helper still proves its kernel profile
and authenticates the exact attempt through its protected broker.

A root-starting Fly image can receive a root-owned volume mount destination.
The boot exception is fixed to `/workspace`: require a real non-symlink directory,
and require an exact `/proc/self/mountinfo` mountpoint before repairing UID0
ownership. Change only that directory inode to1500:1500, preserve its original
mode and verify identity. Never recurse or select a path from user environment.
Existing child inodes, owners, modes and contents remain intact. An already
1500:1500-owned image directory needs no change; unexpected ownership blocks an
explicit managed request. This addresses
[Fly's documented mount ownership behavior](https://community.fly.io/t/fly-volumes-getting-permission-denied/1773).

Direct non-root BYOVM legacy entrypoints preserve their user, home and groups.
A managed BYOVM launch needs a pre-granted device and UID/GID1500, and should
start through the bootstrap to establish no-new-privs before the entrypoint's
verification gate. Its fixed `/workspace` must already be a non-symlink directory
owned by UID1500 and writable/searchable by that user; this path performs no
ownership repair. The entrypoint's kernel identity/capability checks prevent a
self-asserted environment marker from bypassing that gate.
The non-root privilege command explicitly preserves this already validated
group set; root boot instead clears supplementary groups before dropping IDs.

The privilege operation uses Debian's normal non-setuid
[`setpriv`](https://manpages.debian.org/bookworm/util-linux/setpriv.1.en.html),
with no-new-privs retained through child execs. The bootstrap performs bounded
metadata work on the device and mount top; it does not scan a workspace or walk
credentials. It does not provide network isolation, cgroup limits or a fleet
scheduling lease.

The runtime and Git builder explicitly use Debian Bookworm. Git2.56.0 is built
from the [official source tarball](https://git-scm.com/install/source), checked
against the fixed upstream SHA256
`26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3`.
Both version and digest must change together. No other Debian release/suite is
mixed into apt. The separate build stage remains reusable across native changes.
Git2.56 builds its Rust library by default; its
[pinned Cargo manifest](https://github.com/git/git/blob/v2.56.0/Cargo.toml)
requires Rust1.49 or newer and declares no external crates. The builder includes
Bookworm's normal `cargo`/`rustc` packages rather than disabling that library;
these build tools are not copied from this stage into the runtime.

This fixes the Bookworm Git2.39 compatibility gap: Git2.41 introduced
[`fetch --porcelain`](https://github.com/git/git/blob/master/Documentation/RelNotes/2.41.0.adoc),
which pinned jj0.40 uses. Upstream Git lives under `/usr/local`; the managed
guest's default PATH includes `/usr/local/bin:/usr/bin:/bin`, inside its explicit
read-only `/usr` runtime bind. The worker retains the jj0.40 pin. Qualification
must perform a changed local/file-remote jj fetch, not just inspect version text.

`tests/fixtures/worker-bootstrap/` is an explicitly owned disposable-VM fixture.
Its nonce/machine marker gate precedes package/device operations. It tests home
and sail dispatch, argument forwarding, updates/credentials/cache access after
drop, malicious utility PATH entries, saved IDs/groups/capabilities, safe and
unsafe FUSE shapes, legacy fallbacks, non-root BYOVM behavior, a real bind-mounted
root-owned `/workspace`, preservation of an existing child tree and post-drop
writes. It also runs the actual four-case managed helper qualifier after the
privilege floor, with pinned Git/jj file-remote fetch inside the restricted guest.
Never execute this fixture against the current host or an active home.

The controller is built by Cargo as `rudder-execution-qualifier` in the native
package and copied/probed via `CARGO_BIN_EXE_rudder-execution-qualifier`:

```sh
cargo test -p rudder-native --test execution_mount
RUDDER_EXECUTION_DRIVER_OUTPUT=/workspace/home/.cache/rudder-worker-bootstrap/controller \
  cargo test -p rudder-native --test execution_mount -- --ignored --nocapture
```

Those commands perform no namespace/device changes. `root-bootstrap.sh` and the
entrypoint can also be checked with `sh -n`. Actual boot qualification belongs
only on a newly owned VM; archive source/binary hashes, raw assertions and exact
machine cleanup proof. These checks do not replace a full multistage image build
or authorize deploying WIP to active machines.

The [owned Fly boot proof](../../examples/execution/evidence/2026-10-30-worker-boot/README.md)
passed all 19 boot cases and all 4 managed helper/RPC cases after the privilege
drop. It includes the actual changed file-remote Git/jj fetch and exact cleanup
proof, plus preserved failures that led to explicit Rust build prerequisites and
the managed non-root group-policy correction.

The worker now builds explicit `rudder-native` and `rudder-jj` targets from the
same source graph and installs both under `/opt/rudder/dist/native`. This paired
packaging change follows the frozen boot proof; that proof's binaries are not
relabeled as the paired image. `/usr/local/bin/jj` remains stock pinned jj0.40
for legacy/user commands. Packaging the bridge does not activate managed launch.
