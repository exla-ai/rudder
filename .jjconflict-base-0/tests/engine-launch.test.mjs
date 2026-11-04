import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { ensureEngineRunning, parseEngineStatus, persistentEngineRequested, readEngineStatus, shutdownEngine } from "../dist/engine.js";

const ready = (repo) => ({ format_version: 1, running: true, repo, authority: "fabric", state: "ready", owner: { project_id: "project", owner_id: "owner", epoch: 4 }, instance_id: "instance-one", attach_socket: `${repo}/.rudder/attach.sock` });

async function fixture(t, mode = "ready") {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder engine launcher "));
  t.after(() => fsp.rm(root, { force: true, recursive: true }));
  const binary = path.join(root, "native fixture");
  await fsp.writeFile(binary, `#!/usr/bin/env node
import fs from 'node:fs';
const args=process.argv.slice(2);
const repo=args[args.indexOf('--repo')+1];
const mode=process.env.RUDDER_ENGINE_FIXTURE;
const state={format_version:1,running:true,repo,authority:'fabric',state:'ready',owner:{project_id:'project',owner_id:'owner',epoch:4},instance_id:'instance-one',attach_socket:repo+'/.rudder/attach.sock'};
if(mode==='legacy') state.authority='legacy-native';
if(mode==='mismatch') state.repo=process.env.RUDDER_ENGINE_OTHER_REPO;
if(args[1]==='shutdown') fs.writeFileSync(repo+'/shutdown-requested','yes');
if(mode==='changed'&&args[1]==='status'&&fs.existsSync(repo+'/shutdown-requested')) state.instance_id='instance-two';
if(mode==='absent') {state.running=false;state.state='not-running';}
if(args[1]!=='status'&&args[1]!=='shutdown') {fs.writeFileSync(repo+'/start-attempted','yes');process.exit(1);}
console.log(JSON.stringify(state));
`, { mode: 0o755 });
  // The fixture's nearest package.json selects ESM even with spaces in the path.
  await fsp.writeFile(path.join(root, "package.json"), '{"type":"module"}');
  return { root, options: { nativeBinary: binary, env: { ...process.env, RUDDER_ENGINE_FIXTURE: mode }, timeoutMs: 2_500 } };
}

test("engine status requires a versioned live owner before treating it as ready", () => {
  assert.equal(parseEngineStatus(JSON.stringify(ready("/repo"))).owner.epoch, 4);
  for (const status of [null, {}, { ...ready("/repo"), format_version: 2 }, { ...ready("/repo"), owner: null }, { ...ready("/repo"), owner: { project_id: "p", owner_id: "o", epoch: 1.5 } }, { ...ready("/repo"), instance_id: null }]) {
    assert.throws(() => parseEngineStatus(JSON.stringify(status)));
  }
});

test("an existing engine is reused without starting another scheduler", async (t) => {
  const { root, options } = await fixture(t);
  const status = await ensureEngineRunning(root, options);
  assert.equal(status.instance_id, "instance-one");
  await assert.rejects(fsp.stat(path.join(root, "start-attempted")), { code: "ENOENT" });
});

test("launcher refuses to take ownership from a live legacy dashboard", async (t) => {
  const { root, options } = await fixture(t, "legacy");
  await assert.rejects(ensureEngineRunning(root, options), /legacy-native.*still running/);
  await assert.rejects(fsp.stat(path.join(root, "start-attempted")), { code: "ENOENT" });
});

test("checkout aliases match, but a response for another repo is rejected", async (t) => {
  const { root, options } = await fixture(t, "mismatch");
  const other = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-other-engine-"));
  t.after(() => fsp.rm(other, { recursive: true, force: true }));
  options.env.RUDDER_ENGINE_OTHER_REPO = other;
  await assert.rejects(readEngineStatus(root, options), /different checkout/);
});

test("shutdown fails honestly if the owner changes before stop confirmation", async (t) => {
  const { root, options } = await fixture(t, "changed");
  await assert.rejects(shutdownEngine(root, options), /ownership changed/);
});

test("unsuccessful detached startup has a bounded readiness wait", async (t) => {
  const { root, options } = await fixture(t, "absent");
  await assert.rejects(ensureEngineRunning(root, options), /did not become ready/);
  assert.equal(await fsp.readFile(path.join(root, "start-attempted"), "utf8"), "yes");
});

test("engine authority migration is explicit until all legacy writers are fenced", () => {
  assert.equal(persistentEngineRequested({}), false);
  assert.equal(persistentEngineRequested({ RUDDER_ENGINE: "fabric" }), true);
  assert.equal(persistentEngineRequested({ RUDDER_ENGINE: "legacy-native" }), false);
  assert.throws(() => persistentEngineRequested({ RUDDER_ENGINE: "mistyped" }), /must be/);
});
