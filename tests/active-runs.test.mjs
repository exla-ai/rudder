import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { activeRunsForCheckout } from "../dist/git.js";
import { createRunRecord, saveRunRecord } from "../dist/state.js";

test("activeRunsForCheckout matches filesystem aliases via realpath", async (t) => {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-active-runs-"));
  t.after(async () => {
    await fsp.rm(root, { recursive: true, force: true });
  });
  const checkout = path.join(root, "repo");
  const alias = path.join(root, "repo-alias");
  await fsp.mkdir(checkout, { recursive: true });
  try {
    await fsp.symlink(checkout, alias, "dir");
  } catch (error) {
    t.skip(`directory symlinks unavailable: ${error instanceof Error ? error.message : String(error)}`);
    return;
  }

  const run = await createRunRecord({
    id: "run-active",
    repoRoot: checkout,
    task: "active run",
    backend: "claude",
    targetBranch: "main",
    baseCommit: "HEAD",
    useWorkspace: true,
    workspacePath: checkout,
  });
  run.status = "running";
  run.process = { pid: process.pid, startedAt: new Date().toISOString() };
  await saveRunRecord(run);

  const active = await activeRunsForCheckout(checkout, alias);
  assert.deepEqual(active.map((item) => item.id), ["run-active"]);
});
