// The verifier judges ONE pass. output.txt holds every turn of a run, and
// scanning all of it let a first turn's "I didn't run any checks" fail every
// later check, so the steerer kept re-prompting a finished, verified run
// (a cloud smoke sail: done in 2.5 minutes, steered for 4.5 more).
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { verifyRun } from "../dist/brain.js";

test("verification reads only the pass it is judging", async (t) => {
  const repo = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-verify-"));
  t.after(() => fs.rmSync(repo, { recursive: true, force: true }));
  execFileSync("git", ["init", "-q"], { cwd: repo });
  fs.writeFileSync(path.join(repo, "SMOKE.md"), "ok\n");

  const id = "run-verify";
  const runDir = path.join(repo, ".rudder", "runs", id);
  fs.mkdirSync(runDir, { recursive: true });
  const firstTurn = "Created SMOKE.md. I didn't run any checks since there's no code to test.\n";
  const secondTurn = "Re-checked: SMOKE.md contains `ok` (od -c shows o k \\n). Only change.\n";
  fs.writeFileSync(path.join(runDir, "output.txt"), firstTurn + secondTurn);

  const run = {
    id,
    repoRoot: repo,
    task: "Write the word ok into SMOKE.md",
    vcs: "git",
    workspace: { enabled: false, path: repo },
  };

  const whole = await verifyRun(run);
  assert.ok(
    whole.missing.some((item) => item.includes("skipped or could not run")),
    "the first turn alone does admit skipping checks",
  );

  const second = await verifyRun(run, { transcriptFrom: Buffer.byteLength(firstTurn) });
  assert.deepEqual(second.missing, [], "the second pass is judged on what it said");
});
