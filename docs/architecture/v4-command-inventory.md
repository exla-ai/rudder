# v4 command inventory and migration map

Source baseline: `017d76ed`, inspected 2026-11-03 for PLAN 16 / E11.
Registry/adapter alignment: 2026-11-04, current n8 implementation. This is a
source inventory, not telemetry or proof that a caller has no users. The n8
registry change and tests cover the owned command boundary. n13 owns shared
surface integration; n11 owns automatic task routing and planning. E11 remains
open until those callers and the generated-file consumers below migrate.

## Canonical surface

The shipped registry is `src/operations/registry.json` (format version 1). Seven human families: `rudder` / `open`, `run`, `agent`, `change`, `project`,
`auth`, `doctor`. `engine` and existing callbacks are machine interfaces, not
additional advertised human families. Help and completion must derive from the
operation registry under `src/operations/`; native consumers load the same
registry through `native/src/command_surface/`. Do not create a second inventory
of operation semantics in web, Mac, prompts or skills.

Ordinary implementation input retains one isolated jj workspace. Shared checkout
execution requires an explicit checkout selection. Execution location (local or
Cloud), checkout choice, project, target run/group and conversation continue,
copy or exclusive move are independent arguments, never inferred from an alias.
Native discussion and plan-only input opens a guarded Main conversation with no
implementation workers; decomposition remains approval gated. Canonical CLI
`run` and ordinary task text (including after `--`) now explicitly set isolated
checkout policy. The
backend compatibility aliases retain the old checkout policy during their finite
window, so alias normalization does not silently move existing edits.

The CLI adapter deterministically classifies new tasks and safely refuses
non-execute discussion/decomposition until n11's native task-owner route is
integrated. It does not launch implementation as a fallback. Explicit main
checkout also refuses and directs the user to its native owner. Cloud `run`
selects the task-only sail adapter to avoid interpreting a one-word task as a
project attach. Explicit Cloud backend/model selections refuse because this
adapter cannot preserve them. These refusals preserve intent/scope; they are not
proof of complete canonical journey support. CLI/Cloud/native parity remains an
n11/n13 integration gate.
A project composer starts work. An agent composer sends only to that agent.
A group send requires a visible explicit target set. Send/steer never implies
resume, rescheduling, broadcast or acknowledgment by a model. Stop, resume,
review, integration and settings changes are deterministic operations. Preserve
original engine receipts and epochs; surface dispatch is not a new authority.

## Compatibility policy

All compatibility spellings in the tables have the finite window
**2026-11-03 through 2027-02-01 exclusive (UTC)**, owned by **n13 / command-surface
integration** unless a narrower owner is named. Rejection after expiry must name
the canonical replacement and must never fall through to task creation. Aliases
are absent from generated help/completion. Migration must preserve stdout JSON,
exit status, noninteractive behavior and every original flag. Warnings belong on
stderr. Preserve callback contracts until the separately listed caller migration
has passed; callback retention is not an unbounded human alias.

The date is an alias refusal deadline, not permission to delete session history,
user files or an unmigrated callback. By the deadline the named owner must migrate
or explicitly version a machine interface. `/ask` and native `/run` were already
retired before this work. `/gam` and `/nudge` already return retirement notices;
retain explicit refusal so their syntax cannot become ordinary task text.

## CLI dispatch inventory

Evidence: `src/main.ts::main`, `parseArgs`, `printHelp`, `src/index.ts`. Every
baseline switch label and early raw-argv handler is accounted for below.
“Merge” means replace the public spelling and migrate callers, not delete the
underlying capability. Canonical family actions listed here identify migration
targets; a target needs its execution adapter before its old handler is removed.

| Baseline spelling | Actual behavior / live evidence | Disposition and canonical replacement | Owner / window |
|---|---|---|---|
| no arguments | TTY opens native dashboard, or attaches a second dashboard view when its socket is live; non-TTY prints help | Keep; `open` is explicit equivalent | n8/n13; canonical |
| unrecognized ordinary text | Joins command and args as task, directly calls `startRun` | Keep ordinary text; use n11 routing boundary and preserve isolation/location | n11/n13; canonical |
| `run` | Calls `startRun` with backend/model/workspace/queue/json flags | Keep one task start; explicit scope, n11 routing | n11/n13; canonical |
| `claude`, `codex` | Checks backend executable, calls `startRun` with pinned backend | Merge into `run --backend claude|codex` | n13; 2027-02-01 |
| `acpx` | Checks executable, strips optional leading `codex`, starts acpx run | Merge into `run --backend acpx`; preserve optional `codex` argument conversion | n13; 2027-02-01 |
| `attach` | Native attach to selected live pane | Merge into `agent attach <id>` (or `open <id>`); preserve mirror selection | n13; 2027-02-01 |
| `dashboard` | Opens native dashboard | Merge into `open` | n13; 2027-02-01 |
| `restart` | Resets local session, then opens dashboard | Merge into `project restart`; retain explicit destructive/reset intent | n13; 2027-02-01 |
| `mouse-test` | Native raw/parsed mouse diagnostics | Merge into `doctor mouse` | n13; 2027-02-01 |
| `tmux`, `tui`, `shell`, `interactive`, `legacy-shell` | Refuses removed tmux/legacy shell surface | Keep explicit refusal; never treat as new task | n13; no active alias |
| `doctor` | Runs environment check with optional JSON | Keep diagnostics | n13; canonical |
| `onboard` | Setup wizard with noninteractive/JSON options | Merge into `doctor setup` | n13; 2027-02-01 |
| `login`, `auth` | Both baseline spellings invoke Cloud login; baseline `auth` is not a subcommand family | Canonical `auth login`; finite `login` alias preserves supplied provider args; bare `auth` keeps its login default during the window | n13; 2027-02-01 for login and bare auth |
| `cloud`, `sail` | Delegates all args to `runCloudCommand`; semantics differ for bare cloud vs sail | Merge into `project cloud` compatibility administration; task entry becomes `run` with explicit Cloud location | n11/n13; 2027-02-01 |
| `watch` | `watchRun` with follow=true | Merge into `agent watch [run]` | n13; 2027-02-01 |
| `logs` | Saved output with optional follow | Merge into `agent logs [run]` | n13; 2027-02-01 |
| `status` | Active repo runs, optional JSON | Merge into `agent status` | n13; 2027-02-01 |
| `runs` | Lists repo runs, optional JSON | Merge into `agent list` | n13; 2027-02-01 |
| `report` | Filters/aggregates runs with format/model/date/status/limit/sort options | Merge into `agent report`; retain report format contract | n13; 2027-02-01 |
| `stop` | Calls `stopRun` for required id | Merge into `agent stop <id>`; no classifier | n13; 2027-02-01 |
| `delete` | Deletes run/workspace, force flag | Merge into `agent delete <id>`; retain force protection | n13; 2027-02-01 |
| `handoff` | Queues THIS external conversation for dashboard adoption; `--here` chooses shared checkout; `--list` lists candidates | Merge into `agent continue`; explicit continue/copy/move must retain source identity and target checkout | n13; 2027-02-01 |
| `merge` | Merges specified run into current change | Merge into `change integrate <id>`; retain allow-dirty contract | n13; 2027-02-01 |
| `sync` | Rebases a run's jj change onto its base without merging | Merge into `change sync [id]`; this is NOT current `/sync` | n13; 2027-02-01 |
| `cleanup` | Removes merged run workspaces | Merge into `change cleanup` | n13; 2027-02-01 |
| `gc` | Bounded diagnostics/binary/run-record pruning, supports dry-run | Merge into `project gc` | n13; 2027-02-01 |
| `undo` | Rewinds jj operation, or pops undo stack | Merge into `change undo [opId]`; retain global effect warning/scope | n13; 2027-02-01 |
| `board`, `serve` | Starts board server; board opens browser by default, serve does not | Merge into `project board` / `project serve`; preserve browser-open default | n13; 2027-02-01 |
| `plan` | Legacy CLI planner creates DAG/empty jj changes, not native interactive route | Merge into ordinary `run` routing; explicit plan-only intent must not imply worker approval | n11/n13; 2027-02-01 |
| `fanout` | Legacy N variants and winner judge, backend and N flags | Merge into typed experiment start with explicit budget/winner policy; do not silently convert to ordinary execution | n11/n13; 2027-02-01 |
| `improve` | Improvement run/status/report/launchd schedule, can ship | Move to `doctor improve` administration; preserve delivery authorization and budget | n13; 2027-02-01 |
| `telemetry` | status/on/off; persistent usage preference | Merge into `project settings` (current CLI adapter is telemetry status/on/off) | n13; 2027-02-01 |
| `feedback` | Sends bounded diagnostic report | Merge into `doctor feedback` | n13; 2027-02-01 |
| `remember` | Appends durable decision to tracked DECISIONS.md | Retain machine callback until journal decisions and readers migrate; future scoped context tool | n13; callback gate below |
| `share` | Stores ignored RUDDER_SHARED.md context; may contain secrets | Retain machine callback until private scoped context/credential replacement; never echo data in discovery | n13; callback gate below |
| `context` | Context audit helper | Retain callback; human view moves to `doctor context audit` | n13; callback gate below |
| `done` | Completion JSON sidecar and human decision entry; delivery evidence separate | Retain exact machine callback and `--node`/stdin JSON; no human alias deadline | n13; callback gate below |
| `engine` | Persistent engine status/start/shutdown/API; upload/download bypass generic parser | Keep versioned machine interface, deterministic authority checks | n13/engine owner; machine contract |
| `version`, `--version`, `-v` | Prints package version; may report update to stderr | Keep informational contract | n13; canonical |
| `--help`, `-h` | Prints help without setup | Keep; derive text/completion from registry | n8/n13; canonical |
| `help` | Listed informational in setup predicates, but baseline switch has no handler and falls through as a task | Fix as informational registry dispatch; never start a task for help | n8/n13; canonical |
| unknown `__*` | Inert for version-skew safety | Keep refusal/inert contract; never route to a model | n13; machine contract |

## Integration APIs and wiring

`src/operations/registry.ts` exports `COMMAND_REGISTRY`, `SURFACE_VERSION`,
`OperationDefinition`, `operationDefinition`, `assertAliasCurrent`, `commandHelp`,
`commandCompletions`, `ComposerClassification` and `classifyComposerInput`.
The JSON manifest holds canonical CLI paths, the five advertised slash entries,
finite aliases, callback/retirement lists and per-operation capability, scope,
arguments, receipt kind and existing adapter/tool name. Discovery is metadata,
not delegation of an engine capability.

`src/operations/cli.ts` exports `applyCommandSurface` and
`printCommandDiscovery`. Minimal `src/main.ts` wiring imports those plus
`commandHelp`, normalizes `parseArgs` with the original raw argv (including the
`--` literal delimiter), and handles discovery before update/setup/jj work.
`printHelp` delegates to `commandHelp`. Existing handler output and exit status
are retained; no generic success wrapper changes legacy JSON. `help --json`
returns the manifest; `completion [family] --json` returns canonical strings.
Families/actions without a safe CLI adapter refuse instead of starting a task.
Bare `auth` retains its login default until 2027-02-01 exclusive; after that,
use `auth login`. The old `--worktree` flag has the same finite deadline and
canonical replacement `--workspace`; machine callbacks retain their original
parser contracts. `--` still protects literal task text from scope flags.

Implemented adapter spellings are:

* `open` opens dashboard; `open <id>` or `agent attach <id>` attaches a pane.
  `project restart` retains explicit session-reset behavior.
* `agent list/status/logs/watch/report/stop/delete` and `agent continue` reuse
  their existing handlers. Typed send/ask/reply/inbox/ack/await/steer/interrupt/
  resume/checkpoint/find/receipt/topics, copy and move retain selected-surface
  or authenticated-tool requirements rather than guessing targets from CLI text.
* `change integrate/sync/cleanup/undo` reuse merge, run-rebase, cleanup and undo.
  `change review/check` need selected-surface/authenticated engine dispatch.
  `change sync` remains distinct from `project cloud sync`.
* `project cloud/board/serve/gc` retain existing administrative/view handlers.
  `project settings [status|on|off]` currently adapts the telemetry CLI only.
  The typed `project.settings.v1` schema supports model/effort/speed/
  notifications/sound/color/telemetry; those additional settings still need UI
  and authenticated execution adapters. Do not advertise CLI parity for them.
* `auth login`, `doctor`, and `doctor setup/mouse/feedback/context/improve`
  reuse their existing handlers and flags. `doctor context audit --json` is the
  human context-audit spelling; bare `context` remains a machine callback.

`src/operations/requests.ts` exports `ExecutionScope`, `OperationArguments`,
`OperationId`, `OperationRequest`, `validateOperationRequest`, `operationDigest`,
`OperationReceipt`, `validateOperationReceipt`, `OperationAuthority`,
`TransactionalOperationHandler`, `dispatchOperation` and `agentToolCalls`.
Requests bind format/version, operation, idempotency key, project, location,
checkout, owner epoch, selected run ids/epochs, selection, optional Cloud home
and delegated communication scope. Arguments are bounded and checked for unknown
fields. Receipt validation binds operation/key/request SHA256/kind and preserves
`accepted`, `running`, `completed`, `rejected`, `effect_unknown` separately.
Accepted is not backend completion or message adoption; `effect_unknown`
requires reconciliation, not replay or a success claim.

These typed surface envelopes are adapters. They do not replace authenticated
Root/engine wire schemas, mint authority or implement a second scheduler.
`dispatchOperation` requires the delegated capability and exact authenticated
execution scope, then delegates the transaction to the existing engine handler
and validates its receipt against the original request. That handler still owns
durable idempotency, current epoch checks, transactional receipts and uncertain
effect reconciliation. `agentToolCalls` maps addressed operations to existing
agent tools, including separate per-target group keys; ordinary send never maps
to broadcast recovery or resume.

`native/src/command_surface/mod.rs` exports `REGISTRY_JSON`, `Family`,
`Operation`, `Alias`, `Registry`, `registry`, `operation`, `completions`,
`ComposerInput`, `classify_composer`, `AgentScope`, `Location`, `Checkout`,
`Selection`, `ExecutionScope`, `SurfaceRequest` (`from_json`, `validate`, `digest`) and
`SurfaceReceipt` (`validate`). It embeds the same JSON and validates the same
surface contract. `utc_date` comes from the trusted owner. Backend composers
return `Backend` before interpreting any Rudder slash spelling and must forward
the original text intact. Refused project commands must never become tasks.

Current registration is only a child module in `native/src/engine_operations.rs`,
which exercises Linux library tests; it does not wire the dashboard or Mac UI.
n13 must register at the native root for those consumers:

```rust
#[path="command_surface/mod.rs"]
mod command_surface;
```

Then migrate `models.rs` palette/searchable help to registry discovery and
`main.rs` composer/keyboard dispatch to `classify_composer` plus existing typed
App actions. Preserve the App loop as sole state/PTY owner. Remove retired
`/tell` paired dispatch and picker consumers, expired alias suggestions, legacy
help strings, and generated prompt/skill callers only after their replacements
are integrated. Keeping the old handlers alongside unused new metadata does
not pass E11. n11 planning contracts remain authoritative; n13 connects task
start to that owner without rewriting n11 files from this assignment.

## Required machine callbacks

These are real callers, not speculative unused aliases. Preserve their existing
stdin/stdout framing, exit codes, bounds and identity checks. Delete only after
migrating the named caller and testing old/new-version skew. Callback names need
not appear in human completion.

| Callback / raw handler | Caller and contract | Disposition / owner |
|---|---|---|
| `__fabric-fetch` | SDK filesystem content fetch, raw argv ahead of generic parser | Keep fixed machine path; n13/data-plane owners |
| `__claude-sdk --launch-file` | `native/src/agent_link.rs`, `managed_backend_prepare.rs`; SDK owns argv/stdio | Keep exact bridge; n13/backend owner |
| `engine api upload|download` | `src/fabric-remote.ts`, raw transfer dispatch | Keep exact byte transport; n13/data-plane owners |
| `__worker --repo --run [--attempt]` | `src/run-manager.ts::spawnWorker`; owns detached lifecycle | Keep identity-bound callback; n13 |
| `__route-task` | `native/src/task_routing.rs` background helper; bounded JSON stdin/result | Keep n11 typed classifier contract; n11/n13 |
| `__launch-node` | `native/src/gitio.rs`; TS owns jj workspace creation, parses JSON path | Keep; n13/workspace owner |
| `__graph-mirror` | `native/src/main.rs` background plan projection; graph JSON stdin | Keep until durable planner projection replaces all consumers; n11/n13 |
| `__sync-checkout` | `cloud/worker/home.mjs`; sync report JSON and explicit bundle env | Keep until both home and laptop migrate; n13 |
| `__pull-export` | `cloud/worker/home.mjs`; repo/run/output archive + JSON | Keep; copy/export must not stop remote agent; n13 |
| `__adopt-moved` | `cloud/worker/home.mjs`; stages moved archive, explicit ownership handover | Keep; exclusive move semantics distinct from copy; n13 |
| `__models` | `cloud/worker/home.mjs`; phone model picker uses current/claude/codex JSON | Keep exact result schema; n7/n13 |
| `__refresh-models` | `native/src/models.rs`; detached stale-cache refresh | Keep; discovery must not block App loop; n7/n13 |
| `__event` | `native/src/telemetry.rs`; fire-and-forget bounded telemetry, quiet failure | Keep inert on unknown versions; n10/n13 |
| `done`, `remember`, `share`, `context` | Worker prompts (`native/src/tasks.rs`, `src/brain.ts`), surfaces and board | Keep callbacks until journal/context migration and restart/offline proof; n10/n11/n13 |

The engineering reference's `__agents`, `__task`, `__worker-idle` descriptions
are stale: they are absent from the baseline switch. Preserve unknown-internal
safety, not dormant tmux scheduling machinery.

## Native slash dispatch, suggestions and keybindings

Evidence: `native/src/main.rs::handle_command`, `start_task_from_input`,
`models.rs::command_suggestions`, `searchable_command_suggestions`,
`dashboard_help`, `render.rs`. Baseline bare `/` offers exactly five entries:
`/model`, `/main`, `/resume`, `/cloud`, `/help`. The new registry advertises
`/help`, `/model`, `/login`, `/usage`, `/feedback`; n13 must replace the old palette
and searchable completion, not leave two sets of canonical entries. Searchable suggestions still list
many hidden operational spellings. Hiding a command from the initial five does
not retire its dispatcher or fuzzy completion.

| Baseline slash | Actual action | Migration / owner / expiry |
|---|---|---|
| `/model` | Model/provider/effort defaults; opens picker | Canonical slash/settings action; n7/n13, registry-derived picker |
| `/fast` | Backend-native speed/effort defaults | Merge into model settings; n7/n13, 2027-02-01 |
| `/main`, `/m` | Starts another agent in shared checkout, explicit path | Explicit task checkout control; preserve typed compatibility through 2027-02-01, n11/n13 |
| `/plan` | Hidden interactive planner alias, guarded discuss/plan-only route | Ordinary task router plus explicit intent, n11/n13; 2027-02-01 |
| `/swarm` | Proposes competing experiments, approval launches, no automatic merge | Typed experiment operation and budget/winner policy; n11/n13, 2027-02-01 |
| `/tell` | Still calls `tell_gam_reviewer` for paired reviewer | Registry now explicitly refuses this retired spelling; n13 must remove paired dispatch/picker callers and use addressed messaging, preserving old sessions |
| `/gam` | Retirement notice; old mode/session structures remain | Keep explicit refusal; no new removal credit, n13 |
| `/nudge` | Retirement notice | Keep explicit refusal; remove special broadcast/recovery meaning, n13 |
| `/restore` | Opens existing backend CLI session/pane | Merge into continue-conversation action with explicit target checkout; n13, 2027-02-01 |
| `/resume` | Session picker/adoption, isolated by default, `--here` shared checkout | Finite alias to `agent.continue.v1`; explicit continue/copy/move; n13, 2027-02-01 |
| `/handoff` | With id: adopts chat; bare: moves selected pane to Cloud | Require explicit move/continue choice; bare move must not become ordinary send; n13, 2027-02-01 |
| `/share` | Private shared context capture | Scoped context/credential action; preserve callback until readers migrate; n10/n13 |
| `/feedback` | Diagnostic report | Canonical slash and `doctor feedback`; n13 |
| `/telemetry` | Usage setting | Project settings; n10/n13, 2027-02-01 |
| `/help` | Dashboard help notice, no work | Canonical registry help; n8/n13 |
| `/login` | Cloud login pane | Canonical slash and `auth login`; n13 |
| `/cloud` | Location/home/Cloud command pane | Finite alias to explicit execution/location control; n13, 2027-02-01 |
| `/usage` | Session usage notice; web/open/dashboard opens usage board | Canonical slash usage view; n10/n13 |
| `/goal` | Explicitly forwards `/goal` to focused backend PTY | Preserve backend forwarding; no Rudder task rewrite; n13 |
| `/merge-all` | Requests ready-work integration | Change integrate with explicit ready set and authorization; n13, 2027-02-01 |
| `/review-all` | Reviews all ready work | Change review with explicit selected ready set; n13, 2027-02-01 |
| `/verify` | Resets final gate and starts when all planned nodes integrated | Change check; preserve gate prerequisites; n13, 2027-02-01 |
| `/sync [all|move]` | Cloud/config/default-branch checkout sync; move explicitly hands live agents home | Project sync, explicit all-project scope/move; n13, 2027-02-01. Never map to `change sync` |
| `/notify`, `/sound`, `/color` | Persistent notification/sound/theme preferences | Typed `project.settings.v1` controls; CLI currently adapts telemetry only; n13, 2027-02-01 |
| `/web`, `/ui`, `/board` | Opens web UI | `project board` view; n13, 2027-02-01 |
| `/diff` | Toggles diff panel | Change diff view; n13, 2027-02-01 |
| `/ask`, native `/run` | Already rejected unknown syntax | Keep refusal; not newly removed by v4 |
| unknown single slash token | `start_task_from_input` rejects typo-like slash command | Preserve refusal; paths containing another slash remain ordinary paths |
| Worker composer backend slash | Worker-pane key events go to backend PTY | Preserve exact backend input; unknown backend commands are backend's concern |
| `$ command` | Real interactive shell pane in this checkout; bypasses shared-context capture | Keep explicit shell execution path, preserve encoding and checkout |

Keys are operation callers too: Agents-pane `Enter` focuses/starts/resumes the
selected row, `x` stops, `v` reviews, `m` integrates, `R` reviews ready work,
`M` integrates ready work, `d` deletes, `g` toggles DAG, `U` opens usage, `H`
hands a selected local agent home. Option-1/2/3/4 and Ctrl-W change pane focus.
The old `u` workspace-sync key was already removed. n13 must map deterministic
keys to canonical operations and preserve focus, targeted scope, deliberate
resume, PTY attach sizing, terminal key encoding, Option characters and scrollback.

## Cloud, Mac, scripts, prompts and generated coordination

| Consumer / evidence location | Current dependency | Exact migration requirement / owner |
|---|---|---|
| `src/cloud.ts::runCloudCommand` | Bare cloud opens home; bare sail starts legacy sail; named cloud token may open project; remaining text launches task | `project cloud` temporarily owns administration. n11/n13 must route `run` Cloud tasks through the same task-intent/scope contract, without changing named-project opens into tasks |
| Cloud subcommands in `src/cloud.ts` | login, home, preview, doctor, sync, app/pair, launch, sail, byoc, vm/byo-vm, list/ls, status, logs, attach, talk/say/msg, output/tail, quickstart, slack, workspace, onload, move, pull, bootstrap, pause, resume, stop, delete/rm, setup-github/google/byoc/vm/fly, setup, runtime, region, secrets | Keep administrative capability behind project/auth/doctor adapters. Map message aliases to addressed send, list/status/output to agent views, onload/move/pull to explicit conversation move/copy. Alias owner n13, deadline 2027-02-01; do not remove nested handler before corresponding service caller migration |
| `cloud/worker/home.mjs` | Spawns __sync-checkout, __pull-export, __adopt-moved, __models; receives home task ingress | Preserve machine callbacks; n11/n13 migrate only task ingress through router, retain upload/export identity and copy/move distinction |
| `cloud/worker/supervisor.mjs`, `task-launch.mjs` | Resolver classifies once; legacy execute launches `[backend, "--worktree", task]`, rejects discuss/plan; workspace launches bare rudder. Supervisor header still mentions older JSON argv | Replace with `["run", "--backend", backend, "--workspace", task]`; preserve fail-closed routing and one resolution per startup, n11/n13 |
| `mac/App/LocalAgents.swift::pull` | Executes `["cloud", "pull", runId, "--json"]`, bounded 180s, account-scoped environment | Replace with `["project", "cloud", "pull", runId, "--json"]` only after canonical adapter exists; preserve PullResult JSON/status. Copy leaves remote agent running; n13/Mac owner |
| `mac/Core/RudderCore.swift::LocalTools` | Parses pull result/error; dashboard shell script executes bare rudder; version probe uses --version | Keep schema/error status and bare dashboard/version contracts; add canonical argv test; n13/Mac owner |
| `cloud/app/mac-local.js`, `onboarding.js`, `app.js`, `cloud/src/server.ts` | Human instructions say `rudder login`, `rudder cloud`, `rudder cloud app`; Mac bridge uses pull | Substitute `auth login`, `project cloud`, `project cloud app`. Do not change web auth URLs/deep-link schemes; n13/web owner |
| `src/run-manager.ts` | User hints emit watch/stop/merge/sync commands | Substitute `agent watch`, `agent stop`, `change integrate`, `change sync`; preserve same run ids and flags; n13 |
| `src/auth.ts`, `src/util.ts` | Missing-tool hints say onboard/doctor | Substitute `doctor setup`; retain doctor; n13 |
| `native/src/models.rs` | Hard-coded five palette entries plus large searchable list and help | Derive labels/help/completion from command registry. Remove expired aliases from fuzzy suggestions as well as initial palette; n13 |
| `native/src/main.rs` | Direct slash dispatcher, keyboard/control markers and backend forwarding | Register new command_surface module and invoke classifier before legacy slash dispatch. Dispatch canonical operation to existing deterministic App action; run backend pass-through only in its own composer; n13 |
| `native/src/render.rs` | Special `/model` picker rendering | Consume registry-selected model/settings action rather than guessing new prefixes; n7/n13 |
| `native/src/launch.rs::ORCHESTRATOR_SKILLS` | Generates edit-dag/model/main/goal/usage/cloud/monitor/review-merge/plan-ask/worker-control/help skills writing RUDDER markers | Replace marker spellings with typed authenticated operations only when engine adapters exist; keep old installed session control functional until migrated; n11/n13 |
| `native/src/tasks.rs`, `src/brain.ts` | Prompts require done, ownership reads, RUDDER_PLAN_TASKS, APPROVE/ADD_TASK/RUN/MAIN/REPLAN/INJECT/STOP/RESUME/REGOAL/MERGE/CLOUD_MIGRATE and file-based control results | n11/n13 migrate to scoped discovery, planning revision/approval, addressed control and receipt inspection. Keep `rudder done --node` exact callback meanwhile |
| `native/src/main.rs::RUDDER_NUDGE_ALL`, `nudge_all_workers` | Hidden marker still sends to live workers only; failures never resume. Its help still mentions nudge-all | Replace with explicit group-send target list; never restore old recovery behavior. Remove marker/help only after installed skill/session consumers migrate; n13 |
| `src/agent-skills.ts`, `assets/skills/` | Installs provider/domain capability skills globally; unrelated user skills preserved | Do not blanket-delete or rewrite; these are not obsolete orchestration handlers. Only migrate actual Rudder command callers; n13 |
| `scripts/guide-loop.mjs` | Writes `.rudder/steer/*.json`, reads receipts and live runs directly | Migrate through addressed steer and receipt API, preserve bounded waiting/ownership; n10/n13 |
| `src/improve/`, package launchd schedule | Improvement launcher can ship and invoke lifecycle functions | Keep explicit budgets/shipping authorization; migrate human entry only, no automatic deployment in ordinary task dispatch; n13 |
| `scripts/build-native.mjs`, `copy-native.mjs`, `postinstall.mjs`, release scripts | Artifact and installation workflow, not ordinary command dispatch | Keep build/install gates; do not run or mutate for surface work; n13/release owner |
| `tests/task-routing.test.mjs`, `tests/tui/task-routing.test.mjs`, `native/src/app_tests.rs` | Native/TS route fixtures, five-entry palette, retired syntax and deterministic controls | n11/n13 run updated canonical journey coverage with real binary after integration. Owned `tests/v4-command-surface.test.mjs` covers registry/compatibility boundary only |

No standalone shell-completion generator was found in baseline `src/`, `scripts/`
or native sources. The native fuzzy palette is the actual completion consumer;
adding a CLI completion endpoint must derive from the same registry and exclude
callbacks/expired aliases without changing machine discovery.

## Generated-file retirement gates

Do not claim fleet-file removal after shrinking help. The baseline still has:

* `src/surfaces.ts::renderRudderMd` and fan-out to root/every workspace;
  `src/daemon.ts` calls projections at startup and ticks. `src/run-manager.ts`
  and `src/rudder-md.ts` also write/lock the coordination file.
* `src/surfaces.ts` writes DECISIONS.md and worker completion entries.
  `src/main.ts::runRemember` and completion reporting call those writers.
* `native/src/tasks.rs` and generated `launch.rs` skills instruct models to read
  and write RUDDER control markers. `native/src/main.rs` consumes them and reports
  consumed/success/failure separately. Marker disappearance is not success.
* `src/board/daemon.ts` watches and parses DECISIONS.md for its memory view;
  `src/brain.ts`, `src/context-audit.ts`, `src/state.ts`, `src/jj.ts` and
  `src/cloud.ts` reference/copy/filter these files for context, recovery or export.

n10/n11/n13 must first replace each reader with journal/context projections and
typed tools, migrate active legacy sessions, and prove restart/offline/catch-up
behavior. Then remove generated fan-out writers, marker parsers, installed
obsolete orchestration skills and stale tests together. Preserve user-authored
AGENTS.md, private ignored RUDDER_SHARED.md, source/history exports, historical
OneOff/GAM sessions and existing completion sidecars. No cleanup is authorized by
an expired human alias alone.

## E11 acceptance evidence required from integration

Run the same core journeys through canonical CLI, native controls, home/web and
Mac where applicable: task start with visible local/Cloud + checkout scope;
select/send/steer/interrupt; explicit resume; continue/copy/exclusive move;
review/check/integrate; usage/settings; auth/diagnostics. Verify plan-only cannot
launch workers and message cannot broadcast or restart stopped work. Preserve
backend slash forwarding, unknown Rudder-command refusal and unknown-internal
inertness. Compare exact JSON output and exit codes for migrated automation.
Check help and completion budgets, deadline refusal, absence of retired syntax
from suggestions and prompt/skill callers, and generated-file migration gates.

E11 is not closed. Registry/adapter tests do not establish cross-surface real
lifecycle behavior; n11/n13 task-owner integration, native UI/Mac wiring and the
generated-file migration gates above remain blockers. This document does not
claim cross-platform journeys, removed generated writers, backend model
qualification, stable release or deployment proof.

## Owned verification, 2026-11-04

The manifest contains 45 operations, 32 command aliases, one legacy flag alias
and five advertised slash entries. TS and Rust consume 61 shared request vectors
(47 valid including every operation; 14 refusals), with matching canonical SHA256
for valid requests. `npm run check` passed through the required wrapper; final
`rnpm exec -- tsc -p tsconfig.json` compilation also passed. The focused
`rnpm exec -- node --test tests/v4-command-surface.test.mjs` suite passed 18/18
with no skips. Real CLI subprocesses cover JSON help/completion, exact legacy vs
canonical status/list stdout, missing-target exit/error parity, version/help
precedence, task-route callback JSON, inert version-skew callbacks and explicit
unsupported-action refusal. These are local boundary checks, not a model run.

`rcargo test -p rudder-native --lib command_surface` passed 3/3 after the shared
build queue. It covers the same request/digest vectors, completion budgets,
backend namespace, alias cutoff and original-scope receipt checks. Existing
unrelated Rust warnings remain. Local captured output is at
`/workspace/rudder-build/n8-command-surface-node.log` and
`/workspace/rudder-build/n8-command-surface-cargo.log`. The coordinator must run
the integrated native binary, ordinary model journeys, platform and E11 gates;
these tests do not claim those gates, delivery, deployment or generated-file
retirement.
