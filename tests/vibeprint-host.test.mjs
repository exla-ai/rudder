// The home's vibeprint canvases (cloud/worker/vibeprint-host.mjs), driven with a
// stand-in for vibeprint's CLI that records how it was started.
import { test, after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { createVibeprintHosts } from "../cloud/worker/vibeprint-host.mjs";
import { createBuildTracker, TRACK_GAP_MS } from "../cloud/worker/vibeprint-track.mjs";

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-vibeprint-"));
after(() => fs.rmSync(tmp, { recursive: true, force: true }));

const TOKEN = "tok_abcdefghijklmnop";
const cli = path.join(tmp, "fake-vibeprint.mjs");
fs.writeFileSync(cli, `
import fs from "node:fs"; import http from "node:http"; import path from "node:path";
const args = process.argv.slice(2); const get = (f) => args[args.indexOf(f) + 1];
const state = get("--state");
fs.mkdirSync(path.join(state, "runtime"), { recursive: true });
fs.writeFileSync(path.join(state, "args.json"), JSON.stringify(args));
if (fs.existsSync(path.join(state, "fail"))) { console.error("vibeprint: the relay refused this session"); process.exit(1); }
const server = http.createServer((req, res) => {
  if (req.headers["x-vibeprint-token"] !== "${TOKEN}") { res.writeHead(401); return res.end("{}"); }
  if (req.url === "/api/track") {
    let body = "";
    req.on("data", (d) => (body += d));
    return req.on("end", () => {
      fs.appendFileSync(path.join(state, "track.jsonl"), body + "\\n");
      res.writeHead(202); res.end(JSON.stringify({ reading: "Claude Code" }));
    });
  }
  const since = new URL(req.url, "http://x").searchParams.get("since") || "";
  res.end(JSON.stringify({ text: "brief since " + since, at: new Date().toISOString(), components: 2 }));
});
server.listen(0, "127.0.0.1", () => setTimeout(() => fs.writeFileSync(path.join(state, "runtime", "daemon.json"), JSON.stringify({
  pid: process.pid, port: server.address().port, token: "${TOKEN}",
  urls: ["https://abc123def456.live.vibeprint.viraat.dev/?t=${TOKEN}", "http://localhost:1/"],
})), 150));
process.on("SIGTERM", () => { fs.rmSync(path.join(state, "runtime", "daemon.json"), { force: true }); process.exit(0); });
`);

const stateRoot = path.join(tmp, "state");
const repo = path.join(tmp, "code", "app");
fs.mkdirSync(path.join(repo, ".git"), { recursive: true });
const project = { workspaceId: "wsp_app", name: "app", dir: repo };
const made = [];
const hostsFor = () => {
  const hosts = createVibeprintHosts({ stateRoot, embedOrigin: "https://rudder.example", env: () => ({ PATH: process.env.PATH }), log: () => {}, cli: () => cli });
  made.push(hosts);
  return hosts;
};
// A failed assertion must not leave a stand-in canvas holding the run open.
after(() => { for (const hosts of made) hosts.stopAll(); });

const until = async (fn, ms = 5000) => {
  const end = Date.now() + ms;
  for (;;) {
    const value = await fn();
    if (value) return value;
    if (Date.now() > end) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 40));
  }
};
const gone = (pid) => { try { process.kill(pid, 0); return false; } catch { return true; } };

test("a canvas starts beside the repo, reports its relay link, and serves the brief", async () => {
  const hosts = hostsFor();
  assert.deepEqual(await hosts.handle(project, { op: "status" }), { state: "off" });
  assert.equal((await hosts.handle(project, { op: "start" })).state, "starting");
  const view = await until(async () => { const v = await hosts.handle(project, { op: "status" }); return v.state === "running" && v; });
  assert.deepEqual(view, { state: "running", url: "https://abc123def456.live.vibeprint.viraat.dev/", token: TOKEN });

  const dir = path.join(stateRoot, "wsp_app");
  const args = JSON.parse(fs.readFileSync(path.join(dir, "args.json"), "utf8"));
  assert.deepEqual(args, ["serve", "--root", repo, "--state", dir, "--host", "127.0.0.1", "--port", "0", "--embed-origin", "https://rudder.example"]);
  // Nothing in the checkout: no rule, MCP config or .vibeprint/ for its agents to see.
  assert.deepEqual(fs.readdirSync(repo), [".git"]);

  const brief = await hosts.handle(project, { op: "brief", since: "2026-11-18T00:00:00.000Z" });
  assert.equal(brief.text, "brief since 2026-11-18T00:00:00.000Z");
  assert.equal(brief.components, 2);

  // Turned on survives a supervisor restart: the next one brings it back.
  const pid = JSON.parse(fs.readFileSync(path.join(dir, "runtime", "daemon.json"), "utf8")).pid;
  hosts.stopAll();
  await until(() => gone(pid));
  const next = hostsFor();
  next.resume([project]);
  await until(async () => (await next.handle(project, { op: "status" })).state === "running");

  assert.deepEqual(await next.handle(project, { op: "stop" }), { state: "off" });
  await until(async () => !fs.existsSync(path.join(dir, "runtime", "daemon.json")));
  const later = hostsFor();
  later.resume([project]);
  assert.deepEqual(await later.handle(project, { op: "status" }), { state: "off" });
});

test("a canvas that exits says why, and starts again only when asked", async () => {
  const failing = { workspaceId: "wsp_fail", name: "fail", dir: repo };
  const dir = path.join(stateRoot, "wsp_fail");
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "fail"), "");
  const hosts = hostsFor();
  await hosts.handle(failing, { op: "start" });
  const view = await until(async () => { const v = await hosts.handle(failing, { op: "status" }); return v.state === "failed" && v; });
  assert.equal(view.error, "vibeprint exited (exit 1): the relay refused this session");
  await assert.rejects(hosts.handle(failing, { op: "brief" }), /not running/);
  await assert.rejects(hosts.handle({ ...failing, workspaceId: "../x" }, { op: "status" }), /not a project id/);
  fs.rmSync(path.join(dir, "fail"));
  await hosts.handle(failing, { op: "start" });
  await until(async () => (await hosts.handle(failing, { op: "status" })).state === "running");
  hosts.stopAll();
});

// ---------- Build tracking ----------

const record = (id, status, extra = {}) => ({ id, status, attention: null, task: "Add an items endpoint", taskSummary: "Items endpoint", ...extra });
const writeRun = (dir, rec) => {
  fs.mkdirSync(path.join(dir, ".rudder", "runs", rec.id), { recursive: true });
  fs.writeFileSync(path.join(dir, ".rudder", "runs", rec.id, "run.json"), JSON.stringify(rec));
};

test("tracking: a turn's end sends its diff once, then at most every 10 minutes", async () => {
  const dir = fs.mkdtempSync(path.join(tmp, "track-unit-"));
  const project = { workspaceId: "wsp_t", name: "t", dir };
  let clock = 1_000_000;
  let diffText = "diff --git a/a.js b/a.js\n+one\n";
  let busy = false;
  const sent = [];
  const tracker = createBuildTracker({
    log: () => {},
    now: () => clock,
    diff: async (cwd) => { assert.equal(cwd, dir, "a main agent's changes are in the checkout"); return diffText; },
    send: async (_p, body) => { if (busy) throw Object.assign(new Error("busy"), { busy: true }); sent.push(body); },
  });
  let tracked = { run_1: { pending: false } };
  const tick = () => tracker.tick(project, tracked, (id, entry) => {
    tracked = { ...tracked };
    if (entry) tracked[id] = entry; else delete tracked[id];
  });

  writeRun(dir, record("run_1", "running"));
  await tick(); // first look: working, nothing ended
  await tick();
  assert.equal(sent.length, 0);
  writeRun(dir, record("run_1", "running", { attention: "permission" }));
  await tick();
  assert.equal(sent.length, 0, "asking to run a command is not the end of a turn");
  writeRun(dir, record("run_1", "running"));
  await tick();
  writeRun(dir, record("run_1", "completed"));
  await tick();
  assert.deepEqual(sent, [{ author: "Items endpoint", task: "Add an items endpoint", diff: diffText }]);
  assert.equal(tracked.run_1.lastAt, clock);
  assert.equal(tracked.run_1.pending, false);

  // Another turn 2 minutes later: due, but not before the 10 minutes are up.
  diffText += "+two\n";
  writeRun(dir, record("run_1", "running"));
  await tick();
  clock += 2 * 60_000;
  writeRun(dir, record("run_1", "completed"));
  await tick();
  assert.equal(sent.length, 1);
  assert.equal(tracked.run_1.pending, true);
  // The canvas is busy when they are: still due, tried again on the next look.
  clock += TRACK_GAP_MS;
  busy = true;
  await tick();
  assert.equal(tracked.run_1.pending, true);
  busy = false;
  await tick();
  assert.equal(sent.length, 2);
  assert.equal(sent[1].diff, diffText, "the diff as it is when the check runs");

  // A turn that changed nothing costs nothing.
  clock += TRACK_GAP_MS;
  writeRun(dir, record("run_1", "running"));
  await tick();
  writeRun(dir, record("run_1", "completed"));
  await tick();
  assert.equal(sent.length, 2);
  assert.equal(tracked.run_1.pending, false);

  // An agent that is gone is no longer tracked.
  fs.rmSync(path.join(dir, ".rudder", "runs", "run_1"), { recursive: true });
  await tick();
  assert.deepEqual(tracked, {});
});

test("tracking: the eye checks an agent's own workspace now, through the running canvas", async () => {
  const dir = path.join(tmp, "code", "tracked");
  fs.mkdirSync(path.join(dir, ".git"), { recursive: true });
  const ws = path.join(tmp, "workspaces", "agent");
  fs.mkdirSync(ws, { recursive: true });
  execFileSync("jj", ["git", "init", "--quiet"], { cwd: ws, stdio: "ignore" });
  fs.writeFileSync(path.join(ws, "items.js"), "export const items = []\n");
  writeRun(dir, record("run_7", "completed", { workspace: { enabled: true, path: ws } }));
  const project = { workspaceId: "wsp_tracked", name: "tracked", dir };
  const hosts = hostsFor();
  await hosts.handle(project, { op: "start" });
  await until(async () => (await hosts.handle(project, { op: "status" })).state === "running");
  assert.deepEqual((await hosts.handle(project, { op: "status", run: "run_7" })).tracking, { on: false });

  const view = await hosts.handle(project, { op: "tracking", run: "run_7", on: true, check: true });
  assert.equal(view.tracking.on, true);
  const log = path.join(stateRoot, "wsp_tracked", "track.jsonl");
  const body = await until(() => fs.existsSync(log) && JSON.parse(fs.readFileSync(log, "utf8").trim().split("\n")[0]));
  assert.equal(body.author, "Items endpoint");
  assert.match(body.diff, /^diff --git a\/items\.js b\/items\.js/m);
  const after = await until(async () => { const t = (await hosts.handle(project, { op: "status", run: "run_7" })).tracking; return t.lastAt && t; });
  assert.equal(after.pending, false);
  // Kept across restarts with the canvas's own settings.
  assert.ok(JSON.parse(fs.readFileSync(path.join(stateRoot, "wsp_tracked", "rudder.json"), "utf8")).tracked.run_7.lastHash);

  // Nothing new: the next check costs nothing. Off forgets the agent.
  await hosts.handle(project, { op: "tracking", run: "run_7", on: true, check: true });
  await until(async () => !(await hosts.handle(project, { op: "status", run: "run_7" })).tracking.pending);
  assert.equal(fs.readFileSync(log, "utf8").trim().split("\n").length, 1);
  assert.deepEqual((await hosts.handle(project, { op: "tracking", run: "run_7", on: false })).tracking, { on: false });
  await assert.rejects(hosts.handle(project, { op: "tracking", run: "../x", on: true }), /not an agent id/);
  hosts.stopAll();
});
