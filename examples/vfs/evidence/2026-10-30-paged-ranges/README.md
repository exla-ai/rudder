# Paged directory and block-range VFS qualification

Verified 2026-10-30T15:32:54.859Z on disposable Fly.io VM `813212b92535d8`, instance `01M66Q10TJSFSWDF1K38M94JR4`, `iad`, 2 shared CPUs / 2048 MiB. Kernel `6.12.114-fly`. Both complete mounted matrices passed as UID 0 and fresh UID 1500; the VM was then destroyed and absence verified.

This is the explicit `paged_ranges_v2` data plane with a local verified CAS resolver. It does not enable a product default, qualify remote page faults, or prove complete worker launch/build latency. The active Cloud home was not restarted.

## Measured mounted operations

Preparation below registers an already published immutable root. Initial fixture construction/publication is recorded separately in the raw stage events. Fork samples include the persistent retention/descriptor writes; retention JSON size still grows with pin count. Shared CPU scheduling and storage variance are visible in the samples.

| UID | Inherited entries | Retained-root preparation (ms) | Fork median of 5 (ms) | Rename median of 5 (ms) | Private inodes after rename |
| --- | ---: | ---: | ---: | ---: | ---: |
| 0 | 1 | 7.19 | 15.51 | 5.59 | 3 |
| 0 | 100 | 1.67 | 15.81 | 7.38 | 3 |
| 0 | 10,000 | 2.22 | 21.33 | 6.55 | 3 |
| 0 | 100,000 | 2.12 | 12.34 | 5.27 | 3 |
| 1500 | 1 | 2.13 | 13.11 | 5.24 | 3 |
| 1500 | 100 | 6.43 | 68.04 | 8.44 | 3 |
| 1500 | 10,000 | 1.68 | 15.04 | 6.43 | 3 |
| 1500 | 100,000 | 2.44 | 14.24 | 5.53 | 3 |

The 208 MiB file has 52 logical 4 MiB chunks. Its first seven-byte write appended one 64 KiB private block. After an additional mmap write across a chunk boundary, private data totaled 128 KiB; checkpoint/remount preserved content and reused 50 of the 52 original chunk references. A separately mounted sibling retained the original bytes. These are measured private data bytes, not RSS or total checkpoint cost.

The matrix also passed raw byte names, symbolic links, independent equal-content inode identity, real hardlinks and hardlink restoration, unlink with an open FD, atomic saves, modes, shared mmap/msync/fsync, strict busy unmount, detached descendant detection/reaping, mounted live observations with an open FD, and acknowledged-write/rename recovery after server abort. Missing `/dev/fuse` access failed closed. UID 1500 could not read the fixture root-only secret, enter the initial mount namespace, or read `/root`. PID 1’s mount namespace was unchanged. No setuid helper was installed.

## Exact artifacts

Standalone driver SHA256: `a3e912699db1a01e1f7986993bbaaa9cae98b91e282fd76a5502f740a417a078` (8,787,856 bytes). Built with `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, x86_64 GNU Linux, release engine thin LTO and driver opt-level 3 / debug assertions off. The binary is not stored in Git.

- `fly-mounted.json`: provider identity, cleanup proof and all parsed results/stage timings.
- `mounted-output.log`: full bounded guest output, including both complete matrices.
- `frozen-source.tar.gz`: exact 40 engine/build inputs, including Cargo manifests/lock, the embedded standalone driver, VFS, content, directory and store implementations.
- `build-inputs-sha256.json`: individual hashes for those frozen inputs.
- `guest-canary.sh`: guest test and per-process deadlines, device grant limited to the disposable VM.
- `controller.mjs`: exact host-side lifecycle/transfer controller, with private local state paths and no credentials.
- `initial-debug-timeout.json` / `initial-debug-output.log`: the earlier bounded debug-build failure. It reached the 100k fixture phase and timed out before mounting that case. The old phase label covered allocation/build/publication/trials, so it is not a measurement of retained-root preparation. This result was not treated as a pass.

To rebuild from the archive in a disposable directory:

```sh
tar -xzf frozen-source.tar.gz
RUDDER_VFS_DRIVER_OUTPUT="$PWD/vfs-driver" cargo test --release -p rudder-engine --test vfs_mount mounted_driver_compiles_without_touching_host_mounts -- --nocapture
```

Run the guest script only in a newly owned disposable Linux VM. It changes that VM’s `/dev/fuse` group/mode and creates isolated mount/user namespaces. Do not run it on a developer host or active Cloud home. The controller used a temporary private Fly 6PN fixture listener restricted to the exact VM source address and nonce; it verified the compressed artifact SHA256 before execution and closed the listener during cleanup.

## Remaining gates

Initial ingestion/publication and retained-anchor metadata walks still scale with the content graph; this does not claim end-to-end work proportional only to dirty bytes. The mounted readdir compatibility path collects entries and requires a bounded cursor replacement before broad default use. The resolver is local-only; remote download admission, lazy hydration/block caching and eviction need their own failure/cancellation/availability proof. Live observations are explicitly best effort and cannot serve as quiesced launch seals. Workspace/build/VCS integration and native Mac filesystem behavior require separate qualification.
