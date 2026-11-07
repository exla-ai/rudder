// ⌥d diff panel, end to end on the REAL binary: a worker writes a file into
// its jj workspace, the panel opens beside the worker with that file parsed
// from `jj diff`, gives way to the full-screen worker under ⌥h, and closes on Esc.
import assert from "node:assert/strict";
import test from "node:test";

import { assertPrerequisites, fakeBackends, launchRudder, removeScratch, scratchRepo } from "./helpers.mjs";

assertPrerequisites();

const ALT_D = "\x1bd";
const ALT_H = "\x1bh";

async function finishedWorker(t, prefix) {
  const repo = await scratchRepo(prefix);
  t.after(() => removeScratch(repo));
  const { completer } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: completer, cols: 160, rows: 40 });
  await session.waitForText("describe a task", { timeout: 20_000 });
  await session.type("write the done marker");
  await session.press("Enter");
  // The completer exits after writing DONE.txt; the row reaches review.
  await session.waitForText("press m", { timeout: 30_000 });
  return { repo, session };
}

test("⌥d opens a diff panel with the worker's change set and Esc closes it", { timeout: 90_000 }, async (t) => {
  const { session } = await finishedWorker(t, "rudder-tui-diffpanel-");
  await session.press("Ctrl+W");
  await session.press("2");

  await session.write(ALT_D);
  await session.waitForText("╭ diff", { timeout: 10_000 });
  // The panel parses the real jj diff of the workspace: the completer's file,
  // its one added line, and the running totals in the title.
  await session.waitForText("DONE.txt", { timeout: 15_000 });
  const screen = await session.screen();
  assert.ok(screen.includes("+1 −0"), `stats for the one-line file:\n${screen}`);
  assert.ok(screen.includes("done marker"), `the added line itself:\n${screen}`);
  assert.ok(screen.includes("A DONE.txt"), `an added file is badged A:\n${screen}`);
  assert.ok(screen.includes("╭ claude"), `the worker pane stays beside it:\n${screen}`);

  // ⌥h with the diff focused still puts the WORKER full screen: no diff, no
  // header row (wordmark, branch), no task line. Its title repeats the task
  // with the sidebar gone, so it reads "<task> · claude ..." there.
  await session.write(ALT_H);
  await session.waitFor(
    (s) =>
      s.includes("· claude") &&
      !s.includes("╭ diff") &&
      !s.includes("\u2387") &&
      !s.includes("describe a task"),
    { timeout: 10_000, label: "the worker alone on screen" },
  );
  // ⌥h again: the split comes back with focus on the panel (Esc below needs it).
  await session.write(ALT_H);
  await session.waitForText("╭ diff", { timeout: 10_000 });
  await session.waitForText("╭ claude", { timeout: 10_000 });

  // Esc closes the panel and the worker pane widens again.
  await session.press("Escape");
  await session.waitFor((s) => !s.includes("╭ diff"), { timeout: 10_000, label: "the diff panel closing" });
  assert.ok((await session.screen()).includes("╭ claude"));
});
