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
import { pendingCodexAsks } from "../cloud/worker/asks.mjs";

const fixture = (name) => fs.readFileSync(new URL(`./fixtures/${name}`, import.meta.url), "utf8");

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
