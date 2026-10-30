This evidence records a standalone local auth-contract check against Codex CLI
0.166.0 at 2026-10-30T22:16:10Z. The actual running executable SHA256 was
`b8a11fedf24b2dfb59a70c9e52b030f9a508dcd49cd2dd0d41956f4c785156b8`.
The test used a new private fixture home and a public synthetic API-key string;
it read no existing provider credentials, auth files, sessions or transcripts.

The host connected to the exact held child PID via a Unix socket, checked kernel
peer PID/UID, initialized, required effective ephemeral storage with `config/read`,
sent one `account/login/start` with type `apiKey`, and required an `apiKey` account
from `account/read`. It sent no thread or model request. Afterward it explicitly
killed the owned child and completed `wait`, observing SIGKILL. It found no
`auth.json` and no synthetic-key bytes in any regular file of the fresh fixture.
See [the safe result](synthetic-codex-0.166.json).

The first final run passed auth and child cleanup, then failed its original
post-auth file scan limit. The new Codex home contained 5,502 regular files and
87,111,201 bytes, including a 25,495,172-byte Git pack at
`codex/.tmp/plugins/.git/objects/pack/pack-f811a37b58b15bd30e1edad7816103ab8e2af16e.pack`.
Its `PACK` header was verified. [The failure record](scan-bound-failure.json)
preserves the observed limit and original test source hash. The final rerun
scanned every regular file using 64 KiB buffers with overlap for cross-chunk
matches, explicit count/byte limits, and a 15-second scan deadline. It skipped
no cache or core files and followed no symlinks. That run passed with 5,502 files
and 87,112,164 bytes scanned. No fixture home, cache, server log, credential or
session file is included here.

The startup footprint is a follow-up: fresh Codex homes downloaded/generated
about 87 MB and 5,502 files even without inference. Fast default launches need
an immutable shared provider cache/profile or a supported opt-out for unused
discovery. No unverified feature flag was added.

[Source hashes](source.sha256) identify the nine auth/barrier implementation and
test files. [Focused checks](checks.json) record commands, results and local log
locations. The broader engine baseline ran before these auth changes; these
focused checks are the affected reruns. The subsequent release checks are
recorded separately by the release owner.

This check did not run the managed helper, a real provider/model, OAuth refresh,
or a guest network-isolation test. Internal/default managed launch stays
inactive. The current guest shares host networking; the host board's loopback
access is a separate activation blocker described in
[the network-isolation plan](../../../../docs/architecture/managed-network-isolation.md).
Mandatory private-FD authentication gating is covered by focused engine tests,
not by this standalone binary test. OAuth support needs its own host credential
refresh/writeback design and qualification; ordinary native conversations retain
their existing behavior.
