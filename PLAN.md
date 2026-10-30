# PLAN.md: Rudder Target Architecture

Status: plan of record, agreed with the user (2026-10-30). This document is the
architecture decision set and roadmap for Rudder Cloud scale-out. It supersedes
the file-based coordination model (RUDDER.md / DECISIONS.md / RUDDER_SHARED.md)
when the roadmap lands. Nothing here changes shipped behavior before that.

## 0. Goal and scale

Thousands of concurrent coding agents across a fleet of dedicated machines, at
near-zero marginal cost per agent, coordinated through one bus with one durable
store of what the fleet knows.

Measured workload profile (verified on the current home machine):

- Tracked trees are small: under ~400MB total across all 22 repos (largest: 123MB).
- Over 98% of disk is untracked deps, build output, and data.
- One agent workspace: 17MB tracked, ~1.6GB after a build.
- Claude transcript tree: 4.8GB (drives the dashboard launch spike).
- libra clone family: 6 clones x ~1.5GB node_modules, one dependency universe.

The monorepos in this workload are many packages with small tracked trees and
enormous untracked layers. The architecture is built around that fact: source
is cheap to materialize; deps and builds are the cost. Deduplicate the expensive
layers and never re-pay them per agent.

## 1. Settled decisions (the ledger)

| Decision | Call |
|---|---|
| Scale | thousands concurrent; fleet of dedicated btrfs machines (~10-40) |
| Ownership | Rust **fabric daemon** per machine owns workspaces, agents, caches, signals, audit; TUI/phone/web/CLI are socket views |
| Agent workspaces | overlay dirs, not jj workspaces; jj/git live only at the serialized integration seam |
| Layer split | source tree materialized per machine per commit (MBs); deps/build in input-keyed caches (lockfile hash, never commit) |
| Isolation | native primitives: mount namespaces + Landlock + cgroup v2 caps. gVisor parked as the untrusted-tier runtime, not the default |
| Machine filesystem | btrfs reflink sharing, overlayfs uppers, tmpfs ephemera, zram; EROFS only for repos above ~1GB tracked |
| Sparse checkout | off by default; auto-engages only above ~1GB tracked content |
| Caches | uv (Python), pnpm (JS), sccache + shared CARGO_HOME (Rust), GOMODCACHE (Go), multi-level sccache with S3 remote, Turborepo remote cache for JS monorepos |
| Determinism | injected per agent: SCCACHE_BASEDIRS at workspace root, CARGO_INCREMENTAL=0, UV_CACHE_DIR and pnpm store on the volume |
| Coordination transport | Claude channels (generalized /gam pattern): per-run MCP channel server, daemon is the bus, per-run nonce system prompt, sender allowlist = daemon only |
| Fleet truth | daemon SQLite coordination store; decisions are data |
| RUDDER.md | deleted (launch briefing moves to the spawn prompt; live status to channel injections; control markers to the daemon inbox; human projection to the actual UIs) |
| RUDDER_SHARED.md | deleted (secrets injected per agent as env vars from the vault/store; non-secret context goes to the decisions store) |
| DECISIONS.md as a file | deleted; decisions become immutable numbered entries in the coordination store (see 3.3) |
| AGENTS.md | kept: static, human-authored, version-controlled, read once at launch |
| Signals | lifecycle facts (done/input/error) via unix socket written by a tiny static `rudder-signal` binary; channels carry coordination content |
| State | SQLite in the daemon, evented diffs to the control plane; run.json demoted to export artifact |
| Conventions | committed lockfile required (hard gate); one canonical test command per repo (hard gate); ignore hygiene via audit; short negative-constraint standards |
| Backend scope | richest coordination is Claude Code (channels); codex/opencode ride the same bus via typed injection and CLI tool calls (degraded but symmetric) |

## 2. Architecture

### 2.1 The inversion

A Rust daemon owns the machine; every surface is a view. Today the TUI owns
state and shells out to the Node CLI per action (merge, launch, mirror,
telemetry), freezing the render loop and paying a ~150ms Node boot per call.
Inverted: the daemon owns, views connect over a socket (the shape `rudder
attach` already uses). This single inversion removes the UI freezes, the Node
boot tax, the 1.5s reparse state loop, and the one-machine ceiling.

### 2.2 Runtime split

| Component | Language | Why |
|---|---|---|
| Fabric daemon (per machine) | Rust, single static binary | hot path: overlays, fsnotify, sockets, spawns; no runtime boot, no GC |
| Control plane | stays Node + SQLite | request volume is low (phone/web/org); boring wins |
| CLI | thin client over the daemon socket | remaining Node boots stop mattering |
| Worker image | keeps running the real agent CLIs | Rudder drives agents, never replaces them |

### 2.3 Workspace fabric

- Agents are overlay directories: lower = per-machine materialized source tree
  at the task's base commit (shared); upper = per-agent delta. jj/git exist
  only at the integration seam.
- Source and deps are decoupled. Source: git-as-transport, materialized once
  per machine per commit (tracked trees are MBs). Deps/build: content-addressed
  stores keyed by lockfile hash, never by commit, so a merge advances the
  commit without invalidating a single cached byte.
- Delta capture: fsnotify write-audit into a per-agent SQLite delta store.
  This is ground truth for drift detection, completion verification, ignore
  hygiene (jj diff is blind to untracked writes, which are 98% of disk), and
  the merge gate. Untrusted writes (jq diff never sees them) are recorded as
  they happen.
- /swarm and /gam adopt the branch-context model: experiments are overlays off
  one shared base; losers are deleted delta dirs; abort costs nothing.

### 2.4 Cache layer (all defaults, zero config)

- uv for Python, pnpm for JS, sccache with shared CARGO_HOME for Rust,
  GOMODCACHE for Go. Per-agent installs hard-link from shared stores; per-agent
  dep cost is near zero.
- sccache multi-level (local disk to S3) and Turborepo remote cache on the
  existing S3 bucket. Cross-machine compile hits are configuration, not code.
- The daemon injects determinism env per agent so cache hit rates survive
  different workspace paths.

### 2.5 Isolation and machine filesystem

- Default: plain processes inside a mount namespace (workspace and allowed
  paths exist; nothing else does) + Landlock policy + cgroup v2 caps per
  agent. Forbidden paths are absent, not filtered.
- `--hide` semantics for swarm evals (gold answers invisible to agents) are a
  mount decision, not a prompt request.
- btrfs on fleet volumes, tmpfs for ephemera (signals, in-flight deltas),
  zram for memory headroom.
- gVisor: parked, named, not forgotten. It becomes the runtime of a future
  untrusted tier (org/team features, third-party agents) where read-level
  audit rides its gofer for free. Not the default for trusted agents.

## 3. Coordination

### 3.1 Surfaces: two files plus invisible machinery

| Surface | Verdict |
|---|---|
| AGENTS.md | keep: static repo engineering reference, read once at launch |
| DECISIONS.md (file) | delete; decisions become data (3.3) |
| RUDDER.md | delete; four jobs move (briefing to spawn prompt, live status to injections, markers to daemon inbox, projection to the UIs) |
| RUDDER_SHARED.md | delete; secrets per-agent env injection, non-secret context to the decisions store |
| Coordination store | the daemon's SQLite; not a file agents know about |

Net: from four files plus hope-based rereading to one static file plus
guaranteed delivery. This also deletes the biggest efficiency finding (the
RUDDER.md fan-out: ~2000 git spawns and ~10k file ops per transition at scale)
architecturally rather than patching it.

### 3.2 The bus

- Every Claude agent gets a per-run MCP channel server at creation
  (generalized from /gam, which is the proven existence proof: two Claudes
  coordinated over a channel, measured working in production).
- The daemon is the bus, always. Agents never hold credentials to each other.
  A sibling message routes: daemon inbox, relevance check, journal, sibling
  delivery. Rationale: agents distracting each other is a measured Anthropic
  failure mode (the daemon enforces rate/relevance etiquette); a sibling is a
  prompt injection vector (the daemon filters and journals); and journalling
  every relayed message creates the ledger automatically.
- Measured hardening carried over from /gam: Claude ignores mid-turn channel
  pushes until the system prompt names the channel plus a per-run nonce. Every
  spawn carries that.
- Channels are a research preview; the channel is a thin adapter at the edge of
  the daemon so protocol churn touches only the adapter. Fallback transport is
  typed injection with zero architecture loss.

### 3.3 Decisions as data

- A decision is an explicit typed act (`decision.record(topic, statement,
  scope, kind)` via channel tool call), never inferred from chatter.
- Entries are immutable and numbered. Change is a new entry with links:
  `supersedes: d17` or `amends: d17`, with a reason and scope delta.
- Current is a fold per topic, never a stored opinion. Topics are keys,
  fuzzy-matched at record time; the recording tool shows the current decision
  on that topic first, so blind contradiction becomes impossible.
- Decision lifetime: entries carry scope `fleet`, `plan:<id>`, or
  `experiment:<id>`. Folds filter per consumer, so experiment choices never
  leak into fleet truth and plan decisions expire with the plan.
- Contested detection: two current entries on one topic with overlapping scope
  route to the conductor/user. This is strictly better than the file world,
  where contradictory headings could coexist silently.
- Journal everything (worker-local decisions included); folds keep every view
  small.

### 3.4 Ratification by geometry

The audit layer gives every agent a touched-files set; the plan gives every
node declared scope. The daemon compares a decision's scope against live
agents' scopes to route it:

| Author | Scope | Becomes current |
|---|---|---|
| user steering message | plan/fleet | immediately, highest authority (conductor journals it) |
| conductor | cross-cutting | on record |
| worker | contained in own lane | on record, automatically |
| worker | intersects another live lane | auto-current, conductor notified, conductor can supersede (the conductor bargain: autonomous, visible, undoable; contested detection is the backstop) |
| interface producer | the interface topic | on record (dependents need it pre-merge); the merge gate verifies the landed code matches the recorded interface |

### 3.5 Change and the knowledge timeline

Every entry has `recorded_at`; every delivery has `delivered_at` and
`acked_at` per agent. The fold over (decision timeline x delivery timeline x
audit touched-files) answers "what did this agent know, when". The merge gate
flags deltas built against superseded truth touching that decision's scope and
auto-queues a reconciliation steer (visible, undoable; not a silent flag).

Supersessions pushed over the channel carry the diff, not just the new state,
so an agent whose context holds the old decision can adapt in place.

### 3.6 Message types (v1)

Structured only: question, interface-change, notify, ack. Free-form sibling
chat later, if earned. Codex/opencode record decisions via a `rudder decision`
CLI tool call; same store, same folds.

Swarm scoreboard: another entry type plus a board view (the leaderboard slot
that /swarm documented as not-built-yet).

## 4. Signals and status accuracy

- opencode gets a real working-signal (the gap in its plugin events).
- Codex: `notify` is documented legacy; migrate to `hooks.json` lifecycle
  events.
- Interrupt handling audited end to end (the no-hook-fires hole).
- Signals via unix socket (`rudder-signal`, a tiny static binary) instead of
  fork + printf + mv per hook event. On the current shared home machine, the
  interim is the signals dir on /dev/shm (7.9GB free, no privileges needed).

## 5. One agent's life, end to end

Task arrives, control plane places it (store warmth + subtree affinity from the
planner's named files). Daemon mounts the overlay upper, injects env (caches
wired) plus briefing (AGENTS.md reference + current relevant decisions slice).
Agent runs headless inside its mount namespace; writes are audited into the
SQLite delta store; coordination deltas arrive over its channel mid-turn; it
can ask siblings through the bus. Done-signal via socket. Delta queued at the
integrator. Serialized landing: apply at head, one batched gate run per landed
batch (turbo run test --affected / cargo test / uv run pytest), decisions
verified. jj merge. Source layer advances (MBs); caches untouched (lockfile
unchanged). Upper dir deleted.

Marginal cost: RAM-minutes plus a few MB of delta.

## 6. Roadmap (dependency-ordered)

| Month | Deliverable | Gate |
|---|---|---|
| 0 (parallel) | Efficiency fast-track on the current machine (section 7) | launch <2s; state loop <5ms idle; no 950MB spike; ~9GB node_modules deduped |
| 1 | Fabric daemon skeleton: signal socket, SQLite run state, write-audit to delta store, delta handback + serialized integrator seam, coordination store + cursors | full agent lifecycle via daemon; zero Node shell-outs in the hot path |
| 2 | Cache layer + machines: shared stores, S3 remote caches, fleet provisioning (btrfs, cgroup caps), placement basics | >80% compile hit rate; <50MB per agent |
| 3 | The bus: per-run channels generalized from /gam (nonce hardening, daemon relay, structured types), mount-ns + Landlock policy, --hide for swarm evals | mid-turn delivery verified; injection coverage 100%; no measurable overhead |
| 4 | Fleet scheduler (locality + subtree affinity), batched gates, lockfile/test-command gates enforced, ratification-by-geometry live | one gate run per landing batch; placement >90% local |
| 5 | Surfaces on the new spine: TUI/phone/web as views, decision timeline on the board, evented state; RUDDER.md / RUDDER_SHARED.md / DECISIONS.md deletion lands | no surface reparses files; gates block a lockless repo |
| 6 | /swarm and /gam on the branch-context model, scoreboard, load test | 1000 concurrent agents; flat per-machine disk; decision audit query <1s |

Component touch points: month 0 is src/board/daemon.ts, src/state.ts,
src/models.ts, src/version-check.ts, src/agent-skills.ts, cloud/worker/home.mjs,
native/src/signals.rs. Month 1 starts the daemon crate (fabric/) and
rudder-signal. Month 2 is the worker image (uv, pnpm, sccache, turbo) and
cloud/src/server.ts cache endpoints. Month 3 generalizes native/src/gam_link.rs
channel machinery. Month 5 retires the fan-out writers
(native/src/gitio.rs write_rudder_context, src/surfaces.ts renderLiveRudderMd,
src/run-manager.ts writeAgentContext).

## 7. Month 0 efficiency fast-track (current home machine)

Pays off immediately, independent of the fleet build:

1. Board snapshot storm: read only the output.txt tail (last 64KB), debounce
   per-run, hash-compare before rebroadcast (src/board/daemon.ts; today every
   backend stream line rebuilds all records and rereads up to 2MB per run).
2. run.json mtime caches: copy the fileCache pattern from cloud/worker/plan.mjs
   into readAgents (home.mjs) and the TS load paths. Today every run.json is
   fully parsed every 1.5s, plus again per 30s heartbeat.
3. Models pipeline: tail-read transcripts (the 4.8GB tree), drop the
   JSON.parse(JSON.stringify) deep clone, lazy network fetch (src/models.ts).
   This is the 950MB launch spike.
4. Memoize: worker_has_config (3 stats per drain for a value that cannot change
   mid-run), commandExists (sync sh fork), the npm update check (use the
   existing 6h cache), agent-skills byte-compares (mtime gate).
5. Signals dir on /dev/shm.
6. checkCredentials: replace the two live model calls every 30 minutes with
   auth-status probes (cloud/worker/home.mjs).
7. Vault rewrites: checksum-compare before writing every file (home.mjs,
   supervisor.mjs, cloud/src/secrets.ts).
8. EnsureClone: git fetch via spawnSync blocks the event loop; make it async.
9. pnpm/uv/sccache wiring on the current machine (dedup the libra family's
   ~9GB node_modules; works today on ext4 via hard links).
10. Buffered + rotated event appenders (events.ndjson is unbounded, one append
    per stream line).

## 8. Codebase conventions (opinionated defaults, enforced)

- Committed lockfile required (hard gate at the integrator; the entire
  input-keyed cache structure depends on it).
- One canonical test command per repo (gate runner requirement).
- uv, pnpm, sccache wired by env injection, not instructions.
- Ignore hygiene: the audit layer detects bulk un-ignored writes and suggests
  the line (jj snapshots non-ignored files into the shared store).
- Standards in agent context: short, negative-constraint shaped (the
  Guardrails paper result); existing linters as gates (biome for JS,
  rustfmt/clippy for Rust); no custom linter framework.
- Media and large binaries out of tracked files (S3 or LFS pointers).

## 9. Metrics (the improve loop owns these)

Workspace cost per agent; compile cache hit rate; coordination lag per agent
(cursor distance); contested-topic resolution time; gate runs per landing
batch; mid-turn supersession-to-ack latency; state loop cost; dashboard launch
time; audit query latency. Before/after for every change, so "is it better" is
data, not vibes.

## 10. Risks

1. Channels are a research preview; protocol may change. Mitigation: the
   channel is a thin adapter; fallback transport is typed injection.
2. Solo bandwidth: months 3 and 6 are the dense ones; month 2 (caches) is the
   deliberate low-risk middle where slip absorbs.
3. Serialized integrator as landing-rate bottleneck: batched gates are the
   designed answer, measured in month 4.
4. Mid-turn supersession handling: agents self-adapt (the message carries
   scope impact); the conductor intervenes only when a change invalidates a
   node's premise.

## 11. Parked tier (named, not forgotten)

- gVisor as the untrusted-tier runtime (orgs, third-party agents), with
  read-level audit via its gofer.
- Soft-dirty pagemap memory snapshots for agent resume without CRIU.
- EROFS base images for repos crossing ~1GB tracked.
- Eval-replay tier of `rudder improve`.

## 12. Reversible calls (revisit if evidence demands)

- Control plane stays Node + SQLite. If fleet echo costs show up at scale, it
  can fold into the fabric binary later.
- run.json demoted to export-only. A writer can return if an undo integration
  needs file-shaped state.

## Appendix: evidence base (measured this session)

- Fresh jj workspace: 17MB tracked; one built workspace: 1.6GB.
- 22 repos, <400MB tracked total, >98% of disk untracked.
- Claude transcripts: 4.8GB on the home machine.
- libra family: 6 clones x ~1.5GB node_modules.
- /dev/shm: 7.9GB free, zero privileges.
- Audit findings: per-tick stat/waitpid storms, save rereads, the RUDDER.md
  fan-out (~2000 git spawns per transition at 1000 workspaces), the 950MB
  launch spike cause, checkCredentials live model calls, vault unconditional
  rewrites, the 1.5s full-reparse state loop.

Literature grounding: instruction adherence attenuates within a session
(arXiv 2605.10039); context-file bloat costs ~20% for zero gain (2602.11988);
negative constraints outperform directives (2604.11088); retrieved relevant
context lifts decision compliance ~49% (2605.08112); compressed decisions and
artifact-plus-reference beat raw trace sharing (Cognition context-engineering
principles; Anthropic multi-agent research system).
