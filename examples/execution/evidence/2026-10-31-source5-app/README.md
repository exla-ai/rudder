# App-owned source5 checkpoint evidence

Composite proof verified at **2026-10-31T16:54:26.760Z**. The original native
producer qualified, then a separate corrected reader verified its same immutable
publication after producer destruction. The original controller report remains
`qualified: false`; its first reader rejected an input-path assertion before
importing the client. That reader hardcoded alpha19 while the actual input and
control plane already pinned alpha20. Only the hardcoded expectation changed.

The actual App exercised source-specific ownership, BackendReady, cancellation
gating, an owner-bound Journal writer and App-owned PTY lifecycle. StopV4 produced
the original V2 PartialRemote checkpoint; its DATA record was fsynced before
retirement. The native proof records successful helper exit, fresh issuer join,
settled source callbacks, and actual off-App owner and supervisor joins. Checkpoint
transfer reconciled lost responses without changing the original Q or retention.

The corrected independent HTTPS reader used a fresh synthetic credential for the
same account and workspace. It checked producer absence twice, read all 14 reachable
objects (13,846 bytes), verified the original Q/attempt/observation/stop evidence and
exact mapped file (32 bytes) plus inherited `/old` (27 bytes), then removed its
credential and checked absence twice. The protected alpha7 home kept its instance
and image; the producer used the guarded alpha20 control plane.

| File | Evidence |
| --- | --- |
| `runtime-proof.json` | Original producer native JSON bytes, unchanged |
| `original-run.json` | Sanitized original controller false result, successful native child, failed first reader |
| `readback.json` | Separate successful retained-publication continuation and fresh absence checks |
| `cleanup.json` | Original producer and credential cleanup record, unchanged |
| `failures.json` | Four original failed attempts, with bounded diagnostics and hashes |
| `provenance.json` | Frozen binary/image identities and build17/test18 receipt summaries |
| `source-envelope.json`, `working-tree-source.patch` | Exact frozen working-tree source artifacts |
| `SHA256SUMS` | Hashes of all public files in this directory |

The earlier attempts remain failed: a pre-admission guard refusal, an actual App
lifecycle timeout, an invalid-JSON proof read whose backend outcome is unestablished,
and a valid diagnostic failure with `activated=false` and `backend_ready=false`.
The frozen source includes the subsequent activation handoff fix. Successful later
proof does not replace those original outcomes.

This is working-tree evidence over base revision
`cf6324cce9eb61f7a502c4722ea6b343a9d3c90a`, with exact source and binary hashes.
Build17 completed and the focused test18 run passed 10 tests with 1 ignored.
The freeze manifest embeds test18's receipt. Earlier baseline or interrupted broad
checks do not qualify this build. Raw provider output, credentials, native
authorization inputs, profiles, large pipes and binaries remain private. Sanitized
summaries retain the original private artifact hashes for review.

The backend is `offline-command-v1`; capture remains `live_best_effort`. These are
historical Source/Runnable and checkpoint DATA, without accepted jj history,
model execution, renewed ownership or grant authority. Explicit managed startup
and the legacy/default routing gate remain separate release requirements.
