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
| Planning, prompts, integration | native `tasks.rs`, `src/brain.ts`, `scheduler.ts` | reference §14 |

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
  workspaces. Preserve old runs and recovery paths. The shipped jj pin is 0.40.0;
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

Run relevant checks; broaden only when a change or failure warrants it.

```sh
npm run check
npm test
npm --prefix cloud run build
cargo test --workspace
npm run build
RUDDER_NATIVE_BIN="$PWD/target/release/rudder-native" npm run test:tui
```

The build cleans `dist/`, compiles TS/board/native and copies the binary. Do not
run it concurrently with `npm test`, which also writes `dist/`. The TUI suite
uses a real binary and disposable backends. Large storage/build fixtures belong
on `/workspace`; never mount over user checkouts or fill Cloud's small `/tmp`.

## Shipping

An explicit push/ship/publish/install/deploy request authorizes the documented
workflow; preserve that authorization across turns. Ordinary edits alone do not
authorize unrelated deployment. Never force-push.

Product pushes to `origin/main` require a versioned npm release: run relevant
checks, commit, `npm version` appropriately, then push main and the created tag.
Tag CI publishes; do not manually `npm publish`. v4 prereleases use `next`;
stable uses `latest`. Verify CI and actual registry availability. Docs/evidence
only pushes do not require a product version bump.

Cloud also needs the worker image and Fly control-plane deployment. Verify
`https://rudder.exla.ai/app`, revision/image/deployment, UTC time and smoke checks.
Let active homes update when idle; never restart live agents to force adoption.
Filter provider output so worker tokens stay private. Mac installation/release
has separate build, install/launch and signing gates; see reference §§9, 11.

Before final handoff, file `rudder done` with summary, interfaces, follow-ups and
actual delivery proof when requested. Report unfinished work honestly. The v4
target is [PLAN.md](PLAN.md); current evidence/continuation state is in
[the implementation record](docs/architecture/implementation.md).
