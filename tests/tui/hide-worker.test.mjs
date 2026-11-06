// Alt+h against the REAL binary: it hides the worker pane and nothing else
// (the agent list and the task line stay), and Alt+h again brings it back.
import assert from "node:assert/strict";
import test from "node:test";

import { assertPrerequisites, fakeBackends, launchRudder, scratchRepo } from "./helpers.mjs";

assertPrerequisites();

test("Alt+h hides only the worker pane and brings it back", { timeout: 90_000 }, async (t) => {
  const repo = await scratchRepo("rudder-hide-worker-");
  const { claudeBin } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin });
  await session.waitForText("describe a task", { timeout: 30_000 });

  // The split shows the agents sidebar (its empty state) beside the worker pane.
  await session.waitForText("no agents yet", { timeout: 10_000 });
  await session.waitForText("╭ terminal", { timeout: 10_000 });

  await session.press("Alt+h");
  await session.waitForGone("╭ terminal", { timeout: 10_000 });
  const hidden = await session.screen();
  assert.ok(hidden.includes("no agents yet"), `the sidebar stays:\n${hidden}`);
  assert.ok(hidden.includes("describe a task"), `the task line stays:\n${hidden}`);

  await session.press("Alt+h");
  await session.waitForText("worker shown", { timeout: 10_000 });
  await session.waitForText("╭ terminal", { timeout: 10_000 });
});
