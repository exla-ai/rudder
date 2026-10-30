# Helper-owned workspace RPC, 2026-10-30

All four explicit execution-v2 cases passed on disposable Fly machine
`8d4e15ce931228` in `sjc`: one shared CPU, 2 GiB RAM, Debian bookworm-slim
`sha256:a3bba7dc7dbcc1f37c2856c51fc9a7fac96608f50cb87b1b4e4d8b8c85ae988e`,
Fly kernel `6.12.114-fly`, Node `18.20.4`. The machine had no services, volumes,
application secrets or injected application environment. Deletion and absence
were verified at `2026-10-30T16:39:31.448Z`.

The actual `TerminalPane`/sealed-FD path ran the helper and Node guest as
UID/GID 1500. The controller's and qualification host's mount namespaces stayed
unchanged. Only this new VM received the reviewed FUSE device delegation.

- Live guest observations used the mounted view while its file descriptor
  stayed open. Original nonce replay survived a later write; source bytes and
  the explicitly declared prepared partition were checked through immutable
  roots. Foreign nonce/forged identity operations failed. A peer fsync assertion
  could not authorize adoption. RPC settled before strict unmount and the
  existing PID-namespace writer barrier preceded checkpoint acknowledgment.
- Capability revocation denied stale RPC, stopped four pidfd-witnessed
  processes and left an orphaned record with no checkpoint in 1,037 ms.
- Actual journal attempt/epoch replacement did the same in 1,037 ms.
- Wrong workspace scope failed before guest effects, with no execution record.

These timings describe this bounded fixture. Uninterruptible kernel/device I/O
still needs an outer watchdog and explicit reconciliation. Adoption witness and
default managed launch remain separately gated; this run installed neither.

The successful helper executable has SHA256
`aed21fcfd7667f6779f129851a7d91fd1a0c930ffdd9d7590771cd25ebdcdd13`;
the controller has SHA256
`345b1b2233646c24fd7f7d00294c23cf25ce3d084a629674e8c8c5ab2f5092f0`.
Binaries are excluded. `source-checksums.json` records artifact hashes and the
94-file frozen source snapshot; every archived source file was checked against
that manifest. The helper payload remained frozen during qualification. The
rebuilt controller used the current caller-side engine library while unrelated
`engine/src/lib.rs`/`remote_source.rs` work continued; remote-source APIs were not
exercised. This is working-tree fixture evidence tied to binary/file hashes,
without a claim that a complete released checkout was qualified or deployed.

`initial-failure/` preserves the first real run and its source snapshot. That
controller incorrectly expected `node_modules` excluded while declaring an
empty source-only profile. The helper honored its declared profile. Only the
fixture changed to declare the prepared partition explicitly. Its machine
`807136b605e6e8` was deleted and absence verified at
`2026-10-30T16:37:16.727Z`.

`runner-used.mjs` records the exact contained runner. `rerun.mjs` requires an
explicit `RUDDER_EXECUTION_FLY_APP=rudder-workers-exla` and frozen artifact
`RUDDER_EXECUTION_CANARY_DIR`, plus optional `RUDDER_FLYCTL`. It creates one
nonce/role/name-bound disposable machine, reconciles an ambiguous create result,
allows one explicit start for a created machine, and deletes only its exact
owned identity. The three fixture routes use private Fly 6PN, restrict requests
to that VM's source address and nonce, and verify compressed-controller, native
and guest-script SHA256 before execution. A missing/error response or missing
actual JSON proof fails the run. No SSH credentials, runtime identities,
provider tokens or authority databases are included in this archive.

See [the execution/RPC contract](../../../../docs/architecture/execution-workspace-rpc.md)
for interfaces, cancellation bounds and remaining adoption/default-launch gates.

The source archive was subsequently audited by member name and rebuilt with a
strict source/compile-asset allowlist. One non-build runtime member was omitted
from each 95-member original; no omitted member or original runtime file was
opened during this audit. Source manifests, counts and archive hashes now refer
to these 94-file sanitized overlays. Raw runtime results and executable hashes
are unchanged; `source_archive_audit` preserves the original artifact digests.
The overlays require the repository’s tracked compile-time assets as before.
