# Inactive authority Node adapter qualification

The final source passed **19 tests** on PostgreSQL 17.11, `pg` 8.23.1 and Node
v22.23.4 on 2026-10-30. The final run lasted 21:17:00–21:17:38 UTC. The library
remains inactive by default; no server route, production database or deployment
was activated.

| Artifact | Meaning |
|---|---|
| [report.json](report.json) | Final result, image digest, exact source/runtime hashes and owned Machine cleanup. |
| [node-tests.tap.log](guest/node-tests.tap.log) | All 19 tests passed, no skips: nine real PostgreSQL subtests, their parent, and nine pure/config/fence tests. |
| [node-proof.json](guest/node-proof.json) | Real concurrent Node sessions, backend PIDs/blockers and the lock-delayed renewal's transaction/expiry times. |
| [server.json](guest/server.json) | PostgreSQL 17.11, fsync/synchronous_commit/full_page_writes/checksums on, network listener disabled and socket permissions 0700. |
| [postgresql.log.gz](guest/postgresql.log.gz) | Full synthetic SQL transcript, errors/rejections and actual immediate-shutdown WAL recovery, compressed without altering bytes. |
| [source.tar.gz](source.tar.gz) | Explicit code/package/test/qualifier allowlist only. SHA256 `deba6056fa19b62e8f23da08f9ba144f9d7a7fbafd359be98ff0cf0b96e6301d`. |
| [source-hashes.json](source-hashes.json) | Exact frozen TypeScript and Node test/runner sources. |
| [runtime-manifest.json](runtime-manifest.json) | Current Node executable hash, actual pg package version, compiled module hashes and separate runtime archive hash. The large runtime archive stays on `/workspace`. |
| [log-compression.json](log-compression.json) | Original and compressed SHA256 values for whitespace-preserving or large raw logs. |
| [final-cleanup-proof.json](final-cleanup-proof.json) | Independent filtered provider inventory confirms all five qualification IDs/names/nonces absent. |
| [SHA256SUMS](SHA256SUMS) | Integrity of this evidence directory. Frozen source/guest archives and runtime results are unchanged. |

The passing Machine was `78111762c2e4d8`, named
`rudder-authority-node-20261030-a5`. The official `postgres:17-bookworm` image was
pinned to linux/amd64 digest
`sha256:2cd2c0f809604aaeff6f0ec8f40aecbd025613107561015c69369a1a7f1d3416`.
It had no services, volumes, configured env or app secrets. PostgreSQL accepted
only a private Unix socket. The image entrypoint was replaced by a bounded sleep;
the runner initialized and stopped its own cluster.

The tests cover:

- Atomic migration rollback for superuser and public-schema-CREATE runtime roles;
  frozen/unsupported authority incarnation refusal and restricted runtime DML/RLS.
- Wrong tenant, principal, credential, body author and unprivileged proof admission.
- Simultaneous owner claims, original renewal-nonce receipt, old boot/process,
  stale epoch/serial, expired renewal, sealed release and epoch advancement.
- An actual Node request waiting on a project row across lease expiry, plus
  credential revocation before delayed admission.
- Simultaneous exact head integrations with one winner, exact candidate identity,
  changed topic rejection and atomic event/idempotency/outbox counts.
- **Lost application COMMIT callback:** a real COMMIT reaches ReadyForQuery, then
  the callback is deliberately discarded. The API returns typed uncertain and
  a fresh connection retries the original key without another event.
- **Separate settlement budget test:** server statement timeout and bounded queue
  pressure, then a client deadline/destruction with a deliberately delayed driver
  promise. The sole admission slot remains busy until that promise settles.
- Immediate server shutdown after project/event/receipt/outbox writes but before
  COMMIT. WAL recovery removes the whole partial acceptance; an earlier committed
  receipt still reconciles after restart. This is a process crash, not host loss.
- Signed bigint overflow, incremental canonical bounds/sparse-array rejection,
  first-send conservative deadlines and irreversible backward/nonfinite clock fences.

The archived source is the actual production library tested here, not a SQL-only
stand-in. The qualification helper uses a privileged synthetic administrator for
enrollment and a separate restricted login for domain API calls. Physical stop,
CAS retention and gate proofs are synthetic admitted fixtures; actual ingress,
native adapters, legacy cutover, provider TLS deployment, managed failover, HA,
RPO and historical disaster recovery remain unqualified.

## Reproduce

From the repository root with the existing authenticated Fly profile and private
6PN connectivity:

```sh
npm --prefix cloud run build
export RUDDER_AUTHORITY_NODE_MACHINE_NAME=rudder-authority-node-your-unique-name
node examples/fleet-authority/qualify-node.mjs --prepare /workspace/authority-node-your-unique-name
node examples/fleet-authority/qualify-node.mjs --run /workspace/authority-node-your-unique-name/state.json
```

Preparation freezes only `cloud/src/project-authority/*.ts`, the two Cloud package
files, `tests/project-authority*.mjs` and the two `qualify-node` scripts. The runtime
payload separately copies compiled authority JS, the explicit pg dependency graph
and the existing Node executable. No ledger, auth profile or user runtime state
is traversed. Both transfers use an owned-source-IP/nonce-gated private 6PN server
and SHA256 checks. No credential values enter the archive or provider command.

The [module integration guide](../../../../cloud/src/project-authority/README.md)
describes exact API contracts and remaining bridge duties.

## Earlier attempts and cleanup

[a1](failures/a1/report.json), [a2](failures/a2/report.json) and
[a3](failures/a3/report.json) stopped before library transactions due to public
runtime/test-directory permissions. a1 also exposed an empty-stdout artifact
retrieval bug; its guest logs were not retrieved, which is recorded rather than
treated as passing evidence. Their original source archives/reports are retained.
[a4](previous-passing/a4/report.json) passed all 19 tests before the final shared
role-audit/protocol hardening; a5 is the final source proof.

All owned Machines were deleted: `82025df7354278`, `8d0e30aed01448`,
`811d3e0bd23d08`, `874792eae505e8` and `78111762c2e4d8`. The independent inventory
at 21:18:30 UTC confirms absence. The passing guest additionally
[removed its data/socket and stopped PostgreSQL](guest/guest-cleanup.json).
The live home and root-owned alpha9 build canary were excluded from all mutations.
