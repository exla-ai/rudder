# Native workspace VCS factory, 2026-10-30

The actual native AgentBroker and execution-v3 factory passed on disposable Fly
machine `8437e2f2dd5558` in `sjc`. The controller asserted one shared CPU, 2 GiB
RAM, no application secrets, services, mounts or injected application environment.
Debian bookworm-slim resolved to
`sha256:a3bba7dc7dbcc1f37c2856c51fc9a7fac96608f50cb87b1b4e4d8b8c85ae988e`;
the kernel was `6.12.114-fly`, Git `2.39.5`, and controller/helper/guest used
UID/GID 1500. Both parent mount namespaces stayed unchanged. VM deletion and
absence were verified at `2026-10-30T18:03:39.794Z`.
The three executed programs used Cargo dev/debug builds, then stripped debug
information for transfer. They are not optimized release/performance results.

This run used the production native composition rather than the earlier fixture
authority/controller. The journal stayed in the external native broker. The
helper authenticated the exact workspace/run/attempt through `BrokerClient`,
loaded canonical mode-0600 hash-bound host context from its sealed spec, and
created the real durable WC adoption witness. After its actual mounted-view RPC
was ready, it prepared the nonempty private jj WC and started the typed service.
The guest received only read-only scoped endpoints/client configuration and the
thin companion executable, never CAS, private jj metadata or the journal.

- Real guest `status`, `diff`, `log` and `new` used the registered jj-cli 0.40
  dispatcher. Same-tree `edit` used the one-observation jj-lib path. A host-side
  exact WC-pointer/baseline check verified the result under the canonical writer
  lock. Source was never materialized beside private metadata.
- `--version` reported `0.40.0-rudder-bridge-v1`. Scoped `--help` listed only the
  supported commands. Both diagnostics avoided observation/history effects;
  the separate offline tests also ran with no identity/socket present.
- The original source FD survived observations and later writes. The latest jj
  observation held `n=4`; a later FD write appeared as `n=5` in the retained
  terminal checkpoint. An actual continuously writing descendant's log was
  also captured.
- No outer fixture closed the command service or supplied a quiescence claim.
  The real helper drained guest processes, closed service ingress, awaited its
  real handlers/children and held the canonical writer guard while settling RPC,
  strictly unmounting FUSE, publishing the checkpoint and fsyncing the execution
  record. All five pidfd-witnessed processes exited.
- A second execution on the same VM advanced the real journal's run/attempt
  epoch from 1 to 2. Old workspace RPC was denied, all five witnessed processes
  exited, and the execution became Orphaned with no checkpoint. Forged command
  attempt/owner-epoch data and arbitrary `--config` argv were also rejected.

Normal companion resolution is strictly `current_exe().parent()/rudder-jj`;
missing companions fail with no stock-jj/scanner fallback. This frozen canary
used an explicit pinned image path. The factory hashed and held its ELF inode;
host command execution used that owned descriptor. A focused test replaced the
image path and proved the held original still executed. Native links the bridge
`jj` feature; only the external companion links `jj-cli` via `cli`.

`report.json`, `mounted-output.log`, `machine-config.json` and `controller.log`
record the actual VM results. `source-checksums.json` binds all executable hashes,
compiler versions and 168 explicitly allowlisted source/compile-time asset files.
`frozen-source.tar.gz` contains exactly those inputs, including Cargo.lock and
the public `assets/rudder.icns` required by the native executable. No hidden
runtime files, transcripts, executables, credentials or authority databases are
included. Every archived file was checked against its manifest.

`test-assets.tar.gz` and `test-assets.json` separately preserve the two public
compile-time routing/readiness JSON fixtures needed to compile filtered native
tests. They do not change any executed runtime binary or the runtime-source
archive. Extract both archives before reproducing the test run:

```sh
tar -xzf frozen-source.tar.gz
tar -xzf test-assets.tar.gz
cargo test --manifest-path source/Cargo.toml --locked -p rudder-native \
  --bin rudder-native workspace_vcs::tests
```

`native-tests.log` records six passing native checks: immutable canonical
context, symlink/hardlink/substitution refusal, exact run/CAS/runtime assignment,
held executable identity, strict companion lookup, and real broker authorization
with deferred preparation. Engine tests additionally exercise v1/v2 canonical
omission, v3 projection/authority checks and a real writer fence held through a
durable record. Bridge tests cover owned-child shutdown producing Uncertain,
unchanged ready-history recovery and the offline diagnostic grammar.
The frozen-source results are **6 native, 21 engine and 31 bridge tests passed**,
with zero failures. `engine-tests.log` and `bridge-tests.log` retain those checks.

`controller.mjs` requires explicit `RUDDER_NATIVE_VCS_FLY_APP=rudder-workers-exla`
and `RUDDER_NATIVE_VCS_CANARY_DIR`. Artifact preparation produces stripped
`native.gz`, `jj.gz`, `controller.gz` and `guest-canary.sh`, with all four hashes
in `native-vcs-source-checksums.json`. The controller checks the exact new
UUID/name/role/configuration, restricts private 6PN transfer to that VM's source
address and a fresh route nonce, verifies all four payload hashes before exec,
and destroys only its owned VM in `finally`. No SSH/SFTP or public service is
created. Device delegation occurs only on the new VM.

The corpus is one small source file plus an active writer, not a launch/cache
benchmark. This qualifies explicit native v3 composition and local checkpoint
semantics, not normal launch activation, real Codex/Claude execution, selected
provider credentials, peer controls/mail, transparent helper recovery, full
uncertain-command recovery, mounted checkout, fleet scaling or macOS execution.
No model session ran, and default launch remains unchanged. Live captures remain
LiveBestEffort; only the separate terminal drain/unmount path certifies the
checkpoint. Network isolation and per-agent cgroup limits were not supplied.
The workspace roots are local. This run does not open a lazy remote VFS session
or qualify remote-root launch; that requires a separate pre-mount factory and
owned remote-source/cache lifecycle.
