// The morning digest: the home machine's reader (cloud/worker/digest.mjs) and
// the control plane's scheduling + grouping (cloud/src/digest.ts).
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { collectDigestRuns, digestRun } from "../cloud/worker/digest.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.join(path.resolve(here, ".."), "cloud");
const built = fs.existsSync(path.join(cloudDir, "dist", "digest.js"));
const test = built ? nodeTest : (name, fn) => nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const lib = built ? await import(path.join(cloudDir, "dist", "digest.js")) : null;

const H = 3600_000;
const UNTIL = Date.parse("2026-10-20T16:01:00Z"); // 9:01 in Los Angeles
const SINCE = UNTIL - 24 * H;

function repo(records) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-digest-"));
  for (const record of records) {
    const runDir = path.join(dir, ".rudder", "runs", record.id);
    fs.mkdirSync(runDir, { recursive: true });
    fs.writeFileSync(path.join(runDir, "run.json"), JSON.stringify(record));
  }
  return dir;
}

nodeTest("a run that did nothing in the window is left out", () => {
  const old = { id: "r1", status: "completed", createdAt: SINCE - 48 * H, updatedAt: SINCE - 30 * H, turns: [] };
  assert.equal(digestRun(old, { since: SINCE, until: UNTIL }), null);
});

nodeTest("a run's activity becomes ordered events with what it shipped", () => {
  const record = {
    id: "r2", taskSummary: "Fix the reaper", task: "fix it", status: "merged", backend: "claude", model: "opus",
    createdAt: String(SINCE + 2 * H), updatedAt: String(SINCE + 6 * H),
    doneSummary: "Reaper retries every 30s.",
    merge: { status: "merged", pushed: true, pushedAt: SINCE + 6 * H - 1000, targetBranch: "main" },
    publish: { url: "https://github.com/o/r/pull/7", number: 7 },
    delivery: { status: "deployed", target: "fly", verifiedAt: new Date(SINCE + 5 * H).toISOString() },
    tokens: { input: 100, output: 20 },
    turns: [{ prompt: "fix it", ts: SINCE + 2 * H }, { prompt: "also add a test", ts: SINCE + 3 * H }],
  };
  const run = digestRun(record, { since: SINCE, until: UNTIL });
  assert.deepEqual(run.events.map((e) => e.kind), ["started", "message", "deployed", "pushed", "merged"]);
  assert.equal(run.events[1].text, "also add a test");
  assert.equal(run.title, "Fix the reaper");
  assert.equal(run.summary, "Reaper retries every 30s.");
  assert.equal(run.merged, true);
  assert.equal(run.pushed, true);
  assert.deepEqual(run.pr, { url: "https://github.com/o/r/pull/7", number: 7 });
  assert.equal(run.tokens, 120);
});

nodeTest("collectDigestRuns reads every run.json and skips broken ones", () => {
  const dir = repo([
    { id: "a", status: "running", createdAt: SINCE + H, updatedAt: SINCE + 2 * H, turns: [] },
    { id: "b", status: "failed", createdAt: SINCE + 3 * H, updatedAt: SINCE + 4 * H, turns: [] },
    { id: "c", status: "completed", createdAt: SINCE - 90 * H, updatedAt: SINCE - 80 * H, turns: [] },
  ]);
  fs.mkdirSync(path.join(dir, ".rudder", "runs", "broken"));
  fs.writeFileSync(path.join(dir, ".rudder", "runs", "broken", "run.json"), "{not json");
  const runs = collectDigestRuns(dir, { since: SINCE, until: UNTIL });
  assert.deepEqual(runs.map((r) => [r.id, r.status]), [["a", "working"], ["b", "failed"]]);
  assert.deepEqual(collectDigestRuns(path.join(dir, "missing"), { since: SINCE, until: UNTIL }), []);
});

test("the digest is due once the account's clock passes 9:01, once a day", () => {
  const tz = "America/Los_Angeles";
  assert.deepEqual(lib.zonedClock(UNTIL, tz), { day: "2026-10-20", hour: 9, minute: 1 });
  assert.equal(lib.digestDueDay(UNTIL - 60_000, tz, "2026-10-19"), null);
  assert.equal(lib.digestDueDay(UNTIL, tz, "2026-10-19"), "2026-10-20");
  assert.equal(lib.digestDueDay(UNTIL + 3 * H, tz, "2026-10-20"), null);
  assert.equal(lib.digestDueDay(UNTIL, tz, null), "2026-10-20");
  // 16:01 UTC is already past 9:01 in New York, not yet in Tokyo's next morning.
  assert.equal(lib.digestDueDay(UNTIL, "Asia/Tokyo", "2026-10-20"), null);
  assert.equal(lib.validTimeZone("Not/AZone"), null);
  assert.equal(lib.validTimeZone("Europe/Paris"), "Europe/Paris");
});

test("each digest picks up where the last one ended, at most a week back", () => {
  assert.deepEqual(lib.digestWindow(UNTIL, UNTIL - 20 * H), { since: UNTIL - 20 * H, until: UNTIL });
  assert.deepEqual(lib.digestWindow(UNTIL, null), { since: UNTIL - 24 * H, until: UNTIL });
  assert.deepEqual(lib.digestWindow(UNTIL, UNTIL - 30 * 24 * H), { since: UNTIL - 24 * H, until: UNTIL });
});

test("composeDigest groups runs by project with totals, a timeline and a headline", () => {
  const mk = (id, status, at, extra = {}) => ({ id, title: id, status, firstAt: at, lastAt: at + H, events: [{ at, kind: "started" }, { at: at + H, kind: status === "failed" ? "failed" : "finished" }], ...extra });
  const digest = lib.composeDigest([
    { workspaceId: "w1", name: "rudder", repo: "exla-ai/rudder", runs: [mk("a", "done", SINCE + H), mk("b", "review", SINCE + 2 * H, { pr: { url: "u", number: 1 } })] },
    { workspaceId: "w2", name: "wager", runs: [mk("c", "failed", SINCE + 3 * H)] },
    { workspaceId: "w3", name: "quiet", runs: [] },
  ], { day: "2026-10-20", timeZone: "America/Los_Angeles", since: SINCE, until: UNTIL, generatedAt: UNTIL });
  assert.deepEqual(digest.projects.map((p) => p.name), ["rudder", "wager"]);
  assert.equal(digest.totals.agents, 3);
  assert.equal(digest.totals.finished, 2);
  assert.equal(digest.totals.prs, 1);
  assert.equal(digest.totals.needsYou, 1);
  assert.equal(digest.headline, "3 agents worked across 2 projects: 2 finished, 1 PR.");
  assert.deepEqual(digest.lines, ["1 agent needs you.", "1 agent failed."]);
  const tl = digest.projects[0].timeline;
  assert.deepEqual(tl.map((e) => [e.runId, e.kind]), [["a", "started"], ["a", "finished"], ["b", "started"], ["b", "finished"]]);
  assert.deepEqual(lib.digestPreview(digest).projects, ["rudder", "wager"]);
  const empty = lib.composeDigest([], { day: "2026-10-20", timeZone: "UTC", since: SINCE, until: UNTIL });
  assert.match(empty.headline, /quiet/);
  assert.equal(empty.projects.length, 0);
});

// ---------------------------------------------------------------------------
// End to end: the real control plane writes the digest from a fake home
// machine that answers `digest` with the real reader over real run records.

import net from "node:net";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";

const serverBuilt = built && fs.existsSync(path.join(cloudDir, "dist", "server.js")) && fs.existsSync(path.join(cloudDir, "node_modules", "ws"));
const serverTest = serverBuilt ? nodeTest : (name, fn) => nodeTest(name, { skip: "cloud not built" }, fn);
const tokenHash = (token) => createHash("sha256").update(token).digest("hex").slice(0, 32);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

async function waitFor(fn, timeout = 10000) {
  const start = Date.now();
  for (;;) {
    try {
      const v = await fn();
      if (v) return v;
    } catch {
      // keep polling
    }
    if (Date.now() - start > timeout) throw new Error("waitFor timed out");
    await sleep(100);
  }
}

// A fixed-offset zone whose clock reads between 10:00 and 20:00 right now, so
// the 9:01 digest is due whenever the test runs.
function zonePastNineOhOne() {
  for (let offset = -12; offset <= 14; offset += 1) {
    const name = offset === 0 ? "Etc/GMT" : `Etc/GMT${offset > 0 ? "-" : "+"}${Math.abs(offset)}`;
    const hour = lib.zonedClock(Date.now(), name).hour;
    if (hour >= 10 && hour < 20) return name;
  }
  throw new Error("no zone found");
}

serverTest("the control plane writes the 9:01 digest from the home machine and serves it to the app", async (t) => {
  const { WebSocket } = await import(path.join(cloudDir, "node_modules", "ws", "wrapper.mjs"));
  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-digest-test-"));
  const dbPath = path.join(dataDir, "state.sqlite");
  const base = `http://127.0.0.1:${port}`;
  const child = spawn(process.execPath, [path.join(cloudDir, "dist", "server.js")], {
    cwd: cloudDir,
    env: {
      ...process.env,
      PORT: String(port),
      BETTER_AUTH_URL: base,
      BETTER_AUTH_SECRET: "test-secret-test-secret",
      RUDDER_CLOUD_DATA_DIR: dataDir,
      RUDDER_CLOUD_DB: dbPath,
      RUDDER_CLOUD_PERSIST_STATE: "0",
      RUDDER_S3_BUCKET: "",
      SLACK_BOT_TOKEN: "",
      FLY_API_TOKEN: "",
      RUDDER_DIGEST_SWEEP_MS: "1000",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  child.stdout.on("data", (d) => { log += d; });
  child.stderr.on("data", (d) => { log += d; });
  t.after(() => {
    child.kill("SIGKILL");
    fs.rmSync(dataDir, { recursive: true, force: true });
  });
  await waitFor(async () => (await fetch(`${base}/health`).catch(() => null))?.ok);
  await waitFor(() => fs.existsSync(dbPath));
  await sleep(150);

  const require = createRequire(path.join(cloudDir, "package.json"));
  const Database = require("better-sqlite3");
  const cliToken = "rdr_digest_test_cli";
  const workerToken = "rdrw_digest_test_worker";
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.prepare("insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)")
    .run(tokenHash(cliToken), "acct-digest", "me@example.com", now, now);
  const insert = db.prepare(`
    insert into rudder_workspaces (id, account_id, workspace_key, repo_name, status, machine_id, worker_token_hash,
      source_kind, repo_url, home_id, created_at, updated_at, last_activity_at)
    values (@id, 'acct-digest', @key, @name, 'running', @machine, @hash, @kind, @url, @home, @now, @now, @now)
  `);
  insert.run({ id: "home-dg", key: "home", name: "home", machine: "m-home", hash: tokenHash(workerToken), kind: "home", url: null, home: null, now });
  insert.run({ id: "rudder-dg", key: "home:exla-ai/rudder", name: "rudder", machine: null, hash: tokenHash(workerToken), kind: "home-repo", url: "https://github.com/exla-ai/rudder.git", home: "home-dg", now });
  db.close();

  const t0 = Date.now();
  const repoDir = repo([
    { id: "run-ship", taskSummary: "Ship the reaper", status: "completed", lifecyclePhase: "deployed", createdAt: t0 - 5 * H, updatedAt: t0 - 4 * H,
      doneSummary: "Deployed the reaper.", delivery: { status: "deployed", target: "fly", verifiedAt: new Date(t0 - 4 * H).toISOString() }, turns: [] },
    { id: "run-wip", taskSummary: "Phone digest", status: "running", createdAt: t0 - 2 * H, updatedAt: t0 - 60_000, turns: [] },
  ]);
  const control = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/home/home-dg/control`, { headers: { authorization: `Bearer ${workerToken}` } });
  t.after(() => control.close());
  const digestCalls = [];
  control.on("message", (data) => {
    const message = JSON.parse(data.toString());
    if (message.type !== "rpc") return;
    let result = { ok: true };
    if (message.method === "digest") {
      digestCalls.push(message.params);
      result = { projects: [{ workspaceId: "rudder-dg", name: "rudder", runs: collectDigestRuns(repoDir, message.params) }] };
    }
    control.send(JSON.stringify({ type: "rpc-result", id: message.id, result }));
  });
  await new Promise((resolve, reject) => { control.on("open", resolve); control.on("error", reject); });

  const auth = { authorization: `Bearer ${cliToken}`, "content-type": "application/json" };
  const tz = zonePastNineOhOne();
  // The phone reports its zone on every state poll; no digest exists yet.
  const first = await (await fetch(`${base}/api/app/state`, { headers: { ...auth, "x-rudder-timezone": tz } })).json();
  assert.equal(first.digest ?? null, null);

  const list = await waitFor(async () => {
    const body = await (await fetch(`${base}/api/app/digests`, { headers: auth })).json();
    return body.digests?.length ? body : null;
  }).catch((error) => { throw new Error(`${error.message}\n${log}`); });
  assert.equal(list.timeZone, tz);
  assert.equal(list.digests.length, 1);
  const preview = list.digests[0];
  assert.equal(preview.day, lib.zonedClock(Date.now(), tz).day);
  assert.equal(preview.headline, "2 agents worked in rudder: 1 finished, 1 deployed.");
  assert.deepEqual(preview.projects, ["rudder"]);
  assert.equal(preview.readAt, null);
  assert.equal(digestCalls.length, 1, "one digest per day, however often the sweep runs");
  assert.ok(Math.abs(digestCalls[0].until - digestCalls[0].since - 24 * H) < 5000);

  const state = await (await fetch(`${base}/api/app/state`, { headers: auth })).json();
  assert.equal(state.digest.id, preview.id);

  const full = await (await fetch(`${base}/api/app/digest?id=${preview.id}`, { headers: auth })).json();
  assert.equal(full.projects[0].name, "rudder");
  assert.equal(full.projects[0].repo, "exla-ai/rudder");
  assert.deepEqual(full.projects[0].runs.map((r) => r.id), ["run-wip", "run-ship"]);
  assert.ok(full.projects[0].timeline.length >= 3);

  assert.equal((await fetch(`${base}/api/app/digest/read`, { method: "POST", headers: auth, body: JSON.stringify({ id: preview.id }) })).status, 200);
  const read = await (await fetch(`${base}/api/app/digest`, { headers: auth })).json();
  assert.ok(read.readAt);

  // "So far today": not stored, starts where the kept one ended.
  const live = await (await fetch(`${base}/api/app/digest?live=1`, { headers: auth })).json();
  assert.equal(live.live, true);
  assert.equal(live.since, full.until);
  assert.equal((await fetch(`${base}/api/app/digests`, { headers: auth }).then((r) => r.json())).digests.length, 1);

  // Another account sees none of it.
  assert.equal((await fetch(`${base}/api/app/digest?id=${preview.id}`)).status, 401);
});
