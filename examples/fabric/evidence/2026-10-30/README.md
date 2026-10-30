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
