// Visual coverage for rudder: status color-coding. screen() reads the words
// ("working", "merged") but not their COLOR — yet color is how the dashboard
// signals state at a glance. A regression that renders every status the same
// color, or drops the color, is invisible to text assertions. These assert on
// the actual rendered fg (truecolor hex) via the framework's styleAt.
import assert from "node:assert/strict";
import test from "node:test";

import {
  assertPrerequisites,
  fakeBackends,
  launchRudder,
  mergeFinishedWorker,
  removeScratch,
  scratchRepo,
} from "./helpers.mjs";

assertPrerequisites();

// Rudder's theme (native/src/theme.rs, dark palette: the test terminal does
// not report its colors): running blue, merged violet.
const RUNNING = "#6cb6ff";
const MERGED = "#a394ff";

/** A working status shimmers: a light band sweeps across it frame by frame,
 * so its exact shade moves. It always stays in the running blue family. */
function isRunningBlue(fg) {
  const hex = String(fg).toLowerCase();
  const match = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/.exec(hex);
  if (!match) return false;
  const [r, g, b] = match.slice(1).map((part) => parseInt(part, 16));
  return b > g && g > r;
}

test("a running worker's status renders in the running color", { timeout: 60_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-vis-run-");
  t.after(() => removeScratch(repo));
  const { sleeper } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("describe a task", { timeout: 20_000 });

  await session.type("a colored running task");
  await session.press("Enter");
  await session.waitForText("working", { timeout: 30_000 });

  const style = await session.styleAt("working");
  assert.ok(style, "the running status label is on screen");
  assert.ok(
    isRunningBlue(style.fg),
    `the running status is rendered in the running (blue) color, got ${style.fg}`,
  );
});

test("running and merged statuses render in DIFFERENT colors", { timeout: 120_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-vis-merge-");
  t.after(() => removeScratch(repo));
  const { completer } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: completer });
  await session.waitForText("describe a task", { timeout: 20_000 });

  await session.type("a task that merges");
  await session.press("Enter");
  // A dashboard worker waits in review when its turn ends; m merges it.
  await mergeFinishedWorker(session);
  // A merged row collapses into the `done` drawer, so its status label lives in the
  // drawer's list. Open the drawer and sample the color there: collapsing the
  // section must not cost the state-at-a-glance color coding.
  await session.waitForText("done 1", { timeout: 30_000 });
  await session.press("Ctrl+W");
  await session.press("1");
  await session.press("Enter");
  await session.waitForText("merged locally", { timeout: 20_000 });

  const merged = await session.styleAt("merged locally");
  assert.ok(merged, "the merged status is on screen");
  assert.equal(
    String(merged.fg).toLowerCase(),
    MERGED,
    "a merged worker is rendered in the merged (violet) color, distinct from running blue",
  );
  assert.notEqual(String(merged.fg).toLowerCase(), RUNNING, "state colors are not identical");
});
