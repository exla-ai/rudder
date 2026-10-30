# Alpha.9 deployed S3 and native restore

The [successful report](report.json) binds the published runnable-root reference,
Cloud receipt, exact alpha.9 image digest, fresh worker instance and independent
cleanup. The fixture contains15 objects: source/environment schema2, runnable
schema1, directory/shard metadata, a9MiB chunked file, raw filename, modes and
symlink metadata. Its knowledge cursor is the full unsigned64-bit maximum.

The deployed Cloud API verifies and retains the complete graph in S3. A fresh
UID1500 worker uses the shipped TypeScript transfer client and native CAS to
verify and publish its own complete local root. Source reads acquire their own
transfer pin and inventory. Exact bytes survive closing/reopening the native
helper; repeated eager restore is verified and honestly still downloads15
objects. This is not a lazy-cache or performance result.

[The archived controller](qualify-cloud.mjs) creates only a nonce-bound synthetic
account/workspace credential, checks the actual image digest, and removes that
credential and its owned VM in its finalizer. No user vault, account credential,
model call or AWS key enters the worker. Immutable synthetic S3 objects remain
under the existing no-GC retention policy. Final inventory and database queries
confirm all owned VMs are absent, no synthetic credentials remain and the49
existing production workspace records remain present.

The four earlier fixture failures are preserved separately. They cover a wrong
fixture token-hash encoding, a data-URL import using a filesystem specifier, and
source reads attempted without their required transfer-owned pin/inventory.
The final fixture fixes those contracts. It reads native-verified metadata and
content, and makes no filesystem materialization, quiescent agent-checkpoint,
default managed-launch, build-cache, fleet or cold-start claim. The controller
contains paths for its archived execution environment and is evidence, not a
general production setup command.
