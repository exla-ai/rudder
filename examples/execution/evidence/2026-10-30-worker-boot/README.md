# Owned worker boot proof, 2026-10-30

All 19 boot cases and all 4 actual execution-v2 helper/RPC cases passed on newly
owned Fly machine `830d6db7507118` in `sjc`, with one performance CPU and 2 GiB RAM.
The machine used Debian bookworm-slim
`sha256:a3bba7dc7dbcc1f37c2856c51fc9a7fac96608f50cb87b1b4e4d8b8c85ae988e`,
kernel `6.12.114-fly` and Node 18.20.4. It had no application secrets, services,
volumes or injected application environment. Exact name/nonce/role-bound
deletion and absence were verified at `2026-10-30T18:05:28.465Z`; the controller
host's mount namespace stayed unchanged. No active home was changed.

The actual boot code dropped real/effective/saved UID/GID to 1500, cleared
supplementary groups and capability sets, and established no-new-privs before
cache setup, credential access, update probes or home/sail dispatch. Root boot
also cleared the capability bounding set. Only the safe existing character
device 10:229 received UID/GID 1500 and mode 0600. A real root-owned bind mount at
`/workspace` exercised the fixed top-inode ownership repair: mode was preserved,
all existing child inodes/owners/modes/content stayed unchanged, and a newly
dropped process could write there. User utility PATH entries were not used by
root boot. Invalid device shapes, foreign/read-only/unmounted workspace cases
and a managed entrypoint without its privilege floor failed before scripts.
Legacy BYOVM user/home/groups and missing-device behavior were preserved.

The pinned official Git 2.56.0 source was SHA256-verified and compiled with its
default Rust library, using Bookworm cargo 1.65/rustc 1.63 and the 480 s deadline.
The resulting fixture Git SHA256 is
`1425cba34f0e170a645f7d91d7b9721a55d324bfe17853f54d8449c417bc8531`.
Pinned stock jj 0.40.0 actually fetched a changed local/file remote inside the
restricted guest through its `/usr/local/bin:/usr/bin:/bin` default PATH.
This tests the operation requiring Git's `fetch --porcelain`, beyond versions.

After the same privilege floor, the mounted-view RPC qualifier exercised live
observations while a file descriptor stayed open, original nonce replay,
foreign/forged operation denial and unsupported peer-asserted adoption. RPC
settled and the PID-namespace writer barrier preceded checkpoint acknowledgment.
Real capability revocation and journal attempt/epoch replacement each stopped
the 4 pidfd-witnessed processes, denied stale RPC and produced no checkpoint
(951 ms and 950 ms respectively). Wrong scope failed before guest effects.

The frozen native helper SHA256 is
`49a4e1bb7c5c2615242fac59755e59309e2873c063026c2b4627638e3ee91bbc`;
the Cargo-built controller SHA256 is
`8a73844ea796172c09c35cb3e6d26fe84d0a8ed5e3d379e62da149da26f61a76`.
The exact source-manifest SHA256 is
`0d5576efd4011d109c5d2866d92b1359e8f101417ce66d8a2bdaf5d8afdad52a`.
Binaries and upstream tool archives are excluded; their hashes/download sources
are recorded. The 119-file frozen overlay is source-only, using an explicit code
and compile-asset allowlist. One non-build runtime member was omitted by
member-name audit without opening that member or its original. Sanitized source
manifests/counts/archive hashes and original source-artifact digests are recorded;
runtime output and executable hashes are unchanged. Each failure overlay uses
the same filter. These are working-tree overlays requiring the base repository's
tracked assets, not complete standalone checkouts.

`failures/` preserves the two fixed-deadline shared-CPU build failures (no Git
binary produced), the missing default Rust-build prerequisites, a Node primary
group fixture error, and the actual missing non-root setpriv group policy.
Every exact machine was deleted and its absence verified. The corrections did
not disable Git features or relax boot assertions; kernel supplementary groups
are checked separately from Node's primary-inclusive `getgroups()` result.

This qualifies the matching Bookworm boot components and the frozen execution-v2
payloads, using dispatch/update/credential probes. It is not a full multistage
worker-image or Node 22 image qualification, a v3 application proof, or a default
managed rollout. `post-proof-packaging.json` explicitly separates the subsequent
paired `rudder-native`/`rudder-jj` Docker build/copy change; the frozen binaries
were not rebuilt or relabeled. Stock `/usr/local/bin/jj` remains pinned 0.40.

`runner-used.mjs` is the exact contained runner. `rerun.mjs` additionally requires
`RUDDER_BOOT_FLY_APP=rudder-workers-exla`, a frozen artifact directory in
`RUDDER_BOOT_CANARY_DIR`, and optional `RUDDER_FLYCTL`. It needs private Fly 6PN
connectivity; three source-IP/nonce-scoped routes and payload SHA256 checks
precede execution. Only a fresh exact-name/nonce/role disposable VM may run
`guest-canary.sh` or the device/mount matrix. Never run those fixtures on the
current host or an active home. A full immutable image build and image-level
smoke checks remain necessary before release/deployment.

Failure `mounted-output.log.gz` files preserve the original runtime bytes
(including package-manager carriage returns); only compression was applied.
