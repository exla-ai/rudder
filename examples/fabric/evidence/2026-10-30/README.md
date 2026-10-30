# Retention and streaming evidence, 2026-10-30

These reports contain resource identities and checks, no credentials. The
workload-role checks ran on Fly using its OIDC identity. The independent
streaming check used the explicitly selected local AWS default admin profile.
Neither profile is copied into application code or worker images.

The remote prefix has no collector or lifecycle expiration. Root receipts retain
content indefinitely in this increment. Bucket versioning, conditional writes,
role restrictions and staging renewal were inspected against the real service.
The streaming report is a 70 MiB S3 primitive/closure test. It is separate from
the production HTTP and independent-worker restore rollout gates.

The cache-miss policy update completed at 13:01:57 UTC. The dedicated bucket's
role and permissions boundary both allow `ListBucket`: without that permission,
S3 returns 403 for a missing key and the application cannot distinguish a miss.
The update leaves immutable-object deletion denied and adds no lifecycle rule.

`workload-role-http.json` verifies the fix using the actual control machine's
Fly OIDC identity. A loopback instance of the current HTTP handler reported a
fresh miss, streamed and verified a conditional upload, published/read a new
root and restored identical bytes. Direct missing-key GET returned 404 and
immutable deletion returned 403. This does not claim a deployed authenticated
API or independent-worker restore; those remain separate rollout checks.

`deployed-independent-workers.json` closes that distinct full-transfer check at
14:40:53 UTC. Fly control release 158 used the deployed alpha.4 CAS handler and
its OIDC workload role. Two fresh alpha.4 worker VMs authenticated as separate
paired workers in a temporary synthetic account, restored all 1,766 objects
(767,656,523 bytes) into independent native stores, verified their full closures
and durably pinned the same prepared environment. Cross-account lookup returned
404; a mismatched worker/workspace pair returned 401.

Cold downloads took 152,040 and 132,115 ms. Eager exports then took 48,575 and
48,840 ms. Each restored Rust 1.99.1 compiler built a real library and a second
library consuming its API; the two library hashes match across machines. Each
restored Node/TypeScript toolchain compiled and ran a program returning 42.
The supported profile deliberately omits the Rust self-contained linker, so
native Rust executables are outside this check. The initial unsupported binary
smoke failed for that reason; the current qualifier records the exact supported
workload and its own SHA256. These timings are full-transfer baselines, not lazy
hydration, remote action hits or agent-launch measurements. Process high-water
RSS values are recorded separately, without inferring a simultaneous total.
The exact [worker qualifier](qualify-worker.mjs) is retained here with SHA256
`1f25cab80a170658866fb845a80a4358f0ad5d12162b86c74d4632e5e360f1c2`.

The initial publication uploaded every object in 294,887 ms. Reusing the
retained root uploaded zero objects; the final two warm checks took 2,358 and
1,713 ms. Immutable S3 data/receipt remain retained. Both disposable VMs were
deleted, their absence checked, synthetic auth/workspace rows removed, revoked
credentials rejected with 401, and temporary control-machine fixture files
removed. No live home was restarted. The report contains identities and proof,
not tokens or private machine configuration.
