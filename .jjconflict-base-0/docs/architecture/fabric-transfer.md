# Native CAS upload and restore

The machine API transfers an already sealed immutable root. It does not stop
agents, establish a writer barrier or snapshot a live checkout. The engine must
first produce a valid root reference containing `kind`, `algorithm`, `digest`
and `size_bytes`.

```sh
rudder engine api upload checkpoint-123 --root-file /owned/root.json
rudder engine api download checkpoint-123 --pin restored/checkpoint --home /owned/fresh-rudder-home
```

`--root` also accepts the exact ObjectRef as JSON. `--url` selects a Cloud base
URL; otherwise the command uses `RUDDER_CLOUD_URL` or the regular Cloud default.
HTTPS is required except for loopback or the existing explicit trusted-network
HTTP override. Credentials come from `RUDDER_CLOUD_TOKEN`, a matching private
`cloud.json` login, or the paired `RUDDER_WORKSPACE_ID`/`RUDDER_WORKER_TOKEN`.
Credentials are never command arguments. `--home` selects the local CAS without
changing which saved Cloud login authenticates the operation.

The server's authentication callback derives the account. Worker credentials
must match their workspace; a valid user token cannot impersonate a worker by
adding its workspace header. Root ids, object references, bodies and URL query
parameters cannot select another account.

`FabricLocalClient` starts one `rudder-native fabric rpc` helper per connection.
The protocol uses a bounded JSON header followed by an exact raw binary length;
objects are neither hexadecimal JSON nor a native process per file. The helper
does not instantiate App or a scheduler. `request` preserves the existing
trusted local store operations; the binary source/target methods add scoped
transfer ownership. Ordinary one-request `rudder engine api` JSON is retained.

Uploads acquire a newly allocated private source pin before inventory/read.
The complete inventory is checked, then only missing objects are uploaded under
a renewed remote staging lease. The client verifies bytes before upload; the
server verifies bytes/schema and the entire closure before its immutable root
receipt. Cleanup releases only the transfer's private pin. Caller pins remain.

Downloads begin local staging before writing any objects. Native receive
atomically checks that lease, validates SHA-256/size and canonical typed schema,
then returns the metadata's immediate child references. Blob contents remain
opaque. Traversal is bounded; a final native publication verifies and syncs the
entire closure and atomically retains the requested local pin. No source store
or mutable original checkout is needed after this succeeds.

The result separates the remote `receipt.durability=replicated` from the fresh
`local.durability=local` retention proof. Transfer does not rewrite embedded
checkpoint bytes or imply that execution, integration or deployment occurred.
An error/timeout is no proof that a publication did not commit; inspect the
returned root id or local retention before retrying an uncertain effect.

Blob size is bounded at 256 MiB and metadata at 4 MiB; inventories are bounded
at 100,000 typed objects and 4 GiB. Default client concurrency is one, with an
additional 256 MiB payload admission budget when small-object concurrency is
raised. The Node/native client currently buffers one complete blob. The Cloud
S3 path spools uploads privately and streams verified uploads/downloads with
backpressure; its global large-request admission limit is separate. Requests
have bounded deadlines and staging leases renew rather than silently replacing
expired authority.

Normal helper EOF cleans up only its own uncommitted staging and source pins.
Published pins remain. A hard process kill can leave a conservative private pin;
a committed transfer reports cleanup uncertainty separately. Failed remote
uploads can leave unreferenced objects and expiring staging records. The first
remote store deliberately has no GC or lifecycle expiry.

The actual-handler tests use the real native helper and independent fresh local
stores, a file-backed streaming adapter, a 70 MiB blob, an above-2^53 u64
checkpoint cursor, source-store deletion, user/worker account isolation,
tampered downloads and interrupted uploads. They verify local/API semantics;
live S3 and independent-worker delivery evidence are recorded separately.

The deployed full-transfer check passed on two fresh Fly workers at
`2026-10-30T14:40:53.368Z`: each restored and locally retained the same 768 MB
prepared environment, compiled a Rust library and a dependent library, and
compiled/ran TypeScript. The supported Rust profile has no executable linker.
Cold download was 132–152 s, followed by about 49 s of eager export; this is the
baseline the lazy data plane must improve. See the
[safe report](../../examples/fabric/evidence/2026-10-30/deployed-independent-workers.json)
and [qualification details](../../examples/fabric/evidence/2026-10-30/README.md).

The next source formats add chunked File descriptors and paged Directory v2,
plus ancestry-free RunnableRoot metadata. They remain an explicit development
path until caller migration and deployment are recorded. Hashes still identify
encoded metadata bytes; a File descriptor's byte size is not its logical file
length. Publication validates actual child spans, page counts/ranges and
runnable child versions even when immutable verification proofs are cached.
Old roots retain their strong history edges unchanged.
