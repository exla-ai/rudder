# E14 fixed repository task comparison

`tasks.json` pins four repository tasks, acceptance criteria, affected checks,
source revision and three paired repeats per task. `compare.mjs` verifies its
byte-level SHA256 before accepting a sample. Changes to tasks require a new
experiment pin, never an in-place continuation of old samples.

Run the deterministic harness checks without model calls:

```sh
/workspace/rudder-build/rnpm exec -- node --test research/agent-efficiency/e14/compare.test.mjs
node research/agent-efficiency/e14/compare.mjs
```

The second command reports **pending**, with zero actual adapter samples. The
checked-in `pending.json` records that state. No genuine model/platform E14
samples were available for this implementation. Unit tests use constructed
records, including records exercising the actual-evidence validation branch;
those records are never saved or represented as measured model evidence.

## Collect private adapter evidence

Use an independent disposable checkout of the pinned source for each task and
repeat. Apply precisely the pinned prompt, preserve explicit user settings,
record both successful and failed attempts, run the task's affected checks, and
have the result reviewed against every acceptance criterion. Never use a
successful retry to replace a failed sample. Alternate which policy goes first
across repeats. Measure warm and cold cache states separately. Record the exact
model, backend build, hardware and platform; a model family alias is inadequate
if it can silently move. Keep raw logs, prompt/command text, checks and reviews
in a private store. Only supported numeric fields and artifact SHA256 references
enter the comparison input. Missing provider cost is `null`, never an invented
price or zero. Cached input is a subset of input, not added to it again.

The legacy policy is an **experiment-only** recreation of the earlier settings:
Codex has no configured compaction/output cap and its actual inherited effort
is recorded. Shipped Codex uses high effort, 150K compaction and 6K tool output.
Explicit choices apply to both arms and Codex max normalizes to xhigh. Claude
keeps its default effort and limits when unset in both arms. The wait-and-read
rule is absent/present in legacy/shipped respectively. These arms compare the
bundle; isolate a single knob in a separately pinned follow-up experiment.
There is no second production launch path and the harness never changes policy.

Supply a private JSON array to:

```sh
node research/agent-efficiency/e14/compare.mjs /private/e14/samples.json
```

Each sample has these fields:

| Fields | Contract |
| --- | --- |
| `schemaVersion`, `taskSetSha256`, `sourceRevision` | `1`, exported `TASK_SET_SHA256`, exact revision from tasks.json |
| `taskId`, `policy`, `repeat` | Pinned task id, `legacy` or `shipped`, positive integer |
| `backend`, `model`, `backendVersion`, `hardware`, `platform`, `cacheState` | `codex` or `claude`, immutable identities, `warm` or `cold` |
| `explicitUserSettings` | Object, including `{}` if no overrides; only keys `effort`, `compactionTokens`, `toolOutputTokens` |
| `launchSettings` | Effective `{effort, compactionTokens, toolOutputTokens, waitAndReadRule}`; unset settings are `null` |
| `evidenceKind`, `evidenceSha256` | `actual` or `fixture`, nonempty array of SHA256 references to private checks/reviews/traces |
| `acceptedChange`, `failedChecks`, `reviewRejections` | Boolean accepted outcome, counts including failed attempts; null if unknown |
| `inputTokens`, `cachedInputTokens`, `outputTokens`, `costUsd`, `wallMs` | Supported totals; null if unknown |
| `modelStepMs`, `contextTokens` | Per-request measurements; no estimates from reasoning text |
| `coordinationMs`, `waitMs` | Measured attribution, not a scheduling signal |
| `reads`, `postCompactionReads`, `repeatedReadsAfterCompaction` | Reads use same file revision plus range identity; repeated post-compaction reads count within the post-compaction denominator |
| `releaseAttempts`, `waitOnlyRequests`, `clockChecks`, `carriedReadTokens` | Counts and measured carried tokens; null if unsupported |

The collector must derive `acceptedChange` from actual acceptance/review, not a
model's final message, and obtain genuine check/review evidence. A hash proves
artifact identity, not that the asserted content is true. The harness cannot
authenticate privately supplied evidence and does not replace reviewer judgment.

## Interpretation

Groups share source/task pins, backend/model/version, hardware/platform,
cache state and explicit user settings. Only same-task/same-repeat pairs are
aggregated. Unpaired samples block a gate. `policies` contains the paired comparison;
`allPolicies` retains all samples and `unpairedPolicies` retains unpaired
outcomes, costs and evidence SHA256 references separately. Failed/rejected
samples stay in those totals. Missing fields are listed and totals remain null;
`observedTotals` preserves the known partial totals and their sample counts
without substituting for the complete totals. Each aggregate includes deduplicated
private artifact hashes so failed and unpaired evidence can be reviewed.
Reports contain aggregates and pin hashes, never arbitrary transcript fields.
Input files still require private storage; this tool does not persist raw logs.
Counters and token/context counts must be nonnegative safe integers, configured
token limits must be positive safe integers or null, and cached input cannot
exceed total input. Unknown explicit-setting keys and unsupported effort values
are rejected rather than accepted as ambiguous comparison pins.

Metrics include input plus output tokens and cost per accepted change, wall
clock per accepted change, model-step p50/p95, context mean/p50/p95,
coordination/wait share, post-compaction re-read rate, failed checks, rejected
reviews and release attempts. Zero accepted changes or zero post-compaction
reads cannot support the corresponding comparison. Quantiles use nearest rank.

A group can be measured only with all pinned tasks, at least three complete
paired repeats per task, actual evidence and all required supported fields.
Candidate support requires lower tokens, cost and wall time per accepted
change, no fewer accepted changes and no increase in failed checks, review
rejections or re-read rate. Otherwise it reports retune-or-revert, with review
still required before changing the single policy. Codex and Claude adapter
status is reported independently; the overall gate remains pending until both
have measured groups. Hardware/platform matrices require their own actual
samples. This descriptive paired comparison makes no causal speed claim,
confidence-interval claim, or hardware/release qualification claim.
