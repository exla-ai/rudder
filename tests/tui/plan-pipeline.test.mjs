// The automatic planning pipeline — the deepest, most complex flow:
// untested end-to-end: a plan is proposed as a task DAG, approved, its nodes
// run as isolated workers, and their work merges into the checkout. This is a
// journey because every step depends on the last, and the whole storyline is
// worth keeping in the report when any of it breaks.
import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import path from "node:path";
import test from "node:test";

import { journey } from "tui-integration-tests";
import {
  assertPrerequisites,
  launchRudder,
  planPipelineBackend,
  removeScratch,
  scratchRepo,
  waitUntil,
} from "./helpers.mjs";

assertPrerequisites();

test("plan → approve → workers → merge lands every node's work", { timeout: 180_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-plan-");
  t.after(() => removeScratch(repo));
  const { claudeBin, nodeCount } = await planPipelineBackend(repo, 2);
  // The headless decomposer scrapes the DAG from the planner's output; the
  // default interactive orchestrator reads it from RUDDER.md instead (a
  // separate, follow-up coverage target).
  const session = await launchRudder(t, {
    repo,
    claudeBin,
    env: { RUDDER_INTERACTIVE_ORCHESTRATOR: "0" },
  });
  const story = journey(session, "plan pipeline: propose, approve, run, merge");
  const nodeIds = Array.from({ length: nodeCount }, (_, i) => `n${i}`);
  const observedWorkers = new Map();
  const readIfPresent = async (file) => {
    try { return await fsp.readFile(file, "utf8"); }
    catch (error) { if (error.code === "ENOENT") return null; throw error; }
  };
  const nodeRecords = async () => {
    const root = path.join(repo, ".rudder", "runs");
    let entries;
    try { entries = await fsp.readdir(root, { withFileTypes: true }); }
    catch (error) { if (error.code === "ENOENT") return []; throw error; }
    const records = await Promise.all(entries.filter((entry) => entry.isDirectory()).map(async (entry) => {
      const raw = await readIfPresent(path.join(root, entry.name, "run.json"));
      return raw === null ? null : JSON.parse(raw);
    }));
    return records.filter((record) => record && nodeIds.includes(record.nodeId));
  };
  const everyWorkerProducedOutput = async ({ finished = false, merged = false } = {}) => {
    const records = await nodeRecords();
    for (const nodeId of nodeIds) {
      const matches = records.filter((record) => record.nodeId === nodeId);
      if (matches.length === 0) return false;
      assert.equal(matches.length, 1, `one actual run for ${nodeId}`);
      const record = matches[0], workspace = record.workspace?.path;
      assert.equal(record.vcs, "jj", `${nodeId} uses a jj workspace`);
      assert.equal(typeof workspace, "string", `${nodeId} has its actual workspace path`);
      assert(path.isAbsolute(workspace));
      const relative = path.relative(path.join(repo, ".rudder-workspaces"), workspace);
      assert(relative && !path.isAbsolute(relative) && relative !== ".." && !relative.startsWith(`..${path.sep}`), `${nodeId} writes in an isolated workspace`);
      assert(record.workspace.enabled || record.status === "merged", `${nodeId} cannot execute in the main checkout`);
      assert.equal(typeof record.planId, "string");
      assert(record.planId.length > 0);
      // Match the existing backend's actual output name; no extra marker or
      // simulated status is introduced to keep a fast worker visible.
      const slug = path.basename(workspace).replace(/[^a-z0-9]/g, "").slice(-6);
      const file = `NODE-${slug}.txt`;
      const identity = { runId: record.id, planId: record.planId, workspace, file };
      if (observedWorkers.has(nodeId)) assert.deepEqual(identity, observedWorkers.get(nodeId));
      else observedWorkers.set(nodeId, identity);
      const integrated = record.status === "merged" && record.merge?.status === "merged";
      if (finished && !["completed", "merged"].includes(record.status)) return false;
      if (merged && !integrated) return false;
      const isolated = await readIfPresent(path.join(workspace, file));
      // The normal workspace sweep may already have removed a merged worker.
      // Its durable merged record plus its actual landed bytes remain proof.
      const landed = integrated ? await readIfPresent(path.join(repo, file)) : null;
      if (merged && landed !== "node work\n") return false;
      if (!merged && isolated !== "node work\n" && landed !== "node work\n") return false;
    }
    const workers = [...observedWorkers.values()];
    assert.equal(new Set(workers.map((worker) => worker.runId)).size, nodeCount);
    assert.equal(new Set(workers.map((worker) => worker.workspace)).size, nodeCount);
    assert.equal(new Set(workers.map((worker) => worker.file)).size, nodeCount);
    assert.equal(new Set(workers.map((worker) => worker.planId)).size, 1);
    return true;
  };

  await story.step("dashboard boots", async () => {
    await session.waitForText("describe a task", { timeout: 20_000 });
  });

  await story.step("a multi-part task automatically proposes a two-node DAG", async () => {
    await session.type("implement the API and UI in parallel");
    await session.press("Enter");
    await session.waitForText("Plan ready", { timeout: 40_000 });
    await session.waitForText("node 0", { timeout: 10_000 });
    await session.waitForText("node 1", { timeout: 10_000 });
  });

  await story.step("approving the plan launches both nodes as workers", async () => {
    // Leave the inline plan-review editor, focus the agents list, select the
    // pinned orchestrator row (◆), and Enter to approve — the launch trigger.
    await session.press("Escape");
    await session.press("Ctrl+W");
    await session.press("1");
    await session.press("k");
    await session.press("k");
    await session.waitFor((s) => s.includes("▶") && s.includes("◆"), {
      timeout: 10_000,
      label: "the orchestrator row selected",
    });
    assert.equal((await nodeRecords()).length, 0, "both nodes await actual approval");
    await session.press("Enter");
    await waitUntil(() => everyWorkerProducedOutput(), {
      timeout: 40_000,
      interval: 400,
      label: "both approved workers produce their isolated outputs",
    });
  });

  await story.step("both node workers finish", async () => {
    await waitUntil(() => everyWorkerProducedOutput({ finished: true }), {
      timeout: 60_000,
      interval: 400,
      label: "both actual node runs finish with their output intact",
    });
  });

  await story.step("nodes auto-merge and every file lands in the checkout", async () => {
    // A plan integrates its nodes automatically as they finish (no manual
    // merge). Wait for the merged state, then let the disk be the arbiter:
    // every node's file must be in the main checkout.
    await session.waitForText("merged", { timeout: 40_000 });
    await waitUntil(() => everyWorkerProducedOutput({ merged: true }), {
      timeout: 40_000,
      interval: 400,
      label: "both actual merged node outputs land in the checkout",
    });
    const landed = (await fsp.readdir(repo)).filter((name) => name.startsWith("NODE-")).sort();
    assert.deepEqual(landed, [...observedWorkers.values()].map((worker) => worker.file).sort());
  });

  await story.end();
});
