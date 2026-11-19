import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import { build } from "esbuild";

async function load(entry) {
  const result = await build({ entryPoints: [new URL(entry, import.meta.url).pathname], bundle: true, write: false, platform: "node", format: "esm", target: "node22" });
  return import(`data:text/javascript;base64,${Buffer.from(result.outputFiles[0].text).toString("base64")}`);
}
const { agentNotes } = await load("../src/project-files/agent-notes.ts");
const { ProjectFileRoutes } = await load("../cloud/src/project-files/routes.ts");
const sha = (text) => createHash("sha256").update(text).digest("hex");
const read = (dir, run) => agentNotes(dir, run, { version: 1, operation: "read" });
const write = (dir, run, baseRevision, text) => agentNotes(dir, run, { version: 1, operation: "write", baseRevision, text });

/** A project with two agents, each with its run.json. */
async function project(t) {
  const dir = await realpath(await mkdtemp(path.join(os.tmpdir(), "rudder-notes-")));
  t.after(() => rm(dir, { recursive: true, force: true }));
  for (const run of ["run_a", "run_b"]) {
    await mkdir(path.join(dir, ".rudder", "runs", run), { recursive: true });
    await writeFile(path.join(dir, ".rudder", "runs", run, "run.json"), "{}\n");
  }
  return dir;
}

test("each agent has its own notes in its run folder, saved by revision", async (t) => {
  const dir = await project(t);
  const file = path.join(dir, ".rudder", "runs", "run_a", "notes.md");

  // No file yet: an empty editor, created on the first save.
  assert.deepEqual(await read(dir, "run_a"), { text: "", revision: null });
  assert.deepEqual(await write(dir, "run_a", null, "first\n"), { outcome: "saved", revision: sha("first\n") });
  assert.equal(await readFile(file, "utf8"), "first\n");

  // Another agent's notes are its own.
  assert.deepEqual(await read(dir, "run_b"), { text: "", revision: null });
  await write(dir, "run_b", null, "other agent\n");
  assert.equal((await read(dir, "run_a")).text, "first\n");

  // A save needs the revision it was loaded at: a second device's edit is a conflict, not overwritten.
  assert.deepEqual(await write(dir, "run_a", sha("first\n"), "from the phone\n"), { outcome: "saved", revision: sha("from the phone\n") });
  assert.deepEqual(await write(dir, "run_a", sha("first\n"), "from the laptop\n"), { outcome: "conflict", text: "from the phone\n", revision: sha("from the phone\n") });
  assert.equal((await write(dir, "run_a", null, "also new\n")).outcome, "conflict");
  assert.equal(await readFile(file, "utf8"), "from the phone\n");

  // Notes belong to an agent that exists, by a plain run id.
  await assert.rejects(read(dir, "run_gone"), /That agent is not on your cloud machine/);
  await rm(path.join(dir, ".rudder", "runs", "run_b"), { recursive: true });
  await assert.rejects(write(dir, "run_b", null, "x"), /That agent is not on your cloud machine/);
  for (const bad of ["..", ".", "a/b", "", "run a"]) await assert.rejects(read(dir, bad), /Not an agent id/);
});

test("the Cloud notes route carries the home's answers for one agent, and a sleeping home stays a 503 the app can tell apart", async (t) => {
  const dir = await project(t);
  let home = (params) => agentNotes(dir, params.runId, params.request);
  const routes = new ProjectFileRoutes({
    workspace: (_account, workspaceId) => {
      if (workspaceId !== "wsp_app") throw Object.assign(new Error("project not found"), { status: 404 });
      return { homeId: "home_1" };
    },
    ownHome: () => "home_1",
    rpc: async (homeId, method, params) => {
      assert.deepEqual([homeId, method, params.workspaceId], ["home_1", "agentNotes", "wsp_app"]);
      try { return JSON.parse(JSON.stringify(await home(params))); }
      catch (error) { throw Object.assign(new Error(error.message), { status: error.status ?? 400, ...(error.needsUpdate ? { needsUpdate: true } : {}) }); }
    },
  });
  const server = createServer((req, res) => { void routes.handle(req, res, new URL(req.url, "http://cloud"), "account_1"); });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const base = `http://127.0.0.1:${server.address().port}/api/app/agent-notes`;
  const call = async (query, body) => {
    const res = await fetch(body ? base : `${base}?${new URLSearchParams(query)}`, body ? { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) } : {});
    return { status: res.status, body: await res.json() };
  };

  // What notes.js sends, end to end.
  assert.deepEqual(await call({ workspaceId: "wsp_app", runId: "run_a" }), { status: 200, body: { text: "", revision: null } });
  assert.deepEqual(await call(null, { version: 1, workspaceId: "wsp_app", runId: "run_a", operation: "write", baseRevision: null, text: "hi\n" }),
    { status: 200, body: { outcome: "saved", revision: sha("hi\n") } });
  assert.deepEqual(await call({ workspaceId: "wsp_app", runId: "run_a" }), { status: 200, body: { text: "hi\n", revision: sha("hi\n") } });
  assert.deepEqual(await call({ workspaceId: "wsp_app", runId: "run_b" }), { status: 200, body: { text: "", revision: null } });

  // A request names one agent.
  assert.equal((await call({ workspaceId: "wsp_app" })).status, 400);
  assert.equal((await call(null, { version: 1, workspaceId: "wsp_app", runId: "../x", operation: "write", baseRevision: null, text: "" })).status, 400);

  // Errors keep their own status: HomeControlSessions answers a home's refusal with 400, homeRpc a sleeping home with 503.
  assert.deepEqual(await call({ workspaceId: "wsp_other", runId: "run_a" }), { status: 404, body: { error: "project not found" } });
  assert.deepEqual(await call({ workspaceId: "wsp_app", runId: "run_gone" }), { status: 400, body: { error: "That agent is not on your cloud machine" } });
  // A home before agentNotes: server.ts homeNeedsUpdate's error keeps needsUpdate, so the app offers Update now.
  home = () => { throw Object.assign(new Error("Your cloud machine runs Rudder 5.0.10, which does not have this yet."), { status: 400, needsUpdate: true }); };
  assert.deepEqual(await call({ workspaceId: "wsp_app", runId: "run_a" }),
    { status: 400, body: { error: "Your cloud machine runs Rudder 5.0.10, which does not have this yet.", needsUpdate: true } });
  home = () => { throw Object.assign(new Error("the cloud machine is not connected yet; it may still be starting"), { status: 503 }); };
  assert.deepEqual(await call(null, { version: 1, workspaceId: "wsp_app", runId: "run_a", operation: "write", baseRevision: sha("hi\n"), text: "more\n" }),
    { status: 503, body: { error: "the cloud machine is not connected yet; it may still be starting" } });
});
