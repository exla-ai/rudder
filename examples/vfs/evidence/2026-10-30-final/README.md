# Final Linux VFS evidence, 2026-10-30

This archive contains public disposable fixtures and source, no credentials or
compiled binaries. `mounted-report.json` records actual root and UID 1500 Fly
qualification, provider/image/kernel, repeated fork/rename samples, source and
driver binary checksums, and verified VM deletion. The compressed source copies match the
qualified binary. Source is gzip-compressed to keep old snapshots out of normal
code searches; the manifest records compressed and original SHA-256 values. `native-cli-smoke.json` records the separate local CLI smoke.

The tested guest script configured `/dev/fuse` only in its disposable VM and ran
the isolated helper. The historical runner is text evidence of that execution;
its app and temporary paths describe the test environment. A new run needs an
explicitly owned disposable Fly target and corresponding paths.

Build the standalone driver from the source test with:

```sh
RUDDER_VFS_DRIVER_OUTPUT=/workspace/rudder-vfs-canary-driver \
  cargo test -p rudder-engine --test vfs_mount -- --nocapture
```

No test mounts or permission changes occur on the host during driver compilation.
The complete contract, primary references and remaining isolation/integration
gates are in `docs/architecture/vfs-qualification.md` at this revision. The
evidence proves filesystem behavior and mount delegation; same-UID host access
needs a separate restricted execution root.
