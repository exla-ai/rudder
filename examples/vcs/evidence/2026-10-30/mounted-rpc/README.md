# Mounted jj RPC consumer, 2026-10-30

The real registered jj 0.40 CLI and same-tree jj-lib Edit passed over an actual
helper-owned mounted VFS on disposable Fly machine `843e4df2291378` in `sjc`.
The controller asserted one shared CPU, 2 GiB RAM, no application secrets,
services, volumes or injected application environment. The image resolved to
Debian bookworm-slim
`sha256:a3bba7dc7dbcc1f37c2856c51fc9a7fac96608f50cb87b1b4e4d8b8c85ae988e`;
the kernel was `6.12.114-fly`, Git `2.39.5`, and both controller and guest used
UID/GID 1500. The parent mount namespaces stayed unchanged. VM deletion and
absence were verified at `2026-10-30T17:10:28.562Z`.

The synthetic source is one small TypeScript file, plus an actual continuously
writing descendant during finalization. This is a semantic qualification, not
a launch/cache/fork benchmark or a representative repository workload.

- Real guest `status`, `diff`, `log` and `new` used the typed command transport
  and ordinary registered jj dispatcher; same-tree `edit` used the explicit
  single-observation jj-lib path. Edit's exact private
  WC commit and unchanged canonical baseline identity were checked under the
  canonical command-writer lock. No source file existed in the private metadata
  directory; the provider used immutable CAS metadata/content, never a scanner.
- The original source FD stayed open through snapshots and later writes. A
  dropped capture reply kept the original retained UUID/root/bytes after a
  consumer-object restart and another FD write. Inspection made zero new
  Observe calls. Explicit retirement removed only that uncommitted candidate.
  Generations advanced from 2 to 4 while the retained original still held `n=2`.
- A completed `new` reply was dropped. Restarting the command service rotated
  its capability while preserving the original command UUID, receipt and jj
  operation. Inspect and same-ID retry returned the identical completed result,
  with no second `new` operation or auto-rebase.
- Guest attempts to change the owner epoch or attempt identity were denied,
  as was arbitrary `--config` argv. Existing real host fixture files were
  inaccessible inside the guest and through its `/proc/1/root`.
- Finalization was host-owned: close ingress, await actual handlers, acquire
  and hold the canonical inherited writer lock, then allow guest exit. All
  five pidfd-witnessed processes exited. The helper drained its PID namespace
  and observation RPC, strictly unmounted FUSE, then acknowledged a retained
  checkpoint. The last jj observation held `n=4`; a later write through the
  same FD appeared as `n=5` in that checkpoint. The active writer's log also
  survived the checkpoint.
- A separate exact execution/attempt on the same VM lost its outside fixture
  authority. Old workspace RPC was denied, all five witnessed processes exited,
  and the record became Orphaned with no checkpoint.

The authority service was a protected fixture in the outer controller, outside
helper and guest. The helper opened no journal. The guest received only scoped
workspace/command endpoints and their own capabilities, never CAS, private jj
metadata, the authority service credential or host configuration. The native
AgentBroker/production launch path was not used and remains unqualified here.
The service and consumer were restarted; the mounted helper process was not
transparently restarted. No real model/provider session was run.

The injected witness verified configured durable WC state, exact source nonce,
projection and actual private jj tree under a distinct publication file lock.
An ordinary peer fsync assertion cannot perform adoption. This qualifies that
explicit witness composition, not arbitrary guest-provided evidence.

`report.json`, `mounted-output.log`, `machine-config.json` and `controller.log`
are the actual results. `source-checksums.json` records the exact 153-file frozen
working-tree snapshot, toolchain and compressed/uncompressed executable hashes.
`frozen-source.tar.gz` contains those sources and Cargo.lock, without binaries,
runtime identities, authority databases or credentials. Its files were checked
against the source manifest before execution and on archival. This is evidence
for those working-tree file/binary hashes, not for an entire released revision.
The shared archive contains 153 source files. An archive-member audit found an
ignored runtime transcript copied alongside source; its three tar entries were
removed without opening the transcript. It is not a build input. The manifest
and report explicitly record archival sanitation, original execution-manifest
identity and the new shared-archive hash. Actual binaries, runtime observations
and cleanup results are unchanged. Future freezes use an explicit source and
compile-time asset allowlist. The original checkout runtime was left untouched.
`bridge-tests.log` records a fresh test run against that same frozen source:
**27 passed, 0 failed**, including all five scoped RPC/actual-CLI tests. The
`cli` profile excludes the separate local-development scanner/RO fixtures.

Rebuild from the archive using the recorded toolchain, then explicitly select
the authorized disposable app and an artifact directory:

```sh
tar -xzf frozen-source.tar.gz
cargo build --manifest-path source/Cargo.toml --locked -p rudder-vcs-bridge \
  --features cli --example qualify_mounted_rpc --bin rudder-jj
# Prepare stripped copies as controller.gz and jj.gz; record both SHA256s in
# vcs-source-checksums.json and include guest-canary.sh in that artifact folder.
RUDDER_VCS_FLY_APP=rudder-workers-exla RUDDER_VCS_CANARY_DIR=/workspace/artifacts \
  node controller.mjs
```

The controller requires explicit app/artifact inputs, reconciles an ambiguous
create result, validates exact UUID/name/role/configuration, and destroys only
its owned machine in `finally`. Payload transfer uses private Fly 6PN HTTP,
restricted to that exact VM's source address and a fresh route nonce. All three
payload SHA256s are verified before execution. No SSH/SFTP or public service is
created. The reviewed FUSE device delegation occurs only on the new VM.

`initial-failure/` preserves the first run and its exact source snapshot. That
fixture incorrectly treated the first topological `jj log` row as the current
WC pointer after editing a parent. The actual edit succeeded. The corrected
fixture checks the exact private WC pointer under its writer lock and preserves
the history check. Only example assertions/negative controls changed; product
engine/bridge sources were unchanged. Its machine `8d4162be421e08` was deleted
and absence verified at `2026-10-30T17:07:20.142Z` before creating the next one.

Default activation, native-broker integration, transparent helper recovery,
semantic reconciliation of a Started/Uncertain command, tree-changing checkout,
source-subset derivation, fleet scaling and macOS execution remain gates.
Live observations remain LiveBestEffort; equality of generations is not a
writer barrier. Network isolation and per-agent cgroup limits were not supplied.
