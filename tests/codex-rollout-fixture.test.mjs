// The phone's Codex cards (cloud/worker/asks.mjs) and the native dashboard's
// "needs you" (native/src/codex_rollout.rs) read the same rollout with the same
// rules. Both are pinned to real Codex 0.170 sessions and one set of
// expectations each, so a rule changed on one side fails here or in
// `codex_rollout::tests` until the other side mirrors it. The second session
// leaves Plan mode with Shift+Tab and with a /model pick (the rollouts were
// trimmed to the lines the rules read).
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { codexAsks, pendingCodexAsks } from "../cloud/worker/asks.mjs";

const fixture = (name) => fs.readFileSync(new URL(`./fixtures/${name}`, import.meta.url), "utf8");
const lines = fixture("codex-0.170-rollout.jsonl").split("\n").filter(Boolean);

for (const name of ["codex-0.170-rollout", "codex-0.170-rollout-plan-left"]) {
  test(`real Codex 0.170 rollout ${name} gives the shared expectations at every line`, () => {
    const session = fixture(`${name}.jsonl`).split("\n").filter(Boolean);
    const expected = JSON.parse(fixture(`${name}.expect.json`));
    assert.equal(expected.length, session.length);
    for (const step of expected) {
      const tools = pendingCodexAsks(session.slice(0, step.lines)).map((ask) => ask.tool);
      assert.deepEqual(tools, step.waits, `after ${step.lines} lines`);
    }
  });
}

test("its plan is over once the cleared thread's rollout starts in the same folder", () => {
  const day = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "rudder-codex-fixture-")), "2026", "11", "18");
  fs.mkdirSync(day, { recursive: true });
  try {
    const rollout = path.join(day, "rollout-2026-11-18T11-26-51-01a1ee68-da13-7822-a7de-75c3368eeed4.jsonl");
    fs.writeFileSync(rollout, lines.join("\n") + "\n");
    const live = { process: { pid: process.pid } };
    assert.deepEqual(codexAsks(live, rollout).map((ask) => ask.tool), ["Plan"]);
    fs.writeFileSync(path.join(day, "rollout-2026-11-18T11-31-28-01a1ee6d-12ce-7b62-adf4-508197eeb930.jsonl"), fixture("codex-0.170-rollout-cleared.jsonl"));
    assert.deepEqual(codexAsks(live, rollout), []);
  } finally {
    fs.rmSync(path.resolve(day, "..", "..", ".."), { recursive: true, force: true });
  }
});
