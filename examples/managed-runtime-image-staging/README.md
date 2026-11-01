# Disposable runtime and compiler qualification

This setup runs one private PostgreSQL cluster and one retained qualification
driver at `https://rudder-v4-qualification.fly.dev`. It is separate from the
production control plane and worker homes. Root owns provider operations and
exact resource cleanup. No application task, model or scheduler runs here.

The Fly staging controller supplies `operator.json`, `services.json` and
`birth.json` as app-secret files under `/run/rudder-runtime-qualification`,
with mode0600 and secret environment injection disabled. It binds the private
bundle to a fresh planned UUID before creating the VM. The original supervisor
validates that Root-owned birth file before creating its work directory, and
the controller compares the actual supervisor identity and protected file
hashes before uploading public receipts or admission. The original900-second
setup limit starts before the birth read. Matching identity data grants no
ownership or role. This transport still requires an actual configured Fly run.

The protected `operator.json` selects a purpose through its validated candidate:
a runtime candidate uses `RuntimeImageQualificationDriver`; a compiler candidate
also carries `sdk` and must match `policy.compilerSdk`, selecting
`CompilerSdkQualificationDriver`. Runtime and compiler ledgers are distinct.
Compiler setup installs both compiler migrations, keeps their installation
frozen, and checks that runtime cannot publish completion or activate it.

The installer writes the selected `qualification_kind` into its original private
driver input. The driver rejects a purpose/candidate mismatch before listening
or provisioning. Both paths use the existing registry, original authenticated
Root/Native channel and known-commit publisher. Setup never inserts a ready
engine, SDK role, completion, owner lease or project.

The explicit `staging_flow` input selects the complete SDK, runtime and ordinary
source verification sequence under one immutable policy and PostgreSQL lifetime:

1. Qualify the SDK and activate it using its original committed publisher permit.
2. Qualify the runtime and activate it using its separate original permit.
3. Provision a distinct ordinary home, publish its actual source and environment,
   acquire its first provider-fenced owner, then run the accepted and denied SDK
   cases through the normal command and receipt path.

This flow requires `ownerMode: "provider-fenced-v1"`,
`sdkInitialSource: "sdk-fixed-suite-v1"`, the exact compiler catalogue and a
separate protected services file. Both qualifier scopes omit the fixed initial
source profile. Provider mode cannot replace or release an owner through elapsed
time, and the clock installation stays frozen. Setup cannot reconstruct role
activation permits from saved rows or receipts. Each listener stops accepting
new connections while its original sockets and owners remain retained.

Build coherent Cloud modules before using `prepare-context.py`. The worker
candidate needs its own fresh image, full inventory, manifest, installer root
and four executable hashes. Compiler candidates additionally require the exact
accepted SDK catalogue and its separately retained CAS. The control/PG image is
a different artifact. Prior component reports cannot substitute for these facts.

Keep the original supervisor, children, descriptors and prepared provider
intent through success or failure. Setup's 900-second limit does not extend the
driver's 300 seconds, Root's 30 seconds or provider's 15 seconds. Missing real
provider, fresh-volume, durability, origin or image evidence keeps the setup
frozen. VM deletion is separate from normal helper or filesystem retirement.

This source has not yet passed a complete configured runtime or SDK run.
Current component and build evidence is in
[the implementation record](../../docs/architecture/implementation.md).
