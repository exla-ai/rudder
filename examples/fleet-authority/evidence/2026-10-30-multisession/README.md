# PostgreSQL concurrency and recovery evidence

All 36 assertions passed on a real PostgreSQL 17.11 server using separate Unix
socket client sessions, on 2026-10-30 from 20:24:30 to 20:25:14 UTC. Both
disposable Machines used during qualification were deleted. This qualifies the
documented SQL updates and this synthetic transaction harness on one server;
it does not qualify an application integration, provider HA or an RPO contract.

| Evidence | Result |
|---|---|
| Server | PostgreSQL 17.11, Debian `17.11-1.pgdg12+2`, x86_64. |
| Image | Official `postgres:17-bookworm`, resolved and pinned to `docker.io/library/postgres@sha256:2cd2c0f809604aaeff6f0ec8f40aecbd025613107561015c69369a1a7f1d3416`. |
| Passed Machine | `8233e0b7242698`, `rudder-fleet-sql-20261030-9b68e4-r2`, Fly region `iad`. |
| PostgreSQL configuration | `fsync=on`, `synchronous_commit=on`, `full_page_writes=on`, data checksums on, WAL sync method `fdatasync`. |
| Isolation | No services, volumes, configured environment variables or app secrets. PostgreSQL `listen_addresses=''`; socket directory and socket permissions `0700`. No user checkout or production state was mounted/copied. |
| Main report | [report.json](report.json), including all results, source hashes, resolved image and deletion verification. |
| Server proof | [server.json](guest/server.json), [socket permissions](guest/socket-permissions.txt), [PostgreSQL log](guest/postgresql.log). |
| Assertion proof | [result.json](guest/result.json), including all 36 assertion records and receipt values. |
| Concurrent sessions | [concurrency.jsonl](guest/concurrency.jsonl), per-session `.sql`, `.log`/`.log.gz` and `.exit` files in [guest](guest). |
| Frozen source archive | [source.tar.gz](source.tar.gz), SHA256 `add521463a5068a5c4ef6126b55f6faf9111688d1ecc587b26bec7fb5156f693`. Code/document/SQL only, with a file manifest; no database directory or credentials. |
| Integrity | [source-hashes.json](source-hashes.json), [documented-updates.sql](documented-updates.sql), [SHA256SUMS](SHA256SUMS). |

Only raw `.log` files flagged by the Git whitespace check are stored as
`.log.gz`. Decompression preserves every original byte, including psql alignment
and trailing blank lines. [log-compression.json](log-compression.json) records
raw/compressed hashes; the frozen source and guest archives remain unchanged.

The image was resolved through Docker's official registry, including the
`linux/amd64` manifest digest, before provisioning. Its entrypoint was replaced
with a bounded sleep, so the image did not create a default network database.
The runner explicitly initialized its own private cluster. See the
[official image documentation](https://hub.docker.com/_/postgres).

## Exercised behavior

| Scenario | Observed result |
|---|---|
| Concurrent owner claims against epoch 1 | Backend 718 held the project row; backend 745 waited on its transaction ID. Exactly one accepted claim, one conflict, one event and owner epoch 2. |
| Renewal retry and stale identities | Original renewal nonce returned its identical durable grant; serial advanced once. Old boot, another engine process within the same boot, old epoch and old renewal serial were rejected. |
| Lock wait crosses expiry | Backend 821 waited on backend 794. The post-lock clock rejected the expired lease and left renewal serial 0. The transaction-start clock would still have allowed it. |
| Expired renewal competes with reassignment | Backends 854 and 880 overlapped. The renewal could not revive the old lease; replacement advanced the owner epoch to 3 and fenced the old owner. |
| Competing integrations | Backend 958 waited on backend 930. Both candidates were verified against the same exact base and policy version; exactly one integration/head advance committed. |
| Topic update during integration wait | Backend 1023 waited on backend 995's policy change. Its old verification vector was rejected and the base head remained unchanged. |
| Candidate does not match verification | A different candidate/root with another candidate's verification was rejected without an acceptance record. |
| Commit reply deliberately discarded | The first client did not consume an acceptance receipt and was terminated after commit. A new connection retried the same key and obtained the identical receipt without another event. Changing request bytes under that key conflicted. |
| Client/backend exits before commit | Termination during an open transaction removed the owner change, receipt and outbox; the original project state remained. |
| Server stops with transaction open | Immediate PostgreSQL shutdown forced crash recovery. WAL redo preserved the earlier committed receipt; the partial owner/event/receipt/outbox transaction disappeared. A new-session retry after restart still returned the original committed receipt. |
| Final accounting | Every project's committed event count matched its sequence; the committed receipt had one deduplication record. |

The expiry observation was:

| Timestamp | UTC |
|---|---|
| Waiter's transaction started | `20:24:56.100664` |
| Existing lease expired | `20:24:57.008589` |
| Clock sampled after project lock acquired | `20:24:58.037309` |

The clock test explicitly allows a 3 s lock wait and 4 s statement duration to
exercise this crossing. Artificial blocker sleeps only coordinate the test;
production transaction timeouts and performance are not qualified here.

The [pre-crash witness](guest/pre-crash-witness.json) records the committed
receipt plus current/flushed WAL LSNs from an independent client. The
[post-crash witness](guest/post-crash-witness.json) retains the identical receipt.
The server log records interruption, redo start/completion and the recovery
checkpoint. This uses `pg_ctl -m immediate`, which causes recovery on restart;
it is a database-process crash, not a host power-loss or AZ-failure experiment.
[PostgreSQL shutdown/recovery semantics](https://www.postgresql.org/docs/17/app-pg-ctl.html).

## Reproduce and audit

The [controller](../../qualify.mjs) extracts the two UPDATE statements from
[the design document](../../../../docs/architecture/fleet-authority.md), changes
only named placeholders to typed SQL function parameters, then archives and
hashes them. [qualify-protocol.sql](../../qualify-protocol.sql) supplies synthetic
authorization guards, canonical request bytes, seeded roots, receipts/events/
outbox and controlled competing operations.
[qualify-guest.sh](../../qualify-guest.sh) creates independent `psql` sessions,
observes real blockers, asserts database state, and crashes/restarts the server.

The original [schema fixture](../../schema.sql) is installed only in this
disposable database by replacing its final `ROLLBACK` with `COMMIT`; the executed
copy is [installed-schema.sql](guest/installed-schema.sql). The application does
not install these functions or enable Cloud authority.

From the repository root, with the existing authenticated Fly profile, private
6PN connectivity and a fresh qualification name:

```sh
export RUDDER_FLEET_SQL_MACHINE_NAME=rudder-fleet-sql-your-unique-run
node examples/fleet-authority/qualify.mjs --prepare /workspace/fleet-sql-your-unique-run
node examples/fleet-authority/qualify.mjs --run /workspace/fleet-sql-your-unique-run/state.json
```

Prepare resolves the pinned image and freezes source without provisioning. Run
creates and deletes one owned disposable VM. It checks that the app has zero
secrets, rejects an existing name, excludes home `8edd225a7d0378`, and requires
exact name/nonce/role ownership before deletion. Source transfer is bound to a
private 6PN address, the exact owned source IP and a random per-run route; every
file and the archive are checked before execution. No app env is copied and no
credential values enter commands, archives or reports.

Actual user authentication, run adapters, CAS pin admission, durable relay
implementation, provider replication/failover, historical restore, host reboot,
clock jumps, and physical legacy-writer cutover remain separate gates. These
fixtures use a privileged PostgreSQL user and synthetic identities/retained roots
to test transaction behavior; they cannot establish those application contracts.

## Failure and cleanup record

The [first attempt](failures/01-fixture-permissions/report.json) stopped before
protocol tests: root-created mode-0600 SQL input was unreadable to the non-root
client. Its [install log](failures/01-fixture-permissions/guest/install-schema.log)
and original source archive are preserved. Feeding SQL through stdin corrected
the harness; schema/design behavior required no correction.

The first Machine, `867639fe1ed178`, was deleted at `20:23:31 UTC`. The passing
Machine, `8233e0b7242698`, was deleted at `20:25:14 UTC`. Each report confirms the
owned ID absent and zero matching name/nonce/role inventory rows. The passing
[guest cleanup](guest/guest-cleanup.json) additionally confirms PostgreSQL stopped
and its data and Unix socket directories were removed before VM deletion. No
production Machine or project was restarted.

An [independent inventory read at 20:27:28 UTC](final-cleanup-proof.json) again
confirmed both IDs, names and qualification nonces absent after both runners ended.
