# PLAN 20 consumer wiring

Worker n10 owns the implementations below. Shared registrations, engine command
handlers, adapter event pumps, prompts and App consumers belong to n13. These
helpers are local advisory evidence and observations. They do not create a
scheduler, authorize work, publish completion or replace transactional receipts.

## Adapter telemetry

Import `src/agent-efficiency/index.ts`. At a completed step, call
`TelemetryRecorder.record` with the authenticated project/run identity and a
stable step id. Duplicate run/step delivery replaces its row. Combine
`supportedUsage(provider, event)` with measured tool/model durations, actual
output bytes, selected model/effort, retries and a failure fingerprint input.
These timing values must come from the adapter's real execution instrumentation.
Do not infer model latency from transcript prose or treat an unsupported field
as zero. Merge usage and tool measurements into one step before recording.

`supportedUsage` accepts Codex rollout `event_msg/token_count` with
`info.last_token_usage`, and Claude `assistant.message.usage`. It deliberately
ignores cumulative totals, unknown event shapes and context-window capacity.
Claude context occupancy is known only when all three input/cache components
are present. Codex cached input is a subset of input; do not add it twice.
Reasoning output is reported separately; this module makes no price estimate.
Until a managed app-server/SDK event has a supported usage mapping, its usage
is unknown, not a guessed translation from another protocol.

The recorder stores at most 512 steps and 256 KiB by default, with hard maximum
bounds of 2048 steps and 1 MiB. Actual pretty-printed file bytes are bounded.
Only allowlisted metrics, safe model/tool labels and hashed identities/failure
references enter JSON. Existing unknown/malformed rows are discarded. Shared
JSON locks prevent independent adapter writers from clobbering each other;
staged and final evidence files are mode 0600. Raw transcripts, prompts, command
text, error output and credentials remain in the private adapter store.

## Bounded subscriptions

Add `pub mod agent_efficiency;` to `engine/src/lib.rs`, and
`mod agent_efficiency;` to the native module roots used by App and library tests.
Both registrations are outside n10 ownership. Engine exports
`BoundedWaitRegistry`, `WaitScope`, `WaitKind`, `SettledOutcome`, `Subscription`,
`CancelWait` and `WaitResult`. The TS implementation exports the corresponding
event-driven Promise registry for TS callers.

Construct scope only after authenticating the current existing
`CapabilityIdentity` and checking access to the requested job/release. Project,
owner epoch, run, run epoch, kind and target id all participate in the key.
Replay an authoritative terminal snapshot before subscribing, then publish
committed lifecycle observations through `observe`. On recovery/cache eviction,
replay authoritative state; the registry itself is neither durable state nor a
command receipt. The first terminal observation wins; conflicting observations
require reconciliation. A subscription resolves once as settled, named
`job_wait_timeout`/`release_wait_timeout`, or `wait_cancelled`.

Call `admit` only to mirror actual admission. Call `exited` only after the
process owner observes actual process exit. Completion, timeout, subscription
cancellation and an interrupt request never release that admission. Capacity
limits retain live admissions. This helper makes no admission decision.

Native `AsyncWaitBridge::start` subscribes and waits on a background thread;
its callback only wakes the existing App event source. Drain `try_recv` and
apply typed results in the App loop. Both active waits and unread results hold
bounded permits. `WaitFinished::into_parts` releases its permit. Do not call
`Subscription::wait` in the App loop or mutate App/PTYS from its wake callback.
Cancel outstanding handles during normal shutdown; process exit remains a
separate lifecycle observation.

## Compaction and shared retrieval

After an observed compaction, call `compactionHandoff` with the current task,
constraints, open obligations and recent changes. Its JSON is bounded to 8 KiB
by default, redacted, marked `context_only`, and records omitted counts plus
`needsRetrieval`. Use its opaque references to request missing context from the
private task/evidence store. Do not declare an obligation satisfied merely
because a handoff included or truncated it. Both adapters use this same data
constructor through their existing prompt/handoff path.

At actual acceptance, construct `AcceptedTrace` from instrumented action
receipts and the existing integration/review receipt, then call `traceFindings`
and `ScopedLessonStore.learn` for each finding. At most 4096 action receipts are
reduced to three lesson kinds. Duplicate receipt delivery does not count as
repeated work. Exact input digests identify read content/range, job attempt/epoch
or declared check inputs. Failed checks, rejected reviews, unaccepted outcomes
and groups containing failed actions cannot teach a lesson.

Use one scoped lesson store for Claude and Codex. `retrieve` requires matching
project, repository path relevance, source revision and toolchain identity.
The scope should name the accepted source base consistently across consumers.
Expiry is at most seven days from the trace observation; a revision/toolchain
change requires recheck. `reject(id)` removes and suppresses the lesson.
Replaying the same evidence cannot renew a known expired lesson. The store
retains at most 128 lessons and 512 rejection/expiry tombstones; saturation
refuses further learning until an explicit owner reset. Retrieval returns at
most three lessons/2 KiB by default, hard limits five/4 KiB. Returned text is
fixed advisory copy with opaque provenance, never model-generated instruction
text. Lessons never grant user/system authority or relax required checks.

## Measured policy and remaining gates

The existing mirrored Codex launch policy is unchanged: high default effort,
150K compaction and 6K tool output, with explicit effort normalization and
planner capability limits. Claude retains its default absent an explicit choice.
There is no second production profile or automatic effort steering in this
change. The E14 harness under `e14/` evaluates actual paired outcomes before
retuning/reverting this one policy. Baseline launch settings exist only in
experiment evidence. `e14/pending.json` records zero actual model samples.
Actual Codex/Claude, hardware/platform and model effort experiments remain
pending. Unit fixtures make no speed or release qualification claim.

Targeted verification commands:

```sh
/workspace/rudder-build/rnpm run check
/workspace/rudder-build/rnpm exec -- node --test tests/agent-efficiency-v4.test.mjs
/workspace/rudder-build/rnpm exec -- node --test research/agent-efficiency/e14/compare.test.mjs
```

Rust modules include four engine tests and one native bridge test. They are
unrun while the shared module roots remain unregistered. After n13 registers
them, use the allowed narrow gates:

```sh
/workspace/rudder-build/rcargo test -p rudder-engine --lib agent_efficiency::waits::tests
/workspace/rudder-build/rcargo test -p rudder-native --lib agent_efficiency::tests
```

The native command requires inclusion in the native library test root. Merely
adding a module to the App binary does not make a library test run that code.
