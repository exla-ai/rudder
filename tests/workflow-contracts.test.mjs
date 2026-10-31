import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import {
  CODEX_RUDDER_COMMON_CONFIG_ARGS,
  CODEX_RUDDER_PLANNER_CONFIG_ARGS,
  CODEX_RUDDER_WORKER_CONFIG_ARGS,
} from "../dist/codex-binary.js";
import { codexEffortConfigArgs, DEFAULT_EFFORT } from "../dist/effort.js";
import { columnForRun, hasVerifiedDelivery } from "../dist/board/daemon.js";
import { parseCompletionNoteArg } from "../dist/surfaces.js";

function run(overrides = {}) {
  return {
    id: "run-1",
    status: "completed",
    task: "do the work",
    backend: "codex",
    createdAt: "2026-07-17T00:00:00Z",
    updatedAt: "2026-07-17T00:01:00Z",
    repoRoot: "/tmp/repo",
    targetBranch: "main",
    baseCommit: "abc",
    workspace: { enabled: false, path: "/tmp/repo" },
    ...overrides,
  };
}

test("Codex workers get plugin and Computer Use parity while planners stay restricted", () => {
  assert.ok(CODEX_RUDDER_WORKER_CONFIG_ARGS.includes("features.plugins=true"));
  assert.ok(CODEX_RUDDER_WORKER_CONFIG_ARGS.includes("features.computer_use=true"));
  assert.ok(!CODEX_RUDDER_WORKER_CONFIG_ARGS.includes("features.plugins=false"));
  assert.ok(CODEX_RUDDER_PLANNER_CONFIG_ARGS.includes("features.plugins=false"));
  assert.ok(CODEX_RUDDER_PLANNER_CONFIG_ARGS.includes("features.computer_use=false"));
});

test("TS and native Codex launches share one efficiency policy", async () => {
  const launch = await readFile(new URL("../native/src/launch.rs", import.meta.url), "utf8");
  const start = launch.indexOf("pub(crate) const CODEX_COMMON_CONFIG");
  const block = launch.slice(start, launch.indexOf("];", start));
  const native = [...block.matchAll(/^\s*("(?:[^"\\]|\\.)*"),$/gm)].map((match) => JSON.parse(match[1]));
  const ts = CODEX_RUDDER_COMMON_CONFIG_ARGS.filter((_, index) => index % 2 === 1);
  assert.deepEqual(native, ts);
  assert.ok(ts.includes("model_auto_compact_token_limit=150000"));
  assert.ok(ts.includes("tool_output_token_limit=6000"));

  const models = await readFile(new URL("../native/src/models.rs", import.meta.url), "utf8");
  assert.match(models, /const DEFAULT_EFFORT: EffortLevel = EffortLevel::High;/);
  assert.equal(DEFAULT_EFFORT, "high");
  assert.deepEqual(codexEffortConfigArgs(undefined), ["-c", 'model_reasoning_effort="high"']);
  assert.deepEqual(codexEffortConfigArgs("low"), ["-c", 'model_reasoning_effort="low"']);
  assert.deepEqual(codexEffortConfigArgs("max"), ["-c", 'model_reasoning_effort="xhigh"']);
});

test("direct completion is done, but requested delivery stays review until proof is complete", () => {
  assert.equal(columnForRun(run({ mode: "main" })), "done");
  assert.equal(
    columnForRun(
      run({
        mode: "main",
        delivery: { required: true, status: "pending", checks: [] },
      }),
    ),
    "review",
  );

  const delivered = run({
    mode: "main",
    delivery: {
      required: true,
      kind: "web",
      status: "verified",
      target: "https://example.test",
      revision: "abc123",
      verifiedAt: "2026-07-17T00:02:00Z",
      checks: ["GET / returned 200"],
    },
  });
  assert.equal(hasVerifiedDelivery(delivered), true);
  assert.equal(columnForRun(delivered), "done");

  assert.equal(
    columnForRun(run({ workspace: { enabled: true, path: "/tmp/worker" } })),
    "review",
    "an isolated worker still needs integration",
  );
});

test("rudder done preserves structured delivery evidence", () => {
  const note = parseCompletionNoteArg(
    JSON.stringify({
      summary: "deployed production",
      delivery: {
        kind: "web",
        status: "verified",
        target: "https://example.test",
        verifiedAt: "2026-07-17T00:02:00Z",
        checks: ["homepage passed"],
      },
    }),
  );
  assert.equal(note.delivery?.kind, "web");
  assert.deepEqual(note.delivery?.checks, ["homepage passed"]);
});
