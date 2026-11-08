// The finished-work drawer as a SEQUENCE against the real binary. A long session
// ends with dozens of merged and failed rows; listing each one buries the two that
// are still moving. `done` and `closed` collapse to a single sidebar row that opens
// into member cards in the sidebar while preserving the worker pane.
//
// Unit tests cover the state machine; this covers what a user actually does — merge
// a worker, watch its row leave the sidebar, open the drawer, and read the result —
// through real keystrokes on the rendered screen.
import test from "node:test";

import { journey } from "tui-integration-tests";
import {
  assertPrerequisites,
  fakeBackends,
  launchRudder,
  mergeFinishedWorker,
  removeScratch,
  scratchRepo,
} from "./helpers.mjs";

assertPrerequisites();

test("a merged row collapses into the done drawer and opens to its result", { timeout: 180_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-drawer-");
  t.after(() => removeScratch(repo));
  const { completer } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: completer });
  const story = journey(session, "finished work collapses into a drawer");

  await story.step("a worker merged with m collapses into the done drawer", async () => {
    await session.waitForText("describe a task", { timeout: 20_000 });
    await session.type("write the done marker");
    await session.press("Enter");
    // A dashboard worker waits in review when its turn ends; m merges it.
    await mergeFinishedWorker(session);
    // One collapsed row with a count and a closed fold glyph — not a per-run row.
    await session.waitForText("done 1", { timeout: 20_000 });
    const screen = await session.screen();
    if (!screen.includes("\u251c\u2504")) {
      throw new Error(`expected the closed fold glyph on the done header:\n${screen}`);
    }
  });

  await story.step("Enter on the drawer header opens it to the run's result", async () => {
    await session.press("Ctrl+W");
    await session.press("1");
    // j walks past the live rows onto the done header, which Enter opens.
    for (let i = 0; i < 6; i += 1) {
      const screen = await session.screen();
      if (/▶ *done/.test(screen)) break;
      await session.press("j");
    }
    const beforeOpen = await session.screen();
    if (!/▶ *done/.test(beforeOpen)) {
      throw new Error(`j never landed the cursor on the done header:\n${beforeOpen}`);
    }
    await session.press("Enter");
    // Expansion reveals the member card in the sidebar and keeps its session visible.
    await session.waitForText("\u252c\u2500", { timeout: 20_000 });
    await session.waitForText("This agent is not running", { timeout: 20_000 });
    // The status coding survives the collapse: the row inside the drawer still
    // says what state the run ended in.
    await session.waitForText("merged locally", { timeout: 20_000 });
  });

  await story.step("Esc backs out and the sidebar is intact", async () => {
    await session.press("Escape");
    // A lone ESC is held briefly by the input reader while it waits to see whether
    // an escape SEQUENCE follows, so wait for the drawer to go rather than snapshot.
    await session.waitForGone("\u252c\u2500", { timeout: 20_000 });
    const screen = await session.screen();
    if (!screen.includes("done 1")) {
      throw new Error(`the collapsed header survives closing the drawer:\n${screen}`);
    }
    if (screen.includes("\u252c\u2500")) {
      throw new Error(`the expanded sidebar should be gone after Esc:\n${screen}`);
    }
    if (!screen.includes("\u251c\u2504")) {
      throw new Error(`the fold glyph should read closed after Esc:\n${screen}`);
    }
  });

  await story.end();
});
