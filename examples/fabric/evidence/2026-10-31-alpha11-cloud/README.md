# Alpha.11 deployed S3 to fresh native CAS restore

The [report](report.json) records a synthetic 9 MiB, 15-object RunnableRoot
published through the deployed Cloud API and restored into a fresh worker's
native CAS. Source revision and actual image digest were checked. Only a
temporary synthetic account/worker credential was supplied, never an AWS key,
user vault, model profile or existing session.

The native CAS retained the root across helper restart, preserved u64-max bytes,
verified chunk hashes and raw-name/mode/symlink metadata, and required pin plus
inventory before reads. Filesystem materialization and managed-agent launch were
not performed. This small complete fixture took 4,646 ms; it is not a cold-host,
fork or build-cache latency measurement. The repeated eager restore downloaded
all 15 objects again. No incremental-hit or lazy-S3 performance claim is made.

The controller removed its exact VM and temporary credential. A separate
[cleanup check](independent-cleanup.json) confirmed both canaries absent, no
synthetic credential row, unchanged active home and intact SQLite. Immutable
fixture objects remain under the fabric no-GC retention policy. The archived
[controller](qualify-cloud.mjs) requires its exact frozen compiled source paths.
