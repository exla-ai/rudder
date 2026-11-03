# Disposable runtime and compiler qualification

To build a fresh worker candidate from one committed revision, use a new output
directory and the configured Fly executable:

```sh
python3 examples/managed-runtime-image-staging/build-candidate.py \
  --revision FULL_COMMIT_SHA \
  --output /workspace/rudder-candidate-UNIQUE \
  --fly-executable /absolute/path/to/flyctl
```

This captures `source.tar`, installs locked JavaScript dependencies in its private
`source/`, builds CLI/board/Cloud JavaScript, and remotely builds and pushes only
the `managed_c1` image. It does not run local Cargo, deploy, create a home, or
qualify SDK/runtime execution. The stock managed target builds all five native
products and bakes its runtime CAS. The appended C1 stage preserves the four
installed executable hashes and runtime inventory from that same build, retains
the actual runtime manifest, sidecar policy, component descriptors and CAS
roots, checks
the pinned C1 manifests and sealed layout, and keeps the managed entrypoint.

`build-receipt.json` records the immutable image digest, source revision/archive
hash, closed generated JavaScript map, same-build product pins, actual command
waits and private log hashes. Pass its captured `source/` and receipt to the
same-revision controller preparation. Each invocation requires unused output
and intent paths; failures retain the exact stage log and never retry. An
uncertain original command remains held. Logs use files and make no pipe-EOF
claim. Disk capacity is checked before each stage, not continuously. Use
`--prepare-only` for capture/JavaScript generation without a provider call, or
`--self-test` for pure renderer/parser checks. `--prepare-only` also consumes its
output path; it does not provide an implicit build-resume operation.

The default `--builder depot` uses Fly's Depot integration. An explicit
`--builder fly` selects Fly's traditional remote builder with `--depot=false`.
The intent, receipt and exact command bind that choice; changing services needs
a new invocation and unused output paths. The builders have separate caches.
An exporter push acknowledgment does not establish registry availability. Read
back the immutable manifest/config before starting the filesystem qualification.

Qualify a fresh isolated filesystem volume against that exact pushed worker:

```sh
env -u FLY_ACCESS_TOKEN python3 examples/managed-runtime-image-staging/qualify-volume.py \
  --build-receipt /workspace/rudder-candidate-UNIQUE/build-receipt.json \
  --flyctl-sha256 EXPECTED_FLY_ELF_SHA256 \
  --output /workspace/rudder-fresh-volume-run-UNIQUE
```

This command performs provider operations only when explicitly invoked. It checks
the captured archive/revision, original successful build waits, retained build
log, same-build product marker, completed immutable push and actual Fly bytes.
Build receipt files are provenance DATA; the operator must have observed the
original build. No saved source qualification or independent review file grants
execution authority. Keep the candidate's archive, stage records and log intact.

The existing component uses only the `rudder-v4-qualification` app and retains its
fixed protected-home identity checks. A changed protected instance or digest
refuses before resource creation. It creates one encrypted 1 GiB volume and one
machine, probes `/usr/local/bin/node`, checks write/fsync/rename/readback, then
destroys only its original resources and checks two absences plus protected-home
readbacks. Its limits remain 180 seconds of work, up to 120 seconds of cleanup,
64 calls and 16 reserved cleanup calls. Uncertain effects retain the original
child/owner indefinitely for separate reconciliation; expiry never permits a
retry or inventory adoption. A fresh invocation needs unused output paths.
`report.json` and private raw streams prove only this filesystem component;
they do not qualify a cold restart, SDK, runtime, physical erasure or deployment.
Run `python3 -B examples/managed-runtime-image-staging/test-qualify-volume.py`
for fake receipt and retained-input tests without provider calls.

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

Prepare the controller from the same candidate capture, using the qualified Node
archive and the expected SHA256 of the configured Fly ELF:

```sh
python3 examples/managed-runtime-image-staging/prepare-context.py \
  /workspace/rudder-candidate-UNIQUE/source \
  /workspace/rudder-candidate-UNIQUE/source/examples/managed-runtime-image-staging \
  /absolute/path/to/qualified-node.tar.gz /workspace/rudder-controller-UNIQUE \
  /absolute/path/to/flyctl \
  --build-receipt /workspace/rudder-candidate-UNIQUE/build-receipt.json \
  --source-archive /workspace/rudder-candidate-UNIQUE/source.tar \
  --flyctl-sha256 EXPECTED_FLY_ELF_SHA256
```

The receipt binds a full committed revision, exact archive bytes and closed
`dist/**/*.js` plus `cloud/dist/**/*.js` digest map. The packager checks its own
source, entry, installer, supervisor, Python helpers, package metadata and
Docker inputs against that archive, then checks every copied compiled module
against the captured generated map. `context-manifest.json` retains these joins
as build DATA. It grants no runtime or role qualification. The qualified Node
archive/ELF pins, Fly ELF checks, dependency closure and payload modes remain
mandatory. The output directory must be fresh. Controller builds omit the
separately disabled genuine mechanics check by default; explicitly setting
`RUN_GENUINE_INGRESS_TARGET_CHECK=true` retains that check's own source guard.

A protected `operator.json` for `staging_flow` may include `positive_ingress`
with `openCommandKey`, `startCommandKey`, optional `stopCommandKey`, `model`,
`profileId` and `text`. The installer validates this closed selection before
PostgreSQL effects and forwards it to the entry's eighth flow argument. Keys
must be distinct. With Stop selected, the original Start fence identifies the
task to stop and actual join/release observations remain mandatory. It cannot
be combined with `genuine_ingress`, whose separate false guards remain. The
existing SDK, runtime, ordinary flow, service probes and original clocks remain
in use.

The worker candidate needs its own fresh image, full inventory, manifest, installer root
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


### Full host lifecycle from one capture

`derive-lifecycle.py` renders the retained full controller/supervisor/binder and
cleanup sources. It preserves the original 900-second controller and 1080-second
cleanup lifetime, waits for both `bound.json` and the original binder completion
gate, and keeps the original child/pidfd owner. It performs no provider operation.
Use a fresh run directory beneath a private mode-0700 parent:

```sh
python3 -B examples/managed-runtime-image-staging/derive-lifecycle.py derive \
  --baseline /workspace/rudder-source37-historical-controller-compatibility-bound-source-v1/reused/adapter/baseline \
  --config /private/launch-config.json --output /private/fresh-run
```

The closed config has `formatVersion: 1`, `controlAppId` (the actual app ID from
provider readback), `credentialStorePath`, `positiveIngress`, and five references:
`workerBuildReceipt`, `controllerBuildReceipt`, `volumeReceipt`, `pgEvidence`,
`s3Evidence`. Each reference is exactly `{path, sha256}`. `positiveIngress` has the
Open/Start keys, explicit model/profile/text and optional Stop key described above.
Optional `positiveProfileSnapshot: {path, sha256}` selects one private canonical
access-only `CodexOAuthAccessSnapshotV1`; preparation checks its profile, JWT
selection/deadline and closed fields with the captured Cloud validator. It never
refreshes a credential. The snapshot file is mode 0600 beneath a mode-0700 parent.

The worker receipt must contain the actual completed original build, exact push,
SAME-build product marker and captured generated JS. The closed controller receipt
has `formatVersion: 1`, `kind: "SAME_CAPTURE_CONTROLLER_BUILD_DATA"`,
`sourceRevision`, `sourceArchiveSha256`, `generatedJsSha256`,
`contextManifestSha256`, `image`, `digest`, and `originalBuild` containing exactly
`actualWait: true`, `exitCode: 0`, `pidfdHeld: true`, `pipeEofClaimed: false`,
`logSha256`. The three capture fields must match the worker. These are artifact
DATA; actual original tool completion and immutable image verification remain the
caller's responsibility. The volume reference is the existing 17-field
`operator-reviewed-fresh-volume-component-v1` projection of the same-image raw
component report, after actual completion and cleanup review.

The qualified Node archive currently retained on this host is
`/workspace/rudder-real-pg-immutable-batches-a3/runtime.tar.gz`, SHA256
`7ce81644609f1e653be10c9c855cf3814b624166e673fae45acff86beb661342`.
The packager verifies its archive and executable pins itself.

Prepare with the fresh scoped token supplied privately in `FLY_API_TOKEN`.
`credentialStorePath` is a new private mode-0600 JSON `{token: ...}` beneath a
mode-0700 parent, containing that same token for the unchanged cleanup owner.
Preparation compares the tokens before writing a bundle. Do not put either token
or snapshot in command arguments, logs or tracked configuration.

```sh
env -u FLY_ACCESS_TOKEN node /private/fresh-run/producer/prepare.mjs \
  /private/fresh-run/lifecycle-bindings.json /private/fresh-run/bundle \
  /private/same-image-volume-receipt.json
python3 -B /private/fresh-run/supervision/run.py
```

Retain that original supervisor tool. From its actual ready and planned output,
use its numeric session, exact original identity SHA and nonce to publish boot:

```sh
python3 -B examples/managed-runtime-image-staging/derive-lifecycle.py boot \
  --run /private/fresh-run --original-session ACTUAL_SUPERVISOR_SESSION \
  --identity-sha256 ACTUAL_IDENTITY_SHA --nonce ACTUAL_NONCE --ack-original-events
```

Launch the binder through an original retained `IFS= read -r _ && exec node
/private/fresh-run/producer/bind.mjs /private/fresh-run/bundle ACTUAL_NONCE`
TTY shell. Obtain its tool session before sending one newline. Only after that
same original tool actually settles with integer exit 0, publish completion:

```sh
python3 -B examples/managed-runtime-image-staging/derive-lifecycle.py completion \
  --run /private/fresh-run --original-session ACTUAL_BINDER_SESSION \
  --original-exit-code 0 --identity-sha256 ACTUAL_IDENTITY_SHA --ack-original-events
```

The acknowledgment flag is an explicit external observation prerequisite, not an
authenticator. Both gates are exclusive writes and retain the original raw hash
checks. No saved file or PID reconstructs an original tool acknowledgment.

After admission, the original controller retains its finite-flow owners until
operator cleanup. Run the derived `cleanup/cleanup.py` with its original CLI;
it preserves the 300-second/96-call budget, captured-home subset, protected-home
checks and uncertain-effect hold. Diagnose retained SDK failure before deletion
when needed. After actual cleanup completion, publish `supervision/stop-original.json`
with the retained identity's `pid`, `startTicks` and actual `cleanupReportSha256`.
The same held supervisor reaps its original controller. Revoke the fresh scoped
token through the existing same-child revoker after actual settlement. A refusal
before any resource creation needs its own honest no-created reconciliation;
its empty built-in cleanup is not a protected-home or provider-absence scan.
