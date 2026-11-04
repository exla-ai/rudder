import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { assertLegacyWriterAllowed, projectEngineAuthority } from "../dist/engine-authority.js";
import { persistentProjectEngineRequested } from "../dist/engine.js";
import { spawnWorker, workerRun, startRun, continueRun, stopRun, mergeRun, syncRun, deleteRun, cleanupRuns } from "../dist/run-manager.js";
import { scheduleTick, onRunTransition, launchNode, mergeNodeIntoIntegration, reconcileInjection } from "../dist/scheduler.js";
import { ensureBoardRunning } from "../dist/daemon.js";
import { applyOwnedPlanningCommand } from "../dist/planning-ownership/store.js";
import { v4ScratchPrefix } from './helpers/v4-scratch.mjs';

test("project migration survives a closed surface and blocks duplicate legacy schedulers", async (t) => {
  const repo = await fs.mkdtemp(path.join(os.tmpdir(), "rudder-engine-authority-"));
  t.after(() => fs.rm(repo, { recursive: true, force: true }));
  assert.equal(projectEngineAuthority(repo), undefined);
  assertLegacyWriterAllowed(repo);
  assert.equal(persistentProjectEngineRequested(repo, {}), false);
  await fs.mkdir(path.join(repo, ".rudder/engine"), { recursive: true });
  const ownerFile = path.join(repo, ".rudder/engine/owner.json");
  await fs.writeFile(ownerFile, JSON.stringify({ format_version: 1, authority: "fabric", state: "stopped" }));
  assert.equal(persistentProjectEngineRequested(repo, {}), true);
  assert.equal(persistentProjectEngineRequested(repo, { RUDDER_ENGINE: "legacy-native" }), false);
  assert.throws(() => spawnWorker(repo, "must-not-start"), /persistent engine/);
  await assert.rejects(workerRun(repo, "must-not-start"), /persistent engine/);
  await assert.rejects(scheduleTick(repo, {}), /persistent engine/);
  await assert.rejects(onRunTransition(repo, "must-not-start", {}), /persistent engine/);
  await assert.rejects(ensureBoardRunning(repo), /persistent engine/);
  await assert.rejects(fs.stat(path.join(repo, ".rudder/runs/must-not-start")), { code: "ENOENT" });
  await fs.writeFile(ownerFile, '{"format_version":2,"authority":"fabric"}');
  assert.throws(() => assertLegacyWriterAllowed(repo), /Unsupported/);
  await fs.writeFile(ownerFile, "interrupted write");
  assert.throws(() => assertLegacyWriterAllowed(repo), /unreadable/);
});

test("every compatible legacy writer refuses migration and recovery markers before side effects", async t => {
  const repo = await fs.mkdtemp(v4ScratchPrefix('migration-admission'));
  const previous = process.cwd();
  t.after(async () => { process.chdir(previous); await fs.rm(repo, { recursive: true, force: true }); });
  process.chdir(repo);
  const engine = path.join(repo,".rudder/engine"); await fs.mkdir(engine,{recursive:true});
  for (const name of ["v4-migration.json", "v4-recovery.json"]) {
    const marker=path.join(engine,name);
    for (const contents of ['interrupted write', JSON.stringify({phase:"committed"}), JSON.stringify({phase:"rolled_back"})]) {
      await fs.writeFile(marker,contents);
      assert.throws(()=>assertLegacyWriterAllowed(repo),/explicit recovery/);
      assert.throws(()=>spawnWorker(repo,"must-not-start"),/explicit recovery/);
      for (const action of [()=>startRun({task:"must not start"}),
        ()=>continueRun({runId:"must-not-start",prompt:"resume"}),()=>workerRun(repo,"must-not-start"),
        ()=>stopRun("must-not-start"),()=>mergeRun("must-not-start"),()=>syncRun("must-not-start"),
        ()=>deleteRun("must-not-start"),()=>cleanupRuns(),()=>scheduleTick(repo,{}),
        ()=>onRunTransition(repo,"must-not-start",{}),()=>launchNode(repo,{}, {},{}),
        ()=>mergeNodeIntoIntegration(repo,{},{}),()=>reconcileInjection(repo,{prompt:"must not start"},{}),
        ()=>applyOwnedPlanningCommand(repo,{mode:"scheduler"},{kind:"approve",input:{}}),
        ()=>ensureBoardRunning(repo)]) await assert.rejects(action(),/explicit recovery/);
    }
    await fs.rm(marker); await fs.symlink(path.join(repo,"missing"),marker);
    assert.throws(()=>assertLegacyWriterAllowed(repo),/explicit recovery/);
    await fs.rm(marker); await fs.mkdir(marker);
    assert.throws(()=>assertLegacyWriterAllowed(repo),/explicit recovery/);
    await fs.rmdir(marker);
  }
  assert.deepEqual(await fs.readdir(path.join(repo,".rudder")),["engine"]);
  assert.deepEqual(await fs.readdir(engine),[]);
});
