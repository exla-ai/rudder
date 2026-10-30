// Actual task routing through the release dashboard, keystrokes, backend PTY,
// and persisted run records. No model/network calls or in-process App harness.
import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import path from "node:path";
import test from "node:test";

import {
  assertPrerequisites,
  fakeBackends,
  launchRudder,
  removeScratch,
  scratchRepo,
  waitUntil,
} from "./helpers.mjs";

assertPrerequisites();

async function runRecords(repo) {
  const root = path.join(repo, ".rudder", "runs");
  const entries = await fsp.readdir(root).catch(() => []);
  const records = [];
  for (const entry of entries) {
    try {
      records.push(JSON.parse(await fsp.readFile(path.join(root, entry, "run.json"), "utf8")));
    } catch {
      // A record being atomically published is not yet a persisted run.
    }
  }
  return records;
}

test("a discussion-only task keeps one checkout owner and cannot approve implementation workers", { timeout: 60_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-discuss-");
  t.after(() => removeScratch(repo));
  const { sleeper } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });

  const task = "Just discuss the architecture and deployment plan; do not implement yet";
  await session.type(task);
  await session.press("Enter");
  await session.waitForText("discussion session in this checkout", { timeout: 20_000 });
  await waitUntil(async () => {
    const records = await runRecords(repo);
    return records.length === 1 && records[0].currentPrompt?.startsWith("Rudder discussion scope:");
  }, { timeout: 10_000, label: "the discussion owner and its scoped prompt persisted" });

  const [owner] = await runRecords(repo);
  assert.equal(owner.mode, "main");
  assert.equal(owner.task, task, "the original user request is preserved");
  assert.equal(owner.workspace.enabled, false, "discussion does not create an implementation workspace");
  assert.equal(owner.delivery.required, false, "talking about deployment does not request a release");
  assert.match(owner.currentPrompt, /Do not implement code changes, start implementation workers, launch a task DAG/);
  assert.ok(!owner.currentPrompt.includes("Drive the requested outcome through implementation"));

  // The normal approval gesture with an empty task line cannot turn discussion
  // into implementation. Its visible owner remains the same single Main row.
  await session.press("Ctrl+W");
  await session.press("3");
  await session.press("Enter");
  const screen = await session.waitFor((value) => value.includes("main") && !value.includes("Plan ready"), {
    timeout: 10_000,
    label: "one discussion owner with no implementation plan",
  });
  assert.ok(!screen.includes("isolated worker running"), screen);
  assert.deepEqual((await runRecords(repo)).map((record) => record.id), [owner.id]);
});

test("retired gam and nudge commands show their replacement without launching work", { timeout: 60_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-retired-");
  t.after(() => removeScratch(repo));
  const { sleeper } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });

  for (const [input, notice] of [
    ["/gam fix the caching bug", "/gam has been retired"],
    ["/nudge keep implementing", "/nudge has been retired"],
  ]) {
    await session.type(input);
    await session.press("Enter");
    const screen = await session.waitForText(notice, { timeout: 10_000 });
    assert.ok(!screen.includes("pick the adversarial model"), "retired launch picker is absent");
    assert.ok(!screen.includes("working"), "retired commands do not launch an agent");
    assert.deepEqual(await runRecords(repo), [], "no run is persisted for a retired command");
  }
});
