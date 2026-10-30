# Restricted Linux execution evidence, 2026-10-30

The mounted report and output record the real `TerminalPane` path on an owned,
disposable Fly VM. Static shell, Node/compiler, broker authentication, private
root/FD access checks, Unicode/ANSI, Ctrl+C, detached writers and supervisor
death passed. The recorded VM was destroyed and absence verified. No live home
or production agent was restarted.

`source-checksums.json` includes hashes of both compiled test binaries. Binaries
are deliberately excluded. The compressed source copies are the exact files
qualified; `manifest.json` hashes compressed and original bytes. Fixtures mint
temporary test credentials during execution. The archive contains no credential
or environment dumps, provider profiles, or compiled binaries.

The repeat-run script requires explicit `RUDDER_EXECUTION_FLY_APP` (an owned,
disposable target), `RUDDER_EXECUTION_CANARY_DIR` (the artifact directory), and
optionally `RUDDER_FLYCTL`. It refuses an app with secrets and verifies no
services, volumes or production environment before executing the test. It
destroys only its newly created machine in `finally` and verifies absence.
Owner paths and the app are parameterized; the historical report preserves
the actual target and machine id. The archived runner's readable names still
include `vfs` because it qualifies the VFS-backed execution path.

The artifact directory must contain the copied `scripts/guest-canary.sh` and
`scripts/fly-canary-config.json`, `controller.gz`, `native.gz`, and newly generated
`execution-source-checksums.json` for the exact binaries/sources in that run.
Build the native binary and standalone controller at the desired source revision:

```sh
cargo build -p rudder-native
RUDDER_EXECUTION_DRIVER_OUTPUT="$RUDDER_EXECUTION_CANARY_DIR/controller" \
  cargo test -p rudder-engine --test execution_mount -- --ignored --nocapture
```

These build commands do not mount filesystems, enroll devices or create cloud
resources. `controller.gz` is the gzip-compressed controller; `native.gz` is the
gzip-compressed native binary. Runtime tests and `/dev/fuse` boot delegation run
only inside the explicitly owned VM. The evidence describes one kernel/profile;
it is not a broad kernel security or performance qualification. Default Rudder
launch remains off until view/engine/VCS integration. See
[the execution contract](../../../../docs/architecture/worker-execution.md).
