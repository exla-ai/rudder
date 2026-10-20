// The team view and private agents (cloud/src/team.ts), through the REAL
// server: teammates see each other's agents from the in-memory home state, a
// private agent is a muted row with no title and a 404 transcript, outsiders
// see nothing, every agent write stays owner-only, and visibility survives a
// control-plane restart on the same database.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.resolve(here, "..", "cloud");
const cloudReady =
  fs.existsSync(path.join(cloudDir, "dist", "server.js")) &&
  fs.existsSync(path.join(cloudDir, "dist", "team.js")) &&
  fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3")) &&
  fs.existsSync(path.join(cloudDir, "node_modules", "ws"));
const test = cloudReady
  ? nodeTest
  : (name, fn) => nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const require = createRequire(path.join(cloudDir, "package.json"));
const Database = cloudReady ? require("better-sqlite3") : null;
const WebSocket = cloudReady ? require("ws").WebSocket : null;
const team = cloudReady ? await import(path.join(cloudDir, "dist", "team.js")) : null;

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

async function waitFor(fn, { timeout = 10000, interval = 50 } = {}) {
  const start = Date.now();
  for (;;) {
    try {
      const v = await fn();
      if (v) return v;
    } catch {
      // keep polling
    }
    if (Date.now() - start > timeout) throw new Error("waitFor timed out");
    await sleep(interval);
  }
}

// ---- pure rules -----------------------------------------------------------

test("a private agent keeps only its status; a team agent is a whitelist", () => {
  const agent = {
    id: "run-1", title: "Secret plan", task: "the whole task", status: "working", backend: "claude", model: "opus",
    updatedAt: 5, lastMessage: "said something", remoteUrl: "https://claude.ai/x", asks: [{ id: "1-aaaaaaaa", kind: "question", questions: ["?"] }],
  };
  assert.deepEqual(team.teamAgentView(agent, "private"), { id: "run-1", private: true, status: "working", asking: "question", updatedAt: 5 });
  const open = team.teamAgentView(agent, "team");
  assert.equal(open.title, "Secret plan");
  assert.equal(open.model, "opus");
  for (const field of ["task", "lastMessage", "remoteUrl", "asks"]) assert.ok(!(field in open), `${field} never leaves`);
  assert.equal(team.teamAgentView({ title: "no id" }, "team"), null);
});

test("visibility: per agent, and 'new agents here are private' pins only agents seen afterwards", () => {
  const db = new Database(":memory:");
  const store = team.createVisibilityStore(db);
  assert.equal(store.visibilityOf("ws", "a"), "team");
  store.setAgent("ws", "a", "private");
  assert.equal(store.visibilityOf("ws", "a"), "private");
  store.setNewAgentsPrivate("ws", true, ["a", "b"]);
  assert.equal(store.visibilityOf("ws", "a"), "private", "an agent's own setting wins");
  assert.equal(store.visibilityOf("ws", "b"), "team", "known agents stay where they were");
  assert.equal(store.pinNew("ws", ["b", "c"]), 1);
  assert.equal(store.visibilityOf("ws", "c"), "private");
  store.setNewAgentsPrivate("ws", false, []);
  assert.equal(store.visibilityOf("ws", "c"), "private", "pinned agents keep their lock");
  assert.equal(store.visibilityOf("ws", "d"), "team");
  assert.equal(store.pinNew("ws", ["e"]), 0);
  assert.throws(() => store.setAgent("ws", "../x", "private"), /run is required/);
  assert.throws(() => team.validVisibility("public"), /team or private/);
});

test("the voice overview lists teammates' live team agents, never private ones", async () => {
  const voice = await import(path.join(cloudDir, "dist", "voice.js"));
  const text = voice.teamOverviewText([{ name: "Alice", email: null, projects: [{ name: "app", agents: [
    { private: false, title: "Fix login", status: "working" },
    { private: true, status: "working" },
    { private: false, title: "Old thing", status: "merged" },
  ] }] }]);
  assert.match(text, /read only/);
  assert.match(text, /Alice:\n  - Fix login in app: working/);
  assert.ok(!text.includes("Old thing"));
  assert.equal(voice.teamOverviewText([{ name: "Bob", projects: [{ name: "x", agents: [{ private: true, status: "working" }] }] }]), "");
});

// ---- through the server ---------------------------------------------------

function startServer({ port, dataDir, dbPath }) {
  const base = `http://127.0.0.1:${port}`;
  const child = spawn(process.execPath, [path.join(cloudDir, "dist", "server.js")], {
    cwd: cloudDir,
    env: {
      ...process.env,
      NODE_ENV: "test",
      PORT: String(port),
      BETTER_AUTH_URL: base,
      BETTER_AUTH_SECRET: "test-secret-test-secret",
      RUDDER_CLOUD_DATA_DIR: dataDir,
      RUDDER_CLOUD_DB: dbPath,
      RUDDER_CLOUD_PERSIST_STATE: "0",
      RUDDER_S3_BUCKET: "",
      RUDDER_SECRETS_KEY: "",
      RUDDER_SLACK_CLIENT_ID: "",
      RUDDER_LINEAR_CLIENT_ID: "",
      SLACK_BOT_TOKEN: "",
      FLY_API_TOKEN: "",
      RUDDER_DIGEST: "0",
      RUDDER_ANALYTICS_URL: "http://127.0.0.1:9",
      OPENAI_API_KEY: "",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const out = { log: "" };
  child.stdout.on("data", (d) => { out.log += d; });
  child.stderr.on("data", (d) => { out.log += d; });
  return { child, out, base };
}

async function stopServer(child) {
  if (child.exitCode !== null) return;
  const exited = new Promise((r) => child.once("exit", r));
  child.kill("SIGKILL");
  await exited;
}

// A fake supervisor on a home's control socket: records every rpc, answers a
// transcript with that run's messages.
async function connectHome(base, homeId, workerToken, projects) {
  const control = new WebSocket(`${base.replace("http", "ws")}/api/rudder/home/${homeId}/control`, { headers: { authorization: `Bearer ${workerToken}` } });
  const rpcs = [];
  control.on("message", (data) => {
    const message = JSON.parse(data.toString());
    if (message.type !== "rpc") return;
    rpcs.push(message);
    const result = message.method === "transcript"
      ? { title: `title of ${message.params.runId}`, status: "working", backend: "claude", messages: [{ id: "m1", role: "assistant", text: `transcript of ${message.params.runId}` }] }
      : { ok: true };
    control.send(JSON.stringify({ type: "rpc-result", id: message.id, result }));
  });
  await new Promise((resolve, reject) => { control.on("open", resolve); control.on("error", reject); });
  const send = (list) => control.send(JSON.stringify({ type: "state", projects: list }));
  send(projects);
  return { control, rpcs, send, close: () => new Promise((r) => { control.once("close", r); control.close(); }) };
}

test("team view, private agents, outsiders and owner-only writes", { timeout: 60_000 }, async (t) => {
  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-team-"));
  const dbPath = path.join(dataDir, "state.sqlite");
  let server = startServer({ port, dataDir, dbPath });
  const sockets = [];
  t.after(async () => {
    for (const s of sockets) { try { s.control.terminate(); } catch { /* ignore */ } }
    await stopServer(server.child);
    try { fs.rmSync(dataDir, { recursive: true, force: true }); } catch { /* ignore */ }
  });
  const { base } = server;
  await waitFor(async () => (await fetch(`${base}/health`).catch(() => null))?.ok);
  await waitFor(() => fs.existsSync(dbPath));
  await sleep(150);

  // Accounts, and a home machine with one repo for alice and for bob.
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.pragma("journal_mode = WAL");
  const seedToken = db.prepare("insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)");
  for (const who of ["alice", "bob", "eve"]) seedToken.run(tokenHash(`rdr_${who}`), `acct-${who}`, `${who}@example.com`, now, now);
  const insert = db.prepare(`
    insert into rudder_workspaces (id, account_id, workspace_key, repo_name, status, machine_id, worker_token_hash,
      source_kind, repo_url, home_id, created_at, updated_at, last_activity_at)
    values (@id, @account, @key, @name, 'running', @machine, @hash, @kind, @url, @home, @now, @now, @now)
  `);
  for (const who of ["alice", "bob"]) {
    const hash = tokenHash(`rdrw_${who}`);
    insert.run({ id: `home-${who}`, account: `acct-${who}`, key: "home", name: "home", machine: `m-${who}`, hash, kind: "home", url: null, home: null, now });
    insert.run({ id: `repo-${who}`, account: `acct-${who}`, key: `home:${who}/app`, name: "app", machine: null, hash, kind: "home-repo", url: `https://github.com/${who}/app.git`, home: `home-${who}`, now });
  }
  db.close();

  const call = async (who, method, pathname, body) => {
    const res = await fetch(`${base}${pathname}`, {
      method,
      headers: { authorization: `Bearer rdr_${who}`, "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await res.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { /* not json */ }
    return { status: res.status, json, text };
  };
  const ok = async (...args) => {
    const r = await call(...args);
    assert.ok(r.status >= 200 && r.status < 300, `${args[1]} ${args[2]} -> ${r.status} ${r.text}\n${server.out.log.slice(-2000)}`);
    return r.json;
  };

  // alice and bob share an org; eve is in none of it.
  const org = (await ok("alice", "POST", "/api/app/orgs", { name: "Exla" })).org;
  const invite = (await ok("alice", "POST", `/api/app/orgs/${org.id}/invites`, { role: "member" })).invite;
  await ok("bob", "POST", `/api/app/invites/${invite.url.split("/").pop()}/accept`);

  const aliceAgents = [
    { id: "run-pub", title: "Public work", task: "do the public thing", status: "working", backend: "claude", model: "opus", createdAt: 1, updatedAt: 3 },
    { id: "run-priv", title: "Secret plan", task: "the secret task", status: "waiting", backend: "codex", model: "gpt-6.1-sol", createdAt: 1, updatedAt: 2,
      lastMessage: "secret words", asks: [{ id: "1-aaaaaaaa", kind: "permission", tool: "Bash", input: "rm -rf secret" }] },
  ];
  const alice = await connectHome(base, "home-alice", "rdrw_alice", [{ workspaceId: "repo-alice", name: "app", dashboard: "running", agents: aliceAgents }]);
  sockets.push(alice);
  const bob = await connectHome(base, "home-bob", "rdrw_bob", [{ workspaceId: "repo-bob", name: "app", dashboard: "running", agents: [] }]);
  sockets.push(bob);
  await waitFor(async () => (await call("alice", "GET", "/api/app/state")).json?.projects?.[0]?.agents?.length === 2);

  // Every agent starts as team; the owner locks one.
  const own = await ok("alice", "GET", "/api/app/state");
  assert.deepEqual(own.projects[0].agents.map((a) => a.visibility), ["team", "team"]);
  assert.equal(own.projects[0].newAgentsPrivate, false);
  assert.deepEqual(await ok("alice", "POST", "/api/app/visibility", { workspace: "repo-alice", run: "run-priv", visibility: "private" }),
    { workspaceId: "repo-alice", run: "run-priv", visibility: "private" });
  assert.equal((await call("alice", "POST", "/api/app/visibility", { workspace: "repo-alice", run: "run-priv", visibility: "public" })).status, 400);
  const ownAfter = await ok("alice", "GET", "/api/app/state");
  assert.equal(ownAfter.projects[0].agents.find((a) => a.id === "run-priv").visibility, "private");
  assert.equal(ownAfter.projects[0].agents.find((a) => a.id === "run-priv").title, "Secret plan", "the owner still sees everything");

  // bob sees alice and her agents; the private one is only a status.
  const view = await ok("bob", "GET", "/api/app/team");
  assert.deepEqual(view.orgs, [{ id: org.id, name: "Exla" }]);
  assert.equal(view.members.length, 1, "the viewer is not listed");
  const mate = view.members[0];
  assert.equal(mate.accountId, "acct-alice");
  assert.equal(mate.email, "alice@example.com");
  assert.equal(mate.machine, "awake");
  assert.equal(mate.projects.length, 1);
  assert.equal(mate.projects[0].workspaceId, "repo-alice");
  const [privRow, pubRow] = mate.projects[0].agents; // needs-input sorts first
  assert.deepEqual(privRow, { id: "run-priv", private: true, status: "waiting", asking: "permission", updatedAt: 2 });
  assert.equal(pubRow.title, "Public work");
  assert.equal(pubRow.backend, "claude");
  assert.equal(pubRow.model, "opus");
  assert.equal(pubRow.status, "working");
  for (const secret of ["Secret plan", "secret task", "secret words", "rm -rf", "gpt-6.1-sol", "do the public thing"]) {
    assert.ok(!JSON.stringify(view).includes(secret), `team view leaks ${secret}`);
  }
  assert.deepEqual((await ok("bob", "GET", `/api/app/team?org=${org.id}`)).members.map((m) => m.accountId), ["acct-alice"]);
  // alice sees bob, who has no agents yet.
  const aliceView = await ok("alice", "GET", "/api/app/team");
  assert.deepEqual(aliceView.members.map((m) => [m.accountId, m.projects.length]), [["acct-bob", 0]]);

  // Transcripts: a team agent reads for bob (read-only), the private one is 404
  // for bob and 200 for alice.
  const pub = await ok("bob", "GET", "/api/app/team/transcript?member=acct-alice&workspace=repo-alice&run=run-pub");
  assert.equal(pub.readOnly, true);
  assert.equal(pub.messages[0].text, "transcript of run-pub");
  assert.equal(pub.owner.accountId, "acct-alice");
  const privForBob = await call("bob", "GET", "/api/app/team/transcript?member=acct-alice&workspace=repo-alice&run=run-priv");
  assert.equal(privForBob.status, 404);
  assert.ok(!privForBob.text.includes("Secret"));
  const privForAlice = await ok("alice", "GET", "/api/app/team/transcript?member=acct-alice&workspace=repo-alice&run=run-priv");
  assert.equal(privForAlice.readOnly, false);
  assert.equal((await ok("alice", "GET", "/api/app/transcript?workspace=repo-alice&run=run-priv")).messages[0].text, "transcript of run-priv");
  // An agent the machine never reported, or a project that is not hers: 404.
  assert.equal((await call("bob", "GET", "/api/app/team/transcript?member=acct-alice&workspace=repo-alice&run=run-ghost")).status, 404);
  assert.equal((await call("bob", "GET", "/api/app/team/transcript?member=acct-alice&workspace=repo-bob&run=run-pub")).status, 404);
  assert.deepEqual(alice.rpcs.filter((r) => r.method === "transcript").map((r) => r.params.runId), ["run-pub", "run-priv", "run-priv"]);

  // An outsider sees no one and reads nothing.
  assert.deepEqual((await ok("eve", "GET", "/api/app/team")).members, []);
  assert.equal((await call("eve", "GET", `/api/app/team?org=${org.id}`)).status, 404);
  assert.equal((await call("eve", "GET", "/api/app/team/transcript?member=acct-alice&workspace=repo-alice&run=run-pub")).status, 404);
  assert.equal((await call("eve", "POST", "/api/app/visibility", { workspace: "repo-alice", run: "run-pub", visibility: "private" })).status, 404);

  // Every write (and the live terminal) stays the owner's: bob, signed in and
  // with a connected machine of his own, is refused on each of alice's agents,
  // and nothing reaches her machine.
  const rpcsBefore = alice.rpcs.length;
  const ws = "repo-alice";
  const refused = [
    ["POST", "/api/app/send", { workspace: ws, run: "run-pub", text: "hi" }],
    ["POST", "/api/app/task", { workspace: ws, text: "new task" }],
    ["POST", `/api/app/upload?workspace=${ws}&name=a.txt`, "bytes"],
    ["POST", "/api/app/keys", { workspace: ws, run: "run-pub", keys: "esc" }],
    ["POST", "/api/app/answer", { workspace: ws, run: "run-priv", ask: "1-aaaaaaaa", answer: { behavior: "allow" } }],
    ["POST", "/api/app/pin", { workspace: ws, pinned: true }],
    ["POST", "/api/app/action", { workspace: ws, run: "run-pub", action: "stop" }],
    ["POST", "/api/app/action", { workspace: ws, run: "run-pub", action: "merge" }],
    ["POST", "/api/app/action", { workspace: ws, run: "run-pub", action: "delete" }],
    ["POST", "/api/app/action", { workspace: ws, action: "approve" }],
    ["GET", `/api/app/screen?workspace=${ws}&run=run-pub`],
    ["GET", `/api/app/transcript?workspace=${ws}&run=run-pub`],
    ["POST", "/api/app/visibility", { workspace: ws, run: "run-priv", visibility: "team" }],
    ["POST", "/api/app/visibility", { workspace: ws, newAgentsPrivate: false }],
  ];
  for (const [method, route, body] of refused) {
    const r = await call("bob", method, route, body);
    assert.ok(r.status === 403 || r.status === 404, `bob ${method} ${route} -> ${r.status} ${r.text}`);
  }
  assert.equal(alice.rpcs.length, rpcsBefore, `nothing reached alice's machine: ${JSON.stringify(alice.rpcs.slice(rpcsBefore))}`);
  assert.equal((await ok("alice", "GET", "/api/app/state")).projects[0].agents.find((a) => a.id === "run-priv").visibility, "private");

  // "New agents here are private": agents already there keep their setting,
  // the next one appears private.
  assert.deepEqual(await ok("alice", "POST", "/api/app/visibility", { workspace: ws, newAgentsPrivate: true }), { workspaceId: ws, newAgentsPrivate: true });
  alice.send([{ workspaceId: ws, name: "app", dashboard: "running", agents: [
    { id: "run-new", title: "Brand new idea", status: "starting", backend: "claude", createdAt: 9, updatedAt: 9 }, ...aliceAgents,
  ] }]);
  await waitFor(async () => (await call("bob", "GET", "/api/app/team")).json?.members?.[0]?.projects?.[0]?.agents?.length === 3);
  const after = (await ok("bob", "GET", "/api/app/team")).members[0].projects[0].agents;
  assert.deepEqual(Object.fromEntries(after.map((a) => [a.id, a.private])), { "run-new": true, "run-priv": true, "run-pub": false });
  assert.ok(!JSON.stringify(after).includes("Brand new idea"));
  assert.equal((await ok("alice", "GET", "/api/app/state")).projects[0].newAgentsPrivate, true);

  // A sleeping teammate: the last state still shows, a transcript says asleep,
  // and looking never wakes or touches the machine.
  await alice.close();
  await waitFor(async () => (await call("bob", "GET", "/api/app/team")).json?.members?.[0]?.machine === "asleep");
  const lastActivity = () => {
    const check = new Database(dbPath, { readonly: true });
    try { return check.prepare("select last_activity_at from rudder_workspaces where id = 'home-alice'").get().last_activity_at; } finally { check.close(); }
  };
  const activityBefore = lastActivity();
  await sleep(1100); // past touchWorkspaceActivity's 1s throttle, so a touch would show
  const asleep = await call("bob", "GET", "/api/app/team/transcript?member=acct-alice&workspace=repo-alice&run=run-pub");
  assert.equal(asleep.status, 503);
  assert.equal(asleep.json.asleep, true);
  await ok("bob", "GET", "/api/app/team");
  assert.equal(lastActivity(), activityBefore, "a teammate looking never touches the machine");

  // Restart the control plane on the same database: the lock and the project
  // default are still there once the machine reports again.
  await bob.close();
  await stopServer(server.child);
  server = startServer({ port, dataDir, dbPath });
  await waitFor(async () => (await fetch(`${base}/health`).catch(() => null))?.ok);
  const alice2 = await connectHome(base, "home-alice", "rdrw_alice", [{ workspaceId: ws, name: "app", dashboard: "running", agents: [
    { id: "run-later", title: "After restart", status: "working", createdAt: 10, updatedAt: 10 },
    { id: "run-new", title: "Brand new idea", status: "working", createdAt: 9, updatedAt: 9 }, ...aliceAgents,
  ] }]);
  sockets.push(alice2);
  await waitFor(async () => (await call("bob", "GET", "/api/app/team")).json?.members?.[0]?.projects?.[0]?.agents?.length === 4);
  const restarted = (await ok("bob", "GET", "/api/app/team")).members[0].projects[0].agents;
  assert.deepEqual(Object.fromEntries(restarted.map((a) => [a.id, a.private])), { "run-later": true, "run-new": true, "run-priv": true, "run-pub": false });
  assert.equal((await call("bob", "GET", "/api/app/team/transcript?member=acct-alice&workspace=repo-alice&run=run-priv")).status, 404);
  assert.equal((await ok("bob", "GET", "/api/app/team/transcript?member=acct-alice&workspace=repo-alice&run=run-pub")).readOnly, true);
  const ownRestarted = await ok("alice", "GET", "/api/app/state");
  assert.equal(ownRestarted.projects[0].newAgentsPrivate, true);
  assert.equal(ownRestarted.projects[0].agents.find((a) => a.id === "run-priv").visibility, "private");

  // Unlocking works and shows at once.
  await ok("alice", "POST", "/api/app/visibility", { workspace: ws, run: "run-priv", visibility: "team" });
  const unlocked = (await ok("bob", "GET", "/api/app/team")).members[0].projects[0].agents.find((a) => a.id === "run-priv");
  assert.equal(unlocked.title, "Secret plan");
});
