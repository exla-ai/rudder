# Inactive Cloud project authority library

This module has no server route, scheduler or automatic database setup. Importing
it or calling `createAuthority()` opens no connection. Its default mode is
`inactive`. The existing SQLite control plane and its backup path are unchanged.
Provider HA and RPO remain unqualified.

## Entry points

`index.ts` exports:

- `createAuthority(config?)`, returning the typed project API.
- `createAuthorityMigrator(postgresConfig)`, with explicit `migrate(runtimeRole)`
  and `close()` methods. It uses a separate administrator connection.
- `MonotonicLeaseFence`, `AuthorityError` and the request/receipt/context types.

PostgreSQL configuration requires explicit host, port, database, user and private
password fields. TLS verifies the server certificate and hostname, using the
configured CA or system roots. Connection strings and SSL query overrides are
rejected. Nothing reads a database URL from the environment. Do not log private
configuration. The separate `qualification.ts` helper supports only an owned,
nonce-marked private Unix socket and is not exported from the entry point.

The runtime pool defaults to four connections and sixteen queued admissions.
Connection, request, statement, lock and idle-transaction durations are bounded.
Every transaction uses one checked-out connection, tenant-local RLS settings,
synchronous commit and server deadlines. Client query-timeout shortcuts are
disabled. Capacity is released after the active operation/rollback settles or
the connection is destroyed. A request deadline destroys its connection;
settlement can finish after that deadline. No busy connection returns to the pool.

The public factory does not expose a raw query callback or driver object. Errors
contain fixed codes only, with no driver cause, SQL, parameters or connection
details. The library logs no driver errors. `stats()` reports admission counts;
`close()` drains the private pool.

## Migration and initial admission

Migration is explicit and transactional, with a migration hash and serialized
version check. It creates the configured authority incarnation as **frozen**.
It has no activation method. Production activation requires a separately
qualified provider/recovery procedure and operator action.

The administrator precreates a distinct runtime login role. Runtime connections
reject superuser, database/role creation, replication, RLS bypass, elevated role
membership and schema-owner/create privileges. The migration grants schema usage,
tenant RLS, bounded table/column DML and append-only event/receipt evidence access.
Dummy lock columns allow row locks without granting credential or authority-state
updates. Runtime cannot issue credentials, grant account privileges, register
engine instances or activate the authority. Those are operator enrollment duties.

Authenticated ingress constructs `TrustedContext` separately from operation
bodies. It contains tenant/principal, credential ID/hash and, for engine actions,
the bound engine instance, boot and separate engine credential ID/hash. Context
is snapshotted before asynchronous work. Actor identity is checked against current
PostgreSQL credential, membership, revocation and expiry records. Expiry is checked
again before commit. Bodies cannot supply authors or add context fields.

The API surface is:

| Method | Required authority and result |
|---|---|
| `admitRetainedRoot(context, RootAdmission)` | Explicit service `retention:admit` account grant. Records an already verified durable CAS retention receipt, with exact kind/SHA256/u64 size. Same root ID with different bytes conflicts. |
| `admitHandoff(context, HandoffAdmission)` | Explicit service `handoff:admit` grant. Seals a retained witness bound to project, prior epoch/engine and stopped legacy origin. |
| `bootstrapProject(context, BootstrapRequest)` | Explicit `project:bootstrap` account grant, retained source/environment and a sealed initial handoff. Creates a ready opt-in project only under an already ready global incarnation. Creator privileges/topics are explicit and credential kind is preserved. |
| `acquireOwner(context, AcquireRequest)` | Current project membership and authenticated engine/boot. CAS on expected owner epoch; fresh lease and owner epoch. A live owner cannot be taken over. |
| `renewOwner(context, RenewRequest)` | Exact engine, boot, epoch, lease ID and renewal serial. Uses a post-lock database clock and persists the original nonce/grant atomically. |
| `releaseOwner(context, ReleaseRequest)` | Exact owner identity and a sealed handoff witness; advances the epoch and removes the owner. An expired owner can report observed shutdown but cannot renew. |
| `acceptDecision(context, DecisionRequest)` | Current owner, explicit project/topic grant and expected topic version, including zero for absence. Commits decision, topic pointer, event, receipt and outbox together. |
| `admitVerification(context, VerificationAdmission)` | Explicit service `verification:admit` grant and current owner fence. Binds exact base/candidate roots/head, gate evidence, build contract and complete configured topic vector. |
| `integrate(context, IntegrationRequest)` | Current owner and exact head/version/fence/candidate/verification/topic CAS. Pending uncertain shared effects block it. Commits head, integration evidence, event, receipt and outbox together. |
| `getReceipt(context, projectId, key)` | Current credential and receipt-read membership; returns only that principal's immutable receipt. Available while authority/project admission is frozen. |

Every counter is a canonical decimal string on the JSON wire, bounded by signed
bigint. Object byte sizes retain the full unsigned u64 range separately. Counter
exhaustion fails closed. Request schemas reject unknown fields. Canonical request
bytes have incremental byte/node/depth bounds, safe integer numbers and no sparse
arrays/accessors; fractions should be encoded as strings in this wire version.

## Handoff and verification constraints

A retained witness alone cannot prove shutdown. Only a trusted verifier with the
service admission grant may attest that processes stopped, effects reconciled,
launchers were disabled and publication credentials were fenced. The module
enforces all four assertions, root kind, exact prior owner tuple and single
consumption. The HTTP/native bridge must obtain actual physical evidence before
using this admission method; an agent's narrative or a timeout is insufficient.

This initial slice conservatively requires such evidence even when replacing an
expired owner. An unowned project can reuse only the witness linked to its current
bootstrap/release receipt. Outstanding started/uncertain effects block handoff.
No process is stopped or replaced by this module.

Verification admission requires `policy/verification` and all currently configured
project topics, including absent topics at version zero. Final integration checks
coverage again, along with every recorded version and the exact tested candidate.
Changed heads, policy dependencies or topics require reconciliation and new gates.
Agent JSON cannot admit its own verification or retention proof without the
separate service credentials/grants.

## Receipts, uncertainty and worker deadlines

Mutations return `Outcome<T>`:

```ts
{ status: "committed", value: receipt }
{ status: "uncertain", reconcile: "retry_same_operation" }
```

An error before COMMIT rolls back or destroys the connection before releasing
admission. Failure after COMMIT was sent is conservatively uncertain, even if a
driver error could have followed a successful commit. Query/retry the original
principal/key and identical request; never manufacture another key. Committed
historical retries return the original acceptance/grant without advancing state
or creating another outbox record. Receipt acceptance proves no external effect,
backend acknowledgment, integration export, push or deployment.

`MonotonicLeaseFence` keeps one process/boot's deadline in memory. Call `begin`
before the first network send and reuse that request ID on transport retry. A
retry retains its first send time. `observe` accepts only matching identity and
increasing serials, computes the conservative duration from explicit drift/clock
step/margin bounds, and ignores wall-clock arrival dates. `current` must guard
each privileged action. Backward/nonfinite time, local expiry or explicit
`fence()` permanently disables the instance. A later reply cannot revive it.
On engine restart, boot change or suspend uncertainty, discard/fence it; never
reconstruct permission from a persisted timestamp.

Background renewal/watchdog and typed App-loop bridging remain caller duties.
No network call belongs in the native App loop. This slice does not implement
HTTP routes, run adapters, mailbox dispatch, effect execution, a scheduler or
full legacy-state import. It cannot make unmanaged credentials or old binaries
honor a new epoch.

## Verification

Build with `npm --prefix cloud run build`, then run
`node --test tests/project-authority.test.mjs`. The real PostgreSQL test file
requires the explicit isolated qualification state; ordinary test runs skip it.
`examples/fleet-authority/qualify-node.mjs` freezes an explicit source allowlist,
uses a separately hashed Node/pg runtime payload, and provisions only its exact
owned disposable server. Qualification evidence must identify actual passing
Node transactions, failure artifacts and verified deletion. One-server process
recovery evidence does not qualify managed failover, HA or RPO.
