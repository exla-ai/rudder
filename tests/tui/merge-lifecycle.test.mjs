// The merge lifecycle as SEQUENCES — the block the mutation campaign proved
// uncovered (merge and undo were silent-no-op-able with a green suite).
// Writing these tests found undo broken three ways in production: the pane
// spawned a bare "rudder" the PTY could not resolve, the lifecycle-hooks
// sweep killed hook-less CLI panes within a millisecond, and the recorded
// undo waypoint was the POST-merge op — restoring to it changed nothing while
// reporting success. Screen assertions alone would have blessed that last
// one, which is why every step here also checks the disk.
import fsp from "node:fs/promises";
import path from "node:path";
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

async function waitFile(file, present, timeout = 20_000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const exists = await fsp.access(file).then(() => true, () => false);
    if (exists === present) return;
    if (Date.now() >= deadline) {
      throw new Error(`timed out waiting for ${file} to be ${present ? "present" : "gone"}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 150));
  }
}

test("two workers merge, undo takes the work back out", { timeout: 180_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-lifecycle-");
  t.after(() => removeScratch(repo));
  const { completerUnique } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: completerUnique });
  const story = journey(session, "merge then undo: work lands, work leaves");

  await story.step("dashboard boots", async () => {
    await session.waitForText("describe a task", { timeout: 20_000 });
  });

  await story.step("two workers finish and merge with m", async () => {
    await session.type("first unique task");
    await session.press("Enter");
    await mergeFinishedWorker(session);
    await session.waitForText("done 1", { timeout: 30_000 });
    await session.press("Ctrl+W");
    await session.press("3");
    await session.type("second unique task");
    await session.press("Enter");
    await mergeFinishedWorker(session);
    await session.waitForText("done 2", { timeout: 60_000 });
    // Each fake worker writes DONE-<workspace-slug>.txt; exactly two land.
    const deadline = Date.now() + 20_000;
    for (;;) {
      const files = (await fsp.readdir(repo)).filter((name) => name.startsWith("DONE-"));
      if (files.length === 2) break;
      if (Date.now() >= deadline) {
        throw new Error(`expected 2 DONE-* files after both merges, found: ${files.join(", ")}`);
      }
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  });

  await story.step("u undoes a merge; its file leaves the checkout", async () => {
    await session.press("Ctrl+W");
    await session.press("1");
    await session.press("u");
    // The undo pane is a plain rudder CLI process; its success line is the
    // proof the command ran (three separate production bugs once prevented
    // this line from ever appearing).
    await session.waitForText("Restored jj to operation", { timeout: 30_000 });
    // The undo restores the pre-merge operation, so at least one merged file
    // must actually leave the working copy — the disk is the arbiter.
    const deadline = Date.now() + 20_000;
    for (;;) {
      const files = (await fsp.readdir(repo)).filter((name) => name.startsWith("DONE-"));
      if (files.length < 2) break;
      if (Date.now() >= deadline) {
        throw new Error(`undo reported success but both DONE-* files remain: ${files.join(", ")}`);
      }
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  });

  await story.end();
});

test("a single merge then undo round-trips the checkout", { timeout: 120_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-undo-");
  t.after(() => removeScratch(repo));
  const { completer } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: completer });
  await session.waitForText("describe a task", { timeout: 20_000 });

  await session.type("write the done marker");
  await session.press("Enter");
  // A dashboard worker's turn ending does not merge it: the user may still be
  // talking to it. The row holds in review and the checkout stays untouched.
  await session.waitFor((s) => s.includes("press m to read the") && !s.includes("done 1"), {
    timeout: 60_000,
    stablePolls: 5,
    label: "the finished worker holding in review",
  });
  await fsp.access(path.join(repo, "DONE.txt")).then(
    () => {
      throw new Error("a dashboard worker merged on its own when its turn ended");
    },
    () => {},
  );
  await mergeFinishedWorker(session);
  // The merged row is inside the `done` drawer now; the header carries its count.
  await session.waitForText("done 1", { timeout: 30_000 });
  await waitFile(path.join(repo, "DONE.txt"), true);

  await session.press("Ctrl+W");
  await session.press("1");
  await session.press("u");
  await session.waitForText("Restored jj to operation", { timeout: 30_000 });
  await waitFile(path.join(repo, "DONE.txt"), false);
});

test("a merged row survives a restart with its identity intact", { timeout: 120_000 }, async (t) => {
  // The mutation loop found this gap: dropping a row's workspace identity
  // during merge is invisible to a single-pass test — the file lands, the
  // screen says "merged". It only surfaces on the NEXT life of the dashboard,
  // where a row that lost its workspaceName reloads as broken. Restart is the
  // highest-value class precisely because corruption hides until then; this is
  // the merge-persistence sequence no test covered.
  const repo = await scratchRepo("rudder-tui-merge-restart-");
  t.after(() => removeScratch(repo));
  const { completer } = await fakeBackends(repo);
  let session = await launchRudder(t, { repo, claudeBin: completer });
  await session.waitForText("describe a task", { timeout: 20_000 });

  await session.type("write the done marker");
  await session.press("Enter");
  await mergeFinishedWorker(session);
  // The merged row is inside the `done` drawer now; the header carries its count.
  await session.waitForText("done 1", { timeout: 30_000 });
  await waitFile(path.join(repo, "DONE.txt"), true);

  // Restart. The merged row must reload as merged, still owning its workspace
  // (so it remains undoable) — not orphaned, failed, or identity-stripped.
  await session.kill();
  session = await session.respawn();
  await session.waitForText("write the done marker", { timeout: 30_000 });
  await session.waitForText("merged", { timeout: 20_000 });
  const screen = await session.screen();
  if (/orphaned|identity|no jj workspace/i.test(screen)) {
    throw new Error(`merged row lost its identity across restart:\n${screen}`);
  }

  // Proof the reloaded row is still fully operable: undo works from its new
  // life, which requires the workspace identity to have survived the save.
  await session.press("Ctrl+W");
  await session.press("1");
  await session.press("u");
  await session.waitForText("Restored jj to operation", { timeout: 30_000 });
  await waitFile(path.join(repo, "DONE.txt"), false);
});
