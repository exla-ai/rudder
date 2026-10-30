# Alpha.10 deployed S3 to fresh native restore

The [report](report.json) records a synthetic 9 MiB, 15-object RunnableRoot
published through the deployed authenticated Cloud API and restored in a fresh
alpha.10 Fly worker. Its image digest and source revision are checked. All data
was generated public fixture content; only a temporary synthetic account/worker
credential was supplied, never a user vault, model profile or AWS key.

The real native CAS retained the root across helper restart, preserved exact
u64-max bytes, verified chunk hashes and raw-name/mode/symlink metadata, and
required source-owned pin/inventory before reads. No filesystem export or
managed agent launch was performed. The small complete restore took 4,585 ms;
this is not a cold-host or fork latency measurement. A repeated eager restore
still downloaded 15 objects. No incremental-hit or lazy-S3 claim is made.

The controller deleted its owned VM and temporary credential.
[Independent cleanup](independent-cleanup.json) confirmed both alpha.10 canaries
absent, no synthetic credential row, unchanged active-home instance and intact
SQLite. The immutable fixture objects remain in S3 under no-GC retention.
The [controller](qualify-cloud.mjs) is archived exact code requiring its frozen
local compiled source paths; it is not a general-purpose production command.
