# Agent trace report: 01a18b9c-e6b1-7b02-95c0-e610a0bedea5

8 agent session(s), 10.9K model requests, 2.7B input tokens (99% cached), 11.2M output tokens of which 6.8M reasoning.
Weekly window 2% to 100%; credits used since the window filled: 1.5K.

## Ranked findings

Shares are of all input tokens and overlap: one request can appear in several findings.

| # | Finding | Est. tokens | Share | Evidence | Fix to try |
|---|---|---|---|---|---|
| 1 | Sub-agent sessions (a cost center, not waste by itself) | 1.7B | 63% | 7 sub-agents used 1.7B input tokens (63% of the tree) and made 81.7K of 95.2K edited lines; /root/storage_review: 9.6 h, 303.5M tokens, 12.6K lines, first request 18.4K+; /root/fleet_authority: 9.6 h, 301.6M tokens, 13.5K lines, first request 18.5K+; /root/cloud_plan_review: 11.3 h, 289.7M tokens, 12.0K lines, first request 31.0K+; /root/new_sandbox_options: 11.2 h, 282.0M tokens, 14.6K lines, first request 31.3K+ | Each sub-agent pays the same large-context cost as the main agent; apply the same compaction limit and start them without the parent's history. |
| 2 | Context fills the window before compacting | 1.2B | 44% | average request carries 244.5K tokens; the fixed floor is only 18.4K-31.3K; median compaction happens at 451.0K tokens (55 compactions); tokens above 150.0K per request: 1.2B | Have Rudder launch agents with a lower auto-compaction limit (Codex model_auto_compact_token_limit, about 150.0K) and re-inject the task brief after compaction; check that re-reads do not rise. |
| 3 | Coordination-only requests | 536.3M | 20% | 2218 requests did nothing but message, spawn or follow up with agents; 2249 coordination calls in total | Brief sub-agents once with a scoped file list; have them return a structured result rather than chatting. |
| 4 | Large command outputs stay in context | 488.3M | 18% | 1178 outputs over 2.0K tokens, each resent on every later request until compaction; 10.0K tokens carried 2.3M: `pwd && git status --short \| head -45 && for f in AGENTS.md CLAUDE.md RUDDER.md DECISIONS.md RUDDER_SHARED.md; do if test`; 10.0K tokens carried 2.2M: `sed -n '1,160p' docs/architecture/implementation.md && sed -n '1,240p' native/src/tasks.rs && sed -n '1,170p' src/brain.`; 10.0K tokens carried 2.1M: `pwd; git status --short; rg --files -g 'AGENTS.md' -g 'CLAUDE.md' -g 'RUDDER.md' -g 'DECISIONS.md' -g 'RUDDER_SHARED.md'` | Cap shell output by default; have test, build and deploy wrappers print a status line plus failures only. |
| 5 | Fixed prompt resent on every request | 268.9M | 10% | each request starts from 18.4K-31.3K tokens of system prompt, tools, skills and instructions; 10.9K requests | Trim the injected Rudder prompt, AGENTS.md and the always-listed skills and tools; load the rest on demand. |
| 6 | Repeated build and test suites | 144.6M | 5.4% | 499 test requests, 103 build requests; 136x `git diff --check`; 58x `node /workspace/home/.<redacted`; 45x `python3 /workspace/home/.<redacted` | Cache verification results by input hash so unchanged suites are skipped, and run only affected tests between milestones. |
| 7 | Requests that only wait or poll | 135.8M | 5.1% | 581 requests did nothing but wait; 873 empty polls of running commands; 523 clock checks; 2117 log checks (tail / log reads) | Give agents one blocking wait (for a job, CI run or deploy) that returns a single result instead of polling. |
| 8 | Failed commands | 85.6M | 3.2% | 368 commands exited non-zero; 5x failed: `python3 /workspace/home/.<redacted>.py`; 4x failed: `node /workspace/home/.<redacted>.mjs > /workspace/home/.<redacted>.log 2>&1`; 3x failed: `rustfmt --edition 2021 engine/src/vfs/rpc.rs && cargo check -p rudder-engine > /workspace/home/.cache/rudder-vfs-rpc-che` | Record known failure fixes in Rudder memory so the next attempt starts from the fix. |
| 9 | Release and CI loop | 51.3M | 1.9% | 26 release commands (npm version, tags, pushes of tags); 188 requests spent checking CI or the registry | Run the full platform matrix before tagging, behind one ship command that tags only when every gate is green. |
| 10 | Requests that only edit docs or evidence | 47.9M | 1.8% | 190 requests whose edits touched only docs or evidence files; docs/evidence lines added: 11.2K vs code/tests: 84.0K | Generate delivery evidence mechanically from CI and deploy results instead of having the agent write it. |
| 11 | Re-reading unchanged file ranges | 20.9M | 0.8% | 606 exact re-reads with no edit or compaction in between; 13422 repeat reads of an already-read file overall; native/src/main.rs: 715 reads, 13 exact repeats; engine/src/store.rs: 355 reads, 9 exact repeats; engine/src/vfs.rs: 313 reads, 8 exact repeats | Keep a per-session read cache in Rudder and tell the agent when a range is unchanged since it last read it. |

## Agents

| Agent | Model | Hours | Requests | Input | Floor | Avg context | Peak | Compactions | Commands | Files edited | Lines added |
|---|---|---|---|---|---|---|---|---|---|---|---|
| /root | gpt-6.1-sol | 22.6 | 3.8K | 970.3M | 24.9K | 256.2K | 452.7K | 16 | 3.0K | 186 | 13.6K |
| /root/cloud_plan_review | gpt-6.1-sol | 11.3 | 1.3K | 289.7M | 31.0K | 225.1K | 451.6K | 6 | 1.0K | 78 | 12.0K |
| /root/fly_options | gpt-6.1-sol | 11.2 | 1.0K | 250.5M | 31.3K | 246.8K | 453.6K | 6 | 801 | 67 | 13.0K |
| /root/new_sandbox_options | gpt-6.1-sol | 11.2 | 1.1K | 282.0M | 31.3K | 251.5K | 452.1K | 6 | 897 | 87 | 14.6K |
| /root/cloud_plan_review/content_review | gpt-6.1-sol | 0.0 | 2 | 549.3K | 269.4K | 274.6K | 279.9K | 1 | 87 | 6 | 1.1K |
| /root/pager_handoff | gpt-6.1-sol | 9.6 | 1.1K | 255.6M | 18.6K | 224.8K | 457.6K | 6 | 924 | 69 | 14.8K |
| /root/fleet_authority | gpt-6.1-sol | 9.6 | 1.2K | 301.6M | 18.5K | 244.0K | 461.3K | 8 | 1.1K | 100 | 13.5K |
| /root/storage_review | gpt-6.1-sol | 9.6 | 1.3K | 303.5M | 18.4K | 239.8K | 452.6K | 6 | 1.1K | 69 | 12.6K |

## Where input tokens go

Each request is labeled by the most consequential thing it did (edit > release > test > build > cloud > ci > vcs > coordinate > web > read > wait). A request with no tool call is a message. Time is wall clock until the agent's next response in the same turn, summed across agents running in parallel.

| Request kind | Requests | Input tokens | Share | Commands | Output tokens | Carried | Time |
|---|---|---|---|---|---|---|---|
| read | 2.8K | 628.6M | 24% | 5.4K | 5.9M | 651.8M | 15.2 h |
| edit | 2.2K | 559.6M | 21% | 943 | 131.8K | 11.0M | 15.8 h |
| coordinate | 2.2K | 536.3M | 20% | 0 | 0 | 0 | 12.2 h |
| message | 1.7K | 491.9M | 19% | 0 | 0 | 0 | 10.3 h |
| wait | 581 | 135.8M | 5.1% | 753 | 229.0K | 25.5M | 3.9 h |
| test | 499 | 120.4M | 4.5% | 1.1K | 252.2K | 25.0M | 2.9 h |
| web | 268 | 60.1M | 2.3% | 0 | 0 | 0 | 1.5 h |
| ci | 188 | 46.6M | 1.8% | 33 | 52.3K | 4.3M | 0.8 h |
| build | 103 | 24.2M | 0.9% | 492 | 87.0K | 9.4M | 0.8 h |
| cloud | 94 | 22.1M | 0.8% | 123 | 92.5K | 10.8M | 0.6 h |
| other | 102 | 16.8M | 0.6% | 80 | 79.0K | 8.1M | 0.9 h |
| vcs | 25 | 6.6M | 0.2% | 42 | 20.8K | 3.1M | 0.1 h |
| release | 17 | 4.7M | 0.2% | 22 | 2.1K | 227.9K | 0.1 h |

Carried output estimate covers 28% of all input tokens; the rest is the system prompt, instructions, messages and reasoning.

## Heaviest outputs (by tokens carried in context)

| Agent | Output tokens | Carried | Command |
|---|---|---|---|
| /root | 10.0K | 2.3M | `pwd && git status --short \| head -45 && for f in AGENTS.md CLAUDE.md RUDDER.md DECISIONS.md RUDDER_SHARED.md; do if test` |
| /root | 10.0K | 2.2M | `sed -n '1,160p' docs/architecture/implementation.md && sed -n '1,240p' native/src/tasks.rs && sed -n '1,170p' src/brain.` |
| /root | 10.0K | 2.1M | `pwd; git status --short; rg --files -g 'AGENTS.md' -g 'CLAUDE.md' -g 'RUDDER.md' -g 'DECISIONS.md' -g 'RUDDER_SHARED.md'` |
| /root | 7.1K | 1.9M | `git status --short && git log -3 --oneline` |
| /root | 7.0K | 1.9M | `git diff -- engine/src/service.rs native/src/fabric_cli.rs && sed -n '1,230p' engine/src/partial_checkpoint/recovery.rs ` |
| /root/new_sandbox_options | 10.0K | 1.7M | `<dynamic>` |
| /root | 7.3K | 1.7M | `sed -n '1,95p' RUDDER.md && sed -n '1,210p' .github/workflows/release.yml && tail -65 DECISIONS.md` |
| /root | 7.2K | 1.6M | `sed -n '1,240p' cloud/src/fabric-cas.ts && sed -n '1,240p' cloud/src/fabric-content.ts` |
| /root | 7.2K | 1.6M | `cat <tmp> && sed -n '235,375p' cloud/src/fabric-cas.ts && sed -n '1,210p' tests/fabric-cas.test.mjs` |
| /root | 8.6K | 1.6M | `rg -n 'fabric_\|engine_\|Background\|Receiver<\|Sender<\|channel\(' native/src/main.rs \| head -100; sed -n '1,260p' native/sr` |
| /root | 5.8K | 1.5M | `sed -n '1,240p' scripts/native-artifacts.mjs; cat scripts/build-native.mjs; cat .github/workflows/release.yml` |
| /root/fleet_authority | 10.0K | 1.4M | `<dynamic>` |
| /root/fleet_authority | 10.0K | 1.4M | `<dynamic>` |
| /root/fly_options | 10.0K | 1.4M | `sed -n '115,205p' cloud/worker/tailscale-previews.mjs && tailscale --socket <tmp> serve --help && tailscale --socket <tm` |
| /root | 6.0K | 1.4M | `python3 - <<'PY' from pathlib import Path s=Path('docs/engineering-reference.md').read_text();a=s.index('## 9.');b=s.ind` |

## Most-read files

| File | Reads | Exact unchanged re-reads |
|---|---|---|
| native/src/main.rs | 715 | 13 |
| engine/src/store.rs | 355 | 9 |
| engine/src/vfs.rs | 313 | 8 |
| engine/src/execution/linux.rs | 266 | 2 |
| native/src/app_tests.rs | 200 | 2 |
| native/src/fabric_runtime.rs | 196 | 2 |
| AGENTS.md | 191 | 8 |
| engine/src/execution.rs | 190 | 1 |
| native/src/pty_terminal.rs | 189 | 2 |
| engine/src/journal.rs | 186 | 3 |
| native/src/engine_host.rs | 186 | 8 |
| src/main.ts | 167 | 4 |

## Most-run build and test commands

| Kind | Runs | Failures | Command |
|---|---|---|---|
| test | 136 | 8 | `git diff --check` |
| test | 58 | 6 | `node /workspace/home/.<redacted` |
| test | 45 | 7 | `python3 /workspace/home/.<redacted` |
| test | 33 | 6 | `node --experimental-vm-modules /workspace/home/.<redacted` |
| build | 31 | 1 | `npm run check` |
| build | 25 | 0 | `node --check /workspace/home/.<redacted` |
| build | 23 | 0 | `cargo check -p rudder-engine` |
| build | 21 | 1 | `npm --prefix cloud run build` |
| test | 11 | 0 | `sha256sum -c SHA256SUMS` |
| build | 11 | 0 | `rustfmt --edition 2021 engine/tests/partial_checkpoint.rs` |
| build | 10 | 0 | `cargo build -p rudder-native` |
| test | 9 | 1 | `npm test` |

## Release commands

- 10-30 08:55 /root: `npm version 4.0.0-alpha.1`
- 10-30 09:08 /root: `npm version 4.0.0-alpha.2`
- 10-30 09:36 /root: `npm version 4.0.0-alpha.3`
- 10-30 13:45 /root: `npm version prerelease --preid=alpha`
- 10-30 13:45 /root: `git push --atomic origin main v4.0.0-alpha.4`
- 10-30 14:17 /root: `npm version prerelease --preid=alpha --no-git-tag-version --ignore-scripts`
- 10-30 14:17 /root: `git tag -a v4.0.0-alpha.5 -m '4.0.0-alpha.5'`
- 10-30 14:17 /root: `git push --atomic origin main v4.0.0-alpha.5`
- 10-30 14:27 /root: `npm version prerelease --preid=alpha --no-git-tag-version --ignore-scripts`
- 10-30 14:27 /root: `git tag -a v4.0.0-alpha.6 -m '4.0.0-alpha.6'`
- 10-30 14:27 /root: `git push --atomic origin main v4.0.0-alpha.6`
- 10-30 15:48 /root: `npm version prerelease --preid=alpha --no-git-tag-version --ignore-scripts`
- 10-30 20:41 /root: `git tag --list v4.0.0-alpha.8`
- 10-30 20:42 /root: `npm version prerelease --preid=alpha`
- 10-30 20:43 /root: `npm version prerelease --preid=alpha`
- 10-30 20:44 /root: `git push origin main v4.0.0-alpha.9`
- 10-30 22:33 /root: `npm version 4.0.0-alpha.10`
- 10-30 23:14 /root: `git tag --list 'v4.0.0-alpha.*'`
- 10-30 23:52 /root: `npm version 4.0.0-alpha.11`
- 10-31 00:55 /root: `npm version 4.0.0-alpha.12`
- 10-31 00:55 /root: `git push --atomic origin main v4.0.0-alpha.12`
- 10-31 03:02 /root: `npm version 4.0.0-alpha.13`
- 10-31 03:02 /root: `git push --atomic origin main v4.0.0-alpha.13`
- 10-31 04:34 /root: `npm version 4.0.0-alpha.14`
- 10-31 04:34 /root: `git push --atomic origin main v4.0.0-alpha.14`
- 10-31 02:22 /root/fleet_authority: `git tag --list 'v4.0.0-alpha.*'`

## Edits by area

| Area | File edits | Lines added | Lines removed |
|---|---|---|---|
| code | 1546 | 59.1K | 4.6K |
| tests | 533 | 24.9K | 796 |
| docs | 240 | 9.2K | 680 |
| evidence | 41 | 2.0K | 38 |

Requests whose edits touched only docs or evidence: 190 (47.9M input tokens).

## Method and limits

- Reads the clear-text parts of the rollout: tool calls, outputs, token usage, prompts and updates. Reasoning and inter-agent messages are encrypted and not read.
- Commands are classified from their text, not their effect. Edits made by scripts are detected only from obvious write calls.
- Carried tokens assume an output stays in context until the next compaction; this is an estimate, not a billing record.
- Token counts are raw. Cached input is far cheaper per token than uncached input, but both count toward usage.

## Time: model vs tools

Wall clock inside turns, per agent. Tool time is how long a request's tool calls blocked before returning (a background command returns early and is polled).

| Agent | Active | Model | Tools | Tool share |
|---|---|---|---|---|
| /root | 20.4 h | 20.0 h | 0.4 h | 2% |
| /root/cloud_plan_review | 8.5 h | 7.5 h | 1.0 h | 11% |
| /root/fly_options | 8.1 h | 7.9 h | 0.1 h | 2% |
| /root/new_sandbox_options | 8.3 h | 8.2 h | 0.1 h | 1% |
| /root/cloud_plan_review/content_review | 0.0 h | 0.0 h | 0.0 h | 0% |
| /root/pager_handoff | 6.4 h | 6.1 h | 0.3 h | 4% |
| /root/fleet_authority | 6.7 h | 6.3 h | 0.4 h | 6% |
| /root/storage_review | 6.5 h | 6.3 h | 0.2 h | 3% |

Model time between tool calls: median 12.1 s, p95 1.1 min. Commands per request: 1.0.

Model time per step by the context that step carried:

| Context | Steps | Median | p95 |
|---|---|---|---|
| 0-100K | 1.2K | 6.0 s | 26.2 s |
| 100-200K | 2.6K | 10.0 s | 45.4 s |
| 200-300K | 2.3K | 13.9 s | 1.1 min |
| 300-400K | 2.0K | 16.9 s | 1.4 min |
| 400K+ | 999 | 20.5 s | 1.8 min |

## Most-called tools

Latency percentiles use commands that ran the tool alone; background commands are timed start to finish. Output tokens are split evenly across the steps of a compound command.

| Tool | Uses | Solo | p50 | p95 | Max | Main-step time | Output tokens | p95 output | Carried | Polls | Failures |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `sed -n` | 7.1K | 50 | 100 ms | 1.3 s | 1.6 min | 11.3 min | 3.4M | 1.5K | 387.2M | 6 | 46 |
| `rg` | 3.7K | 56 | 100 ms | 4.0 s | 1.2 min | 3.7 min | 1.3M | 1.2K | 136.1M | 2 | 81 |
| `tail` | 1.8K | 160 | 100 ms | 5.2 s | 5.3 s | 7.2 min | 411.2K | 767 | 45.7M | 0 | 7 |
| `python3 inline` | 1.2K | 404 | 300 ms | 1.3 min | 10.8 min | 182.9 min | 321.3K | 1.1K | 25.9M | 151 | 58 |
| `cat` | 819 | 16 | 100 ms | 1.7 s | 1.7 s | 12.6 min | 291.8K | 1.5K | 37.0M | 22 | 12 |
| `git diff` | 672 | 11 | 200 ms | 5.1 s | 5.1 s | 8.9 min | 125.6K | 860 | 14.5M | 10 | 15 |
| `rustfmt` | 473 | 66 | 200 ms | 400 ms | 1.0 s | 15.6 min | 21.7K | 247 | 2.0M | 12 | 12 |
| `cargo test` | 412 | 86 | 46.9 s | 3.6 min | 8.5 min | 183.0 min | 21.1K | 247 | 1.8M | 156 | 29 |
| `df` | 382 | 2 | 2.7 s | 2.7 s | 2.7 s | 3.8 s | 46.9K | 407 | 4.8M | 0 | 0 |
| `git status` | 242 | 1 | 100 ms | 100 ms | 100 ms | 18.5 s | 87.9K | 2.0K | 11.0M | 0 | 4 |
| `node script` | 206 | 61 | 44.2 s | 2.6 min | 9.2 min | 65.7 min | 18.3K | 281 | 1.7M | 71 | 19 |
| `nl` | 204 | 3 | 100 ms | 200 ms | 200 ms | 2.7 s | 178.6K | 3.8K | 16.4M | 0 | 0 |
| `ls` | 182 | 2 | 100 ms | 100 ms | 100 ms | 6.6 s | 37.2K | 745 | 4.1M | 0 | 9 |
| `node inline` | 170 | 62 | 400 ms | 21.3 s | 51.4 s | 13.3 min | 40.0K | 859 | 3.9M | 20 | 13 |
| `node --test` | 154 | 33 | 10.6 s | 1.5 min | 2.4 min | 24.9 min | 38.6K | 1.3K | 3.6M | 39 | 8 |
| `python3 script` | 149 | 62 | 1.1 min | 9.0 min | 13.4 min | 141.7 min | 18.7K | 485 | 2.2M | 84 | 16 |
| `node --check` | 147 | 5 | 200 ms | 200 ms | 200 ms | 1.6 min | 5.5K | 196 | 305.5K | 1 | 2 |
| `date` | 126 | 0 | 0 ms | 0 ms | 0 ms | 2.7 s | 23.2K | 703 | 1.8M | 0 | 0 |
| `ps` | 124 | 2 | 1.4 s | 1.4 s | 1.4 s | 2.2 s | 25.6K | 658 | 3.0M | 0 | 1 |
| `cargo check` | 112 | 14 | 58.9 s | 2.4 min | 2.4 min | 15.7 min | 17.3K | 362 | 1.6M | 21 | 1 |
| `sha256sum` | 104 | 4 | 100 ms | 200 ms | 200 ms | 1.6 s | 15.7K | 627 | 967.9K | 0 | 0 |
| `-v` | 94 | 0 | 0 ms | 0 ms | 0 ms | 0 ms | 12.9K | 1.1K | 967.3K | 0 | 0 |
| `head` | 76 | 0 | 0 ms | 0 ms | 0 ms | 1.1 s | 22.1K | 857 | 2.6M | 0 | 0 |
| `wc` | 63 | 1 | 100 ms | 100 ms | 100 ms | 1.3 s | 10.3K | 770 | 943.0K | 0 | 0 |
| `flyctl machine` | 63 | 22 | 900 ms | 1.1 s | 5.4 s | 32.1 s | 13.7K | 911 | 2.1M | 1 | 3 |
| `du` | 59 | 0 | 0 ms | 0 ms | 0 ms | 58.6 s | 8.5K | 577 | 1.2M | 1 | 0 |
| `test` | 55 | 0 | 0 ms | 0 ms | 0 ms | 1.4 s | 11.6K | 1.0K | 1.3M | 0 | 4 |
| `cargo build` | 54 | 7 | 1.5 min | 2.4 min | 2.4 min | 9.4 min | 5.6K | 44 | 520.8K | 9 | 1 |
| `npm cloud` | 49 | 10 | 1.5 min | 2.4 min | 2.4 min | 18.2 min | 2.3K | 217 | 112.4K | 12 | 1 |
| `git log` | 45 | 0 | 0 ms | 0 ms | 0 ms | 100 ms | 7.1K | 566 | 1.3M | 0 | 0 |
| `npx tsc` | 43 | 3 | 16.1 s | 23.2 s | 23.2 s | 2.1 min | 4.2K | 561 | 385.3K | 5 | 0 |
| `npm run check` | 39 | 17 | 19.9 s | 2.4 min | 2.4 min | 12.6 min | 477 | 57 | 56.9K | 20 | 1 |
| `flyctl deploy` | 39 | 16 | 2.1 min | 11.1 min | 11.1 min | 66.0 min | 11.4K | 3.8K | 1.3M | 22 | 0 |
| `sh` | 34 | 0 | 0 ms | 0 ms | 0 ms | 32.2 s | 890 | 137 | 34.1K | 1 | 0 |
| `chmod` | 33 | 2 | 100 ms | 100 ms | 100 ms | 36.1 s | 1.0K | 281 | 75.9K | 1 | 0 |
| `stat` | 30 | 1 | 100 ms | 100 ms | 100 ms | 300 ms | 5.4K | 565 | 559.7K | 0 | 1 |
| `git rev-parse` | 26 | 0 | 0 ms | 0 ms | 0 ms | 100 ms | 3.3K | 471 | 254.8K | 0 | 0 |
| `pwd` | 24 | 0 | 0 ms | 0 ms | 0 ms | 300 ms | 10.1K | 2.5K | 1.9M | 0 | 0 |
| `jj status` | 23 | 1 | 8.9 s | 8.9 s | 8.9 s | 9.0 s | 5.3K | 1.6K | 831.9K | 1 | 0 |
| `git push` | 21 | 8 | 13.9 s | 56.0 s | 56.0 s | 2.7 min | 1.3K | 165 | 169.3K | 8 | 0 |

## Git commands

| Variant | Uses | p50 | p95 | Avg output | p95 output | Carried |
|---|---|---|---|---|---|---|
| `git diff --check` | 422 | 200 ms | 5.1 s | 104 | 479 | 4.4M |
| `git status` | 242 | 100 ms | 100 ms | 363 | 2.0K | 11.0M |
| `git diff (summary: --stat/--name-only)` | 171 | 100 ms | 100 ms | 215 | 756 | 4.6M |
| `git diff -- <paths>` | 77 | 700 ms | 700 ms | 585 | 2.6K | 5.4M |
| `git log` | 45 | 0 ms | 0 ms | 157 | 566 | 1.3M |
| `git show` | 17 | 0 ms | 0 ms | 201 | 1.1K | 394.6K |
| `git diff (whole tree)` | 2 | 0 ms | 0 ms | 0 | 0 | 0 |
