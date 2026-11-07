// Alt+h against the REAL binary: hide the agent list and task line while
// keeping the worker, header and footer visible. From the task input, Alt+h
// again brings the panes back with the half-typed task intact.
import assert from "node:assert/strict";
import test from "node:test";

import { assertPrerequisites, fakeBackends, launchRudder, scratchRepo } from "./helpers.mjs";

assertPrerequisites();

test("Alt+h hides agents and task while keeping the worker and chrome", { timeout: 90_000 }, async (t) => {
  const repo = await scratchRepo("rudder-full-screen-");
  const { claudeBin } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin });
  await session.waitForText("describe a task", { timeout: 30_000 });

  // The split shows the agents sidebar (its empty state) beside the worker pane.
  await session.waitForText("no agents yet", { timeout: 10_000 });
  await session.waitForText("╭ terminal", { timeout: 10_000 });
  await session.type("half typed");
  await session.waitForText("half typed", { timeout: 10_000 });

  await session.press("Alt+h");
  await session.waitForText("shows the panes", { timeout: 10_000 });
  await session.waitForGone("no agents yet", { timeout: 10_000 });
  await session.waitForGone("half typed", { timeout: 10_000 });
  const full = await session.screen();
  assert.ok(full.includes("╭ terminal"), `the worker expands:\n${full}`);
  assert.ok(full.includes("\u2387"), `the header stays visible:\n${full}`);
  assert.ok(full.includes("show panels"), `the footer stays visible:\n${full}`);

  await session.press("Alt+h");
  await session.waitForText("panes shown", { timeout: 10_000 });
  await session.waitForText("no agents yet", { timeout: 10_000 });
  // Focus went back to the task input with the draft as it was.
  await session.type(" more");
  await session.waitForText("half typed more", { timeout: 10_000 });
});
