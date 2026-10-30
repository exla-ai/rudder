# Real backend steering evidence

`real-providers.json` records a disposable persistent engine and real attached
dashboard running Claude Code 2.1.314 and Codex 0.166.0. Both used the actual
provider, managed adapters and exact thread/turn pins. The Codex executable
digest is for the native ELF; the npm JavaScript entrypoint has a separate hash.

Claude busy steering confirmed interruption and accepted a replacement. Codex
confirmed interruption, projected Paused and resumed the same thread with typed
untrusted peer context. Backend completion did not acknowledge durable mail.
The Codex yielded shell remained alive after model-turn interruption; both
owned tool processes were gone after engine shutdown. This is not proof that
turn interruption kills every process or that the fleet migration is complete.

Only this report and the runner are archived. Provider homes, credentials,
transcripts and identity files are excluded. Local private paths in the report
are placeholders. The run used an unreleased working tree, not a frozen release
revision; local adapter/actor regression suites cover subsequent lifecycle fixes.

To rerun, build the current native/TypeScript code, install the terminal harness,
and configure these environment variables outside the repository:

- `RUDDER_QUALIFICATION_CODEX_BIN`: pinned Codex CLI entrypoint.
- `RUDDER_QUALIFICATION_CODEX_EXECUTABLE`: its actual native executable, for hashing.
- `RUDDER_QUALIFICATION_CLAUDE_ENV_FILE` and
  `RUDDER_QUALIFICATION_CODEX_ENV_FILE`: private JSON maps of provider environment
  variables. Use dedicated access-token profiles; do not share refresh ownership
  with a running session.
- Optional `RUDDER_NATIVE_BIN`, `RUDDER_QUALIFICATION_CLAUDE_BIN` and
  `RUDDER_QUALIFICATION_REPO` override the built/native paths.

Run `node examples/agent-runtime/evidence/2026-10-30/qualify.mjs`. The runner uses
real inference and a small Claude budget, creates a private disposable checkout
and reports explicit cleanup results. Retain only the safe report afterward.
