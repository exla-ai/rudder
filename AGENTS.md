# Rudder engineering rules

Rudder runs Claude Code and Codex agents locally on macOS and on Fly.io Linux.
The CLI is TypeScript, the dashboard is Rust, and `cloud/` holds the control
plane and workers. v4 adds a persistent engine, immutable workspaces, shared
caches and durable agent communication.

Keep this file short: agents load it automatically. Detailed implementation
notes live in [the engineering reference](docs/engineering-reference.md).
Read the sections relevant to the files you change; do not load the entire
reference, plan or task history for a small change.

## Ownership

- Check generated `RUDDER.md`, when present, for live owners and workspace paths.
  Do not hand-edit its generated block. Preserve unrelated user changes.
- Main agents edit the user's checkout. Isolated workers edit their own jj
  workspace and let Rudder snapshot/integrate it. Do not write into a sibling's
  workspace or manage its version control.
- Batch independent reads/commands. Delegate substantial independent work when
  helpful; give each agent separate files and integrate its result yourself.
- Follow applicable repo instructions once. Recheck coordination when ownership
  overlaps, a relevant decision changes, or work is about to integrate. Do not
  reread every context file before every tool call.
- Credentials belong in private profiles/stores. Shared local setup can use
  ignored `RUDDER_SHARED.md` when needed; never put secrets in tracked docs,
  generated `RUDDER.md` or completion output.

## Code map

| Change | Start here | More detail |
|---|---|---|
| CLI, lifecycle, config, auth | `src/main.ts`, `run-manager.ts`, `state.ts` | reference §§4–6, 8 |
| Native UI, PTYs and input | `native/src/main.rs`, `render.rs`, `pty_terminal.rs` | reference §7 |
| Engine, storage and peer tools | `engine/src/`, native `engine_host.rs`, `fabric_runtime.rs`, `agent_mcp.rs` | `PLAN.md` §§4–6, 10–11, 14 |
| Cloud and private access | `cloud/src/server.ts`, `cloud/worker/`, `src/cloud.ts` | reference §9 |
| Burst build pool | `cloud/src/builders.ts`, `cloud/worker/builder.mjs`, `burst.mjs` | reference §9 |
| External home host | `cloud/src/home-host.ts`, `cloud/host/`, `cloud/worker/home-host.mjs`, `src/cloud-host.ts` | reference §9.6 |
| Planning, prompts, integration | native `tasks.rs`, `src/brain.ts`, `scheduler.ts` | reference §14 |
| Metrics, dashboards, scaler | `observability/`, `cloud/worker/telemetry.mjs`, `cloud/src/telemetry.ts` | `observability/README.md` |

## Invariants

- Only the App loop mutates native App state and owns PTYs. Background work
  returns typed messages; never mutate App from a thread or block the UI on
  network, Git or model calls. Dashboard views are clients. Native Cloud calls
  use a TS helper, not an HTTP client in the render/input loop.
- One scheduling authority per project. A projector has no scheduler. Fabric
  migration refuses known live legacy owners and uses explicit epochs. Old
  incompatible clients are not physically fenced by a new flag. Never silently
  take over live work or start a competing scheduler.
- Backend completion, verification, local merge, push and verified deployment
  are separate facts. A message write is not an agent acknowledgment. An
  interrupt request is not confirmation that work stopped. Intentional PTY
  shutdown persists Paused and removes the live `process` object.
- jj is the history authority, colocated with Git. Use `jj status`/`jj diff` in
  workspaces. Keep crash recovery paths, but put scratch in `$RUDDER_SCRATCH`
  (or the system temp dir), never at the top of `/workspace`; Rudder deletes
  agent scratch after the work finishes. The shipped jj pin is 0.40.0;
  newer colocated workspaces need newer Git. Do not casually change the pin or
  use raw Git history mutations inside managed workers.
- TS JSON writes use `writeJson`/`updateJson`. Engine commands use transactional
  receipts, idempotency keys and owner/run epochs; uncertain effects require
  reconciliation. CAS refs carry kind/raw SHA256/size. Pin before publication
  and hold staging retention through transfer. SQLite is local authority,
  not fleet consensus.
- Preserve terminal key encoding, scrollback, attached-pane sizing and the
  single-writer contract. macOS Option keys may arrive as typographic chars.
- `dist/` is ignored build output; never commit it. Avoid whole-tree formatting
  during parallel work. Use plain product copy and no em dashes.

## Verification

Agents write good code; checks are a quick sanity pass, not a ritual.

- Run one targeted check for what you changed: one test file, one
  `cargo test` filter, or `npm run check`. Docs and copy edits need none.
- Add a test only when it pins a real bug or tricky logic, not to show work.
- No full suites, TUI suite or `npm run build` per change, and no waiting
  on CI. The full suite runs once per release (see Shipping).
- Re-run a failing test alone before treating it as real.

The build cleans `dist/`, compiles TS/board/native and copies the binary. Do not
run it concurrently with `npm test`, which also writes `dist/`. The TUI suite
uses a real binary and disposable backends. Large storage/build fixtures belong
on `/workspace`; never mount over user checkouts or fill Cloud's small `/tmp`.
Keep one task's scratch in one `/workspace/rudder-scratch/<task>/` reused across
attempts, and delete it when the task ends. Tests remove every temp dir they
create, read-only ones included.

## Development speed: spend time on product, not process

- **Work on `main`.** No feature branches, PRs or long-lived side branches.
  Commit small and often to local `main`. `release-check/*` branches exist only
  during a release and are deleted after it.
- **Run only the check for what you changed** (see Verification). The full
  suite runs once per release, not per commit, task or agent.
- **No prereleases until 4.0.0.** Do not cut alpha versions or deploy Cloud.
  CI is disabled except the tag-triggered Release workflow. Push `main` to
  GitHub when it makes sense (a working milestone, end of day); no version
  bump needed.
- **No git archaeology.** Do not spend turns on history, rebases, bisects or
  branch bookkeeping unless the task is about history.
- **No evidence.** No status docs, receipts, qualification or proof files.
  Normal tests are enough. Update
  `docs/v4-handoff.md`; put proof in your final message. Do not commit
  logs, captures or JSON receipts.
- **Add work only when truly needed.** No hardening waves or follow-up
  chains; leftover ideas go in your final message.
- **Keep the user's goal in front:** Rudder agents feel fast. If work does not
  make agents faster, cheaper or more reliable, skip it.

## Shipping

An explicit push/ship/publish/install/deploy request authorizes the documented
workflow; preserve that authorization across turns. Ordinary edits alone do not
authorize unrelated deployment. Never force-push.

Pushing `main` needs no release. A release is: run the full checks below
once, commit, `npm version`, then push main and the created tag. Tag CI
publishes; do not manually `npm publish`. v4 prereleases use `next`;
stable uses `latest`. Verify CI and actual registry availability. Docs-only
pushes do not require a product version bump.

```sh
npm run check
npm test
npm --prefix cloud run build
cargo test --workspace
npm run build
RUDDER_NATIVE_BIN="$PWD/target/release/rudder-native" npm run test:tui
```

Cloud also needs the worker image and Fly control-plane deployment. Verify
`https://rudder.exla.ai/app`, revision/image/deployment, UTC time and smoke checks.
Let active homes update when idle; never restart live agents to force adoption.
Filter provider output so worker tokens stay private. Mac installation/release
has separate build, install/launch and signing gates; see reference §§9, 11.

End with a short final message: what changed, what was checked, what is left.
`rudder done` still exists until the ceremony removal lands; when a run's prompt asks for
it, keep it to that same short summary. Report unfinished work honestly. The
plan is [PLAN.md section 0](PLAN.md); current status and open work
are in [the v4 handoff](docs/v4-handoff.md). `docs/architecture/` is background
reference only.
