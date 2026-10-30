# Alpha.10 deployed image and control proof

Source: `56f825ea3c1d6136b1a20b3031f835ef33f870cd`, tag `v4.0.0-alpha.10`.
The [image report](report.json) records the actual Fly worker digest, UID1500
bootstrap, zero capability sets, no supplementary groups, NNP, boot-delegated
FUSE, user-owned caches, packaged CLI/native/companion and real changed-remote
jj fetch. The image-owned Node preload ran after real bootstrap/entrypoint and
held before supervisor module evaluation. No model, vault or user credential was
used. This is not managed-model launch, network isolation or fleet adoption proof.

The [control proof](control-proof.json) verifies canonical app/health/auth,
actual revision/digest, SQLite integrity and existing home retention. Release 161
uses the configured alpha.10 worker image. The new pg 8.23.1 library is present
and its default factory opened no connections. Production fleet authority is
inactive. The running home stayed on its unchanged alpha.7 instance.

[Independent cleanup](independent-cleanup.json) confirmed both owned canaries
absent, zero synthetic credential rows and database integrity. Immutable S3
fixture objects remain retained by design. The frozen source summary confirms
all 1,301 tagged files matched the build inputs; only package version metadata
changed after the pre-version credential audit.

The controllers are archived exact qualification code, with private local build
paths and explicitly selected app/machine identities. They are not portable
production commands. Initial extra control inspection tried resolving tsx from
Fly exec's root directory; the corrected read-only inspection imports the built
JS module. No product change was needed. Worker build push succeeded; builder
cleanup emitted a deadline warning afterward, and the canary independently
verified the pushed digest. npm publication is recorded separately from Cloud.
