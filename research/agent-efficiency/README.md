# Agent efficiency research

Goal: measure where coding agents spend tokens and time, then change Rudder
so they spend less on the same work. The v4 plan carries this as
[PLAN.md section 20](../../PLAN.md). Started on the local branch
`research/agent-efficiency`; the analyzer and findings now live on main.

Shipped in 4.0.0-alpha.17 as Rudder's only Codex launch policy, for every
Codex CLI launch: reasoning effort `high` unless one is chosen,
`model_auto_compact_token_limit=150000` and `tool_output_token_limit=6000`
(`CODEX_COMMON_CONFIG` in `native/src/launch.rs`, mirrored in
`src/codex-binary.ts`). The acpx adapter has no Codex config passthrough and
gets only the effort default. Verification checks status instead of building a
patch. These values shipped before E14 and are unproven; E14 decides whether
each one stays.

Planned parts:

1. **Trace analyzer** (this folder, first version). Reads agent session logs and
   ranks cost centers and waste.
2. **Experiments.** Each finding becomes one Rudder change, tried on comparable
   tasks and kept only if tokens or time drop without hurting results.
3. **Rudder memory.** One store shared by Codex and Claude agents, built from
   analyzer findings: short lessons tied to the trace that taught them, injected
   only when relevant.

## Run the analyzer

Node 22.18+ runs the TypeScript directly; there are no dependencies.

```sh
node research/agent-efficiency/trace/cli.ts <codex-session-id | rollout.jsonl> \
  [--md report.md] [--json report.json] [--root-only] [--sessions ~/.codex/sessions]
node --test research/agent-efficiency/trace/analyze.test.ts
```

It finds every rollout in the session tree (sub-agents share the root
`session_id`), so one id covers the main agent and all its helpers.

| File | Role |
|---|---|
| `trace/model.ts` | Backend-neutral trace records |
| `trace/codex.ts` | Codex rollout adapter (`~/.codex/sessions/**/rollout-*.jsonl`) |
| `trace/classify.ts` | Shell command classification and credential redaction |
| `trace/analyze.ts` | Token attribution and ranked findings |
| `trace/tools.ts` | Per-tool calls, latency and output; model vs tool time |
| `trace/report.ts` | Markdown report |
| `trace/analyze.test.ts` | Tests on a synthetic session tree |

## First result: Sol's v4 session (2026-10-30/31)

The full generated report contains redacted command text from the session and
stays on the local research branch; rerun the CLI to regenerate it. One main
agent and seven sub-agents, 22.5 hours, 10.8K model requests, 2.6B input tokens (99% cached).

What matters most:

1. **Context runs to the window before compacting.** The average request carries
   about 245K tokens, against an 18-31K fixed floor; compaction happens at about
   451K of a 475K window. Tokens above 150K per request add up to 1.2B, 44% of
   all input. Rudder set no `model_auto_compact_token_limit` for Codex before alpha.17.
   This is the largest single lever.
2. **Sub-agents are the main cost center, and they did the work.** Seven
   sub-agents used 63% of input tokens and wrote about 82K of the 95K edited
   lines. Each one pays the same large-context cost as the main agent.
3. **Coordination-only requests** (messaging, follow-ups, spawning): 2.2K
   requests, about 20% of input.
4. **Large command outputs** kept in context: 784 outputs over 2K tokens, about
   12%. The heaviest are start-of-task orientation dumps (AGENTS.md, RUDDER.md,
   plan and source excerpts at about 10K tokens each). Codex supports
   `tool_output_token_limit`; Rudder did not set it before alpha.17.
5. **Fixed prompt on every request**: 18-31K tokens, about 10%.
6. Smaller: build/test reruns (5.5%, `git diff --check` alone ran 422 times),
   waiting and polling (5.1%, 523 clock checks, 866 empty polls), the release
   and CI loop (1.9%), docs-only edits (1.8%), failed commands (1.7%).

Not the problem: exact re-reads of unchanged files are 0.7%. Hand-written docs
and evidence are about 11K of 95K edited lines. A plain `git diff` line count
suggested half the output was docs and evidence; most of that was generated
by scripts, not written by the agent.

## Tools: what agents call and what is slow

From the same session (all eight agents). The report's "Most-called tools"
and "Git commands" tables have the full numbers.

- **Wall clock is model time, not tool time.** The main agent was active for
  20.4 h; its tool calls blocked for 0.4 h (2%). Sub-agents: 1-11%. Faster
  tools barely change how long a task takes.
- **Model time per step is mostly reasoning output.** Every turn ran at
  reasoning effort `max`. A least-squares fit over the main agent's 2,838
  timed steps gives about 0.8 s + 1.6 s per 100K context tokens + 17 s per 1K
  output tokens. That attributes 74% of model time to output (median 557
  output tokens per step, 330 of them reasoning), 21% to context size and 4%
  fixed. Steps under 200 output tokens take a median 4.5 s; over 1K output
  tokens, 32 s. This is a correlation fit, not a controlled measurement.
- **Context still matters for time, and dominates cost.** Median step time
  rises from 6.0 s under 100K context to 20.5 s above 400K; the fit puts about
  a third of that rise on context itself and the rest on larger outputs later
  in a compaction cycle.
- **jj is not the bottleneck here.** Sol ran 78 jj commands out of 20,017
  tool uses (0.4%), about 2.6 minutes of tool time in 22 hours; it used git
  1,129 times. jj matters more for isolated workers, where Rudder snapshots
  each workspace on every jj call; that needs its own benchmark.
- **The most-called tools are reads, and they are already fast.** `sed -n`
  7.1K uses, `rg` 3.7K, `tail` 1.8K, inline Python 1.2K, `cat` 819, `git diff`
  672, `rustfmt` 473, `cargo test` 412, `df` 382, `git status` 242. Reads run
  in about 100 ms. Their cost is context: `sed -n` output alone is about 387M
  carried tokens (15% of all input), `rg` about 136M.
  `native/src/main.rs` was read 714 times in slices.
- **Slow tools are builds, tests and checks.** `cargo test` median 47 s (p95
  3.6 min, 183 min in total), qualification scripts in Python/Node around
  1 min each, `cargo check` median 59 s, `npm run check` 20 s, `node --test`
  11 s, `flyctl deploy` 2.1 min, `jj status` 8.9 s (one timed sample). 44 tool
  outputs show `Blocking waiting for file lock`: eight agents in one checkout
  share one Cargo target directory and wait on each other.
- **Git is fast; whole diffs are big.** Measured in Sol's checkout
  (1,546 tracked files): `git status` 13 ms, `git diff` 69 ms. But the full
  diff of Sol's uncommitted work is 483 KB, about 121K tokens. Sol avoided it:
  only 2 of its 672 `git diff` calls were whole-tree. It used `--check` (422),
  `--stat`/`--name-only` (171) and path-scoped diffs (77), averaging 100-600
  tokens.
- **Rudder's own verifier does the expensive version.** `verifyRun`
  (`src/brain.ts`) calls `workspaceDiff`, which runs jj status, `diff --stat`
  and the full `diff --git` patch, then only checks that the text is
  non-empty, and runs status again through `runHasChanges`. Each jj call
  snapshots the working copy. Status or `--name-only` answers the same
  question.

What would make agents faster or cheaper, in order of expected effect:

1. Spend reasoning where it matters: routine steps (polls, reads, running a
   known command) do not need effort `max`. Rudder should pick the effort per
   turn, which needs the Codex app-server adapter rather than the TUI.
2. Smaller context (see the compaction experiment below): faster model steps
   on every request.
3. Reads that return less: a symbol or outline read instead of 200-line
   `sed -n` slabs, output caps by default, and splitting very large files
   such as `native/src/main.rs`.
4. Builds and tests that do not wait on each other: a target directory per
   agent or a shared compiler cache, affected-only test runs, and the v4 Buck2
   action cache so unchanged actions are not rebuilt.
5. Rudder's verifier checks status instead of building full patches (done).
6. One blocking wait instead of `tail`/`df`/`ps`/clock polling loops.

## Experiments

Shipped as defaults; E14 measures them against the old launch settings:

| Change | Expected effect | Measure |
|---|---|---|
| Reasoning effort `high` (not `max`) by default | Shorter steps; output is 74% of model time | Wall clock per task, review/test failures |
| `model_auto_compact_token_limit=150000` | Average context from about 245K to under 100K | Input tokens per landed line; re-read rate after compaction |
| `tool_output_token_limit=6000` | Smaller carried outputs | Carried tokens; failures still diagnosed in one step |
| `verifyRun` checks status instead of the full patch | Fewer jj snapshots per verification pass | Verification wall time on a large diff |

Next:

| Change | Expected effect | Measure |
|---|---|---|
| `medium` effort for bounded sub-agent tasks; a short task brief after compaction | Cheaper helpers; less re-orientation | Sub-agent tokens per edited line; re-read rate |
| Test/build wrappers that print status plus failures | Smaller carried outputs | Carried tokens |
| One blocking `rudder wait` for jobs, CI and deploys | Fewer polling requests | Wait-only requests, clock checks |
| Trim the injected prompt and always-listed skills | Lower floor on every request | Floor tokens |
| Sub-agents start without parent history, with a scoped brief and a token budget | Cheaper helpers, fewer follow-ups | Sub-agent tokens per edited line; coordination requests |
| Full platform checks before tagging, behind one ship command | Fewer failed releases | Release attempts per published version |
| A Cargo target directory per agent, or a shared compiler cache | No waiting on another agent's build lock | `Blocking waiting for file lock` count; `cargo check` latency |

## Limits

- Codex encrypts reasoning and inter-agent messages; the analyzer reads only
  tool calls, outputs, token usage, prompts and progress updates.
- Commands are classified from their text, not their effect.
- Carried tokens assume an output stays in context until the next compaction.
  They are an estimate, not a billing record.
- Claude Code transcripts (`~/.claude/projects/**.jsonl`) need their own
  adapter into `trace/model.ts`; not written yet.
