import "./board-source.mjs";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import http from "node:http";
import test from "node:test";
const { startBoardDaemon } = await import("../src/board/daemon.ts");
const { requestBoardBootstrap } = await import("../src/board/host-access.ts");
const { registerProject } = await import("../src/state.ts");
const { RudderBus } = await import("../src/bus.ts");
const { BrowserBoardAccess } = await import("../src/board/ui/access.ts");

async function fixture(t) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "rudder-board-cap-"));
  const previous = process.env.RUDDER_HOME;
  process.env.RUDDER_HOME = path.join(root, "home");
  const repo = path.join(root, "repo"); await fs.mkdir(repo);
  const project = await registerProject(repo);
  const owned = path.join(repo, ".rudder", "runs", "peer");
  await fs.mkdir(owned, { recursive: true });
  await fs.writeFile(path.join(owned, "run.json"), JSON.stringify({ id: "peer", task: "Private peer task", taskSummary: "Peer", taskSummaryLlm: true,
    repoRoot: repo, status: "completed", backend: "codex", vcs: "jj", createdAt: "2026-10-30T00:00:00Z", updatedAt: "2026-10-30T00:00:00Z",
    targetBranch: "main", baseCommit: "baseline", workspace: { enabled: false, path: repo }, autoSteer: { count: 0, max: 0 } }));
  await fs.writeFile(path.join(owned, "output.txt"), "Private peer output\n");
  const server = await startBoardDaemon({ port: 0, repoRoot: repo, controlMode: "projector", bus: new RudderBus() });
  t.after(async () => {
    await server.close();
    if (previous === undefined) delete process.env.RUDDER_HOME; else process.env.RUDDER_HOME = previous;
    await fs.rm(root, { recursive: true, force: true });
  });
  return { root, repo, project, server };
}

async function exchange(server, bootstrap, extraHeaders = {}) {
  return fetch(server.url + "/api/board/session", { method: "POST", headers: { "content-type": "application/json", ...extraHeaders },
    body: JSON.stringify({ ticket: new URL(bootstrap).hash.split("=")[1], instance_id: server.instanceId }) });
}
async function headersFor(repo, server) {
  const reply = await exchange(server, await requestBoardBootstrap(repo));
  assert.equal(reply.status, 200); return { Authorization: `Bearer ${(await reply.json()).token}` };
}
function storage() {
  const values = new Map();
  return { get length() { return values.size; }, key: (i) => [...values.keys()][i] ?? null,
    getItem: (key) => values.get(key) ?? null, setItem: (key, value) => values.set(key, value), removeItem: (key) => values.delete(key) };
}

test("anonymous shell contains no authority or untrusted inline HTML and all private HTTP reads/writes fail", { timeout: 20_000 }, async (t) => {
  const { server, repo, project } = await fixture(t);
  const response = await fetch(server.url + "/rudder/" + encodeURIComponent('</script><script>alert(1)</script>'));
  const html = await response.text();
  assert.doesNotMatch(html, /__RUDDER_TOKEN__|Private peer|alert\(1\)|rudder-board-ticket|Bearer /);
  assert.doesNotMatch(html, /<script(?:\s[^>]*)?>[^<]+/);
  assert.match(response.headers.get("content-security-policy"), /script-src 'self';/);
  assert.equal(response.headers.get("set-cookie"), null);
  assert.equal(response.headers.get("referrer-policy"), "no-referrer");
  const descriptor = await fetch(server.url + "/api/board/descriptor").then((r) => r.json());
  assert.deepEqual(Object.keys(descriptor).sort(), ["format_version", "instance_id", "origin"]);
  for (const suffix of ["", "/state", "/events", "/tasks/peer/log", "/steer-receipts/request-0001"]) {
    const route = suffix ? `/api/projects/${project.slug}${suffix}` : "/api/projects";
    const denied = await fetch(server.url + route); assert.equal(denied.status, 401, route);
    assert.doesNotMatch(await denied.text(), /Private peer/);
  }
  for (const route of ["/api/usage/tokens", "/api/usage/quota?refresh=1", "/api/usage/machine", "/api/board/session"]) {
    assert.equal((await fetch(server.url + route)).status, 401, route);
  }
  for (const suffix of ["/tasks", "/tasks/peer/steer", "/tasks/peer/merge", "/tasks/peer/cancel", "/nudge-all", "/steer"]) {
    const denied = await fetch(`${server.url}/api/projects/${project.slug}${suffix}`, { method: "POST",
      headers: { "x-rudder-token": "a".repeat(64), "content-type": "application/json" }, body: '{"prompt":"untrusted","instruction":"untrusted"}' });
    assert.equal(denied.status, 401, suffix);
  }
  assert.equal((await fetch(server.url + "/api/board/bootstrap", { method: "POST" })).status, 401);
  await assert.rejects(fs.stat(path.join(repo, ".rudder", "steer")), { code: "ENOENT" });
});

test("one-use private bootstrap preserves authorized reads/control and Host/Origin/project boundaries", { timeout: 20_000 }, async (t) => {
  const { server, repo, project, root } = await fixture(t);
  const bootstrap = await requestBoardBootstrap(repo);
  assert.equal((await exchange(server, bootstrap, { Origin: "http://127.0.0.1:1" })).status, 403);
  const exchanged = await exchange(server, bootstrap, { Origin: server.url }); assert.equal(exchanged.status, 200);
  const body = await exchanged.json(); assert.equal(body.session.owner_slug, project.slug);
  assert.equal((await exchange(server, bootstrap)).status, 401);
  const headers = { Authorization: `Bearer ${body.token}` };
  const state = await fetch(`${server.url}/api/projects/${project.slug}/state`, { headers }).then((r) => r.json());
  assert.ok(state.nodes.some((node) => node.id === "peer"));
  assert.match(await fetch(`${server.url}/api/projects/${project.slug}/tasks/peer/log`, { headers }).then((r) => r.text()), /Private peer output/);
  assert.equal((await fetch(server.url + "/api/board/session/renew", { method: "POST", headers })).status, 200);
  assert.equal((await fetch(server.url + "/api/projects", { headers: { ...headers, Origin: "http://localhost:1" } })).status, 403);
  const wrongHost = await new Promise((resolve, reject) => {
    const req = http.get(server.url + "/api/projects", { headers: { ...headers, Host: "evil.test" } }, (res) => { res.resume(); resolve(res.statusCode); });
    req.on("error", reject);
  });
  assert.equal(wrongHost, 403);
  const queued = await fetch(`${server.url}/api/projects/${project.slug}/tasks`, { method: "POST",
    headers: { ...headers, "content-type": "application/json" }, body: '{"prompt":"Authorized task","requestId":"authorized-task-0001"}' });
  assert.equal(queued.status, 202);
  assert.equal((await fs.readdir(path.join(repo, ".rudder", "steer"))).filter((name) => name.endsWith(".json")).length, 1);
  const otherRepo = path.join(root, "other"); await fs.mkdir(otherRepo); const other = await registerProject(otherRepo);
  assert.equal((await fetch(`${server.url}/api/projects/${other.slug}/state`, { headers })).status, 200);
  assert.equal((await fetch(`${server.url}/api/projects/${other.slug}/tasks`, { method: "POST", headers, body: '{"prompt":"Must not run"}' })).status, 409);
  const otherServer = await startBoardDaemon({ port: 0, repoRoot: otherRepo, controlMode: "projector" });
  try { assert.equal((await fetch(otherServer.url + "/api/projects", { headers })).status, 401); } finally { await otherServer.close(); }
});

test("browser exchanges the fragment privately, stores by origin/instance, survives reload and clears stale state", { timeout: 20_000 }, async (t) => {
  const { server, repo, project, root } = await fixture(t);
  const saved = storage(); const requests = []; let erased = false;
  const env = { origin: server.url, href: await requestBoardBootstrap(repo), storage: saved,
    replaceUrl: (value) => { erased = true; assert.ok(!value.includes("ticket")); },
    fetch: (url, init) => { assert.ok(erased); requests.push({ url, init }); return fetch(url, init); } };
  const browser = new BrowserBoardAccess(env); t.after(() => browser.close());
  const info = await browser.initialize(); assert.equal(info.owner_slug, project.slug);
  assert.equal((await browser.fetch("/api/projects")).status, 200);
  assert.ok(requests.every((r) => r.init.credentials === "omit" && r.init.redirect === "error"));
  const count = requests.length; await assert.rejects(browser.fetch("https://elsewhere.invalid/api/projects")); assert.equal(requests.length, count);
  const reloaded = new BrowserBoardAccess({ ...env, href: server.url + "/usage" }); t.after(() => reloaded.close());
  await reloaded.initialize(); assert.equal((await reloaded.fetch("/api/usage/machine")).status, 200);
  for (const name of await fs.readdir(path.join(root, "home", "board"))) if (name.endsWith(".json")) {
    const file = path.join(root, "home", "board", name);
    assert.equal((await fs.stat(file)).mode & 0o077, 0); assert.doesNotMatch(await fs.readFile(file, "utf8"), /ticket|Bearer|token/);
  }
  const restarted = new BrowserBoardAccess({ ...env, href: server.url + "/rudder", fetch: async () => new Response(JSON.stringify({
    format_version: 1, origin: server.url, instance_id: "11111111-1111-4111-8111-111111111111" }), { headers: { "content-type": "application/json" } }) });
  await assert.rejects(restarted.initialize(), /Open this board/); assert.equal(saved.length, 0); restarted.close();
});

test("authenticated SSE admission is bounded and revocation drains actual streams", { timeout: 20_000 }, async (t) => {
  const { server, repo, project } = await fixture(t);
  const headers = await headersFor(repo, server); const readers = [];
  for (let i = 0; i < 4; i++) {
    const response = await fetch(`${server.url}/api/projects/${project.slug}/events`, { headers }); assert.equal(response.status, 200);
    const reader = response.body.getReader(); assert.ok(!(await reader.read()).done); readers.push(reader);
  }
  assert.equal((await fetch(`${server.url}/api/projects/${project.slug}/events`, { headers })).status, 429);
  await server.close();
  for (const reader of readers) { try { while (!(await reader.read()).done) {} } catch { /* Revocation may close the socket with an error. */ } finally { reader.releaseLock(); } }
});

test("a real same-port daemon restart invalidates old browser authority and fresh private open recovers", { timeout: 20_000 }, async (t) => {
  const { server, repo } = await fixture(t);
  const saved = storage();
  const browser = new BrowserBoardAccess({ origin: server.url, href: await requestBoardBootstrap(repo), storage: saved,
    replaceUrl: () => {}, fetch, now: Date.now });
  await browser.initialize(); assert.equal(saved.length, 1);
  const oldHeaders = await headersFor(repo, server);
  await server.close();
  const next = await startBoardDaemon({ port: server.port, repoRoot: repo, controlMode: "projector" });
  try {
    assert.notEqual(next.instanceId, server.instanceId);
    assert.equal((await fetch(next.url + "/api/projects", { headers: oldHeaders })).status, 401);
    const stale = new BrowserBoardAccess({ origin: next.url, href: next.url + "/usage", storage: saved, replaceUrl: () => {}, fetch });
    try { await assert.rejects(stale.initialize()); assert.equal(saved.length, 0); } finally { stale.close(); }
    const fresh = new BrowserBoardAccess({ origin: next.url, href: await requestBoardBootstrap(repo, "usage"), storage: saved, replaceUrl: () => {}, fetch });
    try { await fresh.initialize(); assert.equal((await fresh.fetch("/api/usage/machine")).status, 200); } finally { fresh.close(); }
  } finally { browser.close(); await next.close(); }
});
