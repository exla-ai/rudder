import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import net from "node:net";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { runCloudCommand } from "../dist/cloud.js";
import { canonicalPreviewUrl, cloudPreviews, previewAccess, previewReachable } from "../dist/tailscale-preview.js";
import { createTailnetPreviews, previewOrigin, probePreview, projectForPreviewPath, tailnetStatus } from "../cloud/worker/tailscale-previews.mjs";

const host = "rudder-home.example.ts.net";
const online = () => ({ BackendState: "Running", Self: { HostName: "rudder-home", DNSName: `${host}.`, Online: true } });
const listener = (extra = {}) => ({ port: 3000, pid: 42, workspaceId: "project-a", label: "vite", address: "0100007F", family: 4, ...extra });
const success = { code: 0, stdout: "", stderr: "" };

const project = (name, extra = {}) => ({ name, workspaceId: `project-${name}`, dir: `/workspace/code/${name}`, ...extra });

test("preview ownership uses exact project boundaries regardless of similar names and order", () => {
  const foo = project("foo");
  const fooBar = project("foo-bar");
  for (const projects of [[foo, fooBar], [fooBar, foo]]) {
    assert.equal(projectForPreviewPath(foo.dir, projects), foo.workspaceId);
    assert.equal(projectForPreviewPath(`${foo.dir}/src`, projects), foo.workspaceId);
    assert.equal(projectForPreviewPath(`${fooBar.dir}/src`, projects), fooBar.workspaceId);
    for (const cwd of ["/workspace/code/foobar/src", "/workspace/code/foo-other/src", "/workspace/code/foo-barista/src", "/tmp/foo/src"]) {
      assert.equal(projectForPreviewPath(cwd, projects), null, cwd);
    }
  }
  for (const cwd of ["", null, 42, "foo/src", "/workspace/code/foo\0/src", "/workspace/code/foo/../unmanaged/src"]) {
    assert.equal(projectForPreviewPath(cwd, [foo]), null);
  }
});

test("modern nested workspaces belong to the most specific managed project", () => {
  const outer = project("foo");
  const inner = project("ui", { dir: `${outer.dir}/packages/ui` });
  for (const projects of [[outer, inner], [inner, outer]]) {
    assert.equal(projectForPreviewPath(`${inner.dir}/src`, projects), inner.workspaceId);
    for (const directory of [".rudder-workspaces", ".rudder-worktrees"]) {
      assert.equal(projectForPreviewPath(`${inner.dir}/${directory}/ui-abcdef0123/task/src`, projects), inner.workspaceId);
      assert.equal(projectForPreviewPath(`${outer.dir}/${directory}/foo-abcdef0123/task/src`, projects), outer.workspaceId);
    }
  }
  assert.equal(projectForPreviewPath(`${outer.dir}//src/`, [project("foo", { dir: `${outer.dir}/` })]), outer.workspaceId);
});

test("legacy sibling workspaces require an exact project slug and a complete legitimate hash", () => {
  const foo = project("foo");
  const fooBar = project("foo-bar");
  const sha1 = (value) => createHash("sha1").update(value).digest("hex").slice(0, 10);
  for (const projects of [[foo, fooBar], [fooBar, foo]]) {
    for (const directory of [".rudder-workspaces", ".rudder-worktrees"]) {
      for (const item of [foo, fooBar]) {
        const group = `${item.name}-${sha1(item.dir)}`;
        assert.equal(projectForPreviewPath(`/workspace/code/${directory}/${group}/fix-preview-a1b2c/src`, projects), item.workspaceId);
      }
      // Older/native hashes also have ten lowercase hexadecimal characters.
      assert.equal(projectForPreviewPath(`/workspace/code/${directory}/foo-e2dd83c0e5/old-task-12345678`, projects), foo.workspaceId);
    }
  }
  // A descriptor's display name cannot override the actual project directory.
  assert.equal(projectForPreviewPath(`/workspace/code/.rudder-workspaces/foo-bar-${sha1(fooBar.dir)}/task`, [{ ...fooBar, name: "foo" }]), fooBar.workspaceId);
  assert.equal(projectForPreviewPath(`/workspace/code/.rudder-workspaces/foo-bar-${sha1(fooBar.dir)}/task`, [foo]), null);
});

test("unmanaged and malformed legacy groups never advertise another project's preview", () => {
  const projects = [project("foo"), project("foo-bar")];
  for (const group of ["foo-unmanaged", "foo-other-abcdef0123", "foo-bar-other-abcdef0123", "foo-abcdef012", "foo-abcdef01234", "foo-abcdef012g", "foo-ABCDEF0123", "foo-abcdef0123-neighbor"]) {
    assert.equal(projectForPreviewPath(`/workspace/code/.rudder-workspaces/${group}/task/src`, projects), null, group);
  }
  for (const cwd of ["/workspace/code/.rudder-workspaces/foo-abcdef0123", "/workspace/code/.rudder-workspaces", "/other/code/.rudder-workspaces/foo-abcdef0123/task", "/workspace/code/.rudder-workspaces-neighbor/foo-abcdef0123/task"]) {
    assert.equal(projectForPreviewPath(cwd, projects), null, cwd);
  }
});

test("ambiguous project roots and colliding legacy slugs fail closed", () => {
  const foo = project("foo");
  const alias = { ...foo, workspaceId: "other-owner" };
  for (const projects of [[foo, alias], [alias, foo]]) {
    assert.equal(projectForPreviewPath(`${foo.dir}/src`, projects), null);
    assert.equal(projectForPreviewPath("/workspace/code/.rudder-workspaces/foo-abcdef0123/task/src", projects), null);
  }
  assert.equal(projectForPreviewPath(`${foo.dir}/src`, [foo, { ...foo }]), foo.workspaceId);
  const hyphen = project("foo-bar");
  const underscore = project("foo_bar");
  for (const projects of [[hyphen, underscore], [underscore, hyphen]]) {
    assert.equal(projectForPreviewPath("/workspace/code/.rudder-workspaces/foo-bar-abcdef0123/task/src", projects), null);
    assert.equal(projectForPreviewPath(`${hyphen.dir}/src`, projects), hyphen.workspaceId);
    assert.equal(projectForPreviewPath(`${underscore.dir}/src`, projects), underscore.workspaceId);
  }
  const longName = `${"a".repeat(47)}-project`;
  const long = project(longName);
  // TS truncates the slug before adding its separator; native trims that dash.
  for (const slug of ["a".repeat(47), `${"a".repeat(47)}-`]) {
    assert.equal(projectForPreviewPath(`/workspace/code/.rudder-workspaces/${slug}-abcdef0123/task`, [long]), long.workspaceId);
  }
});

function harness({ status = online(), serve = () => success, probe = async () => true } = {}) {
  const calls = [];
  let clock = 0;
  const manager = createTailnetPreviews({
    now: () => clock,
    probe,
    execute: async (args) => {
      calls.push(args);
      return args[0] === "status" ? { ...success, stdout: JSON.stringify(status) } : serve(args);
    },
  });
  return { manager, calls, status: (value) => { status = value; }, time: (value) => { clock = value; } };
}

test("Tailscale online requires a valid live backend and self state", () => {
  assert.equal(tailnetStatus(online()).online, true);
  assert.equal(tailnetStatus(online()).dnsName, host);
  for (const status of [null, {}, { BackendState: "Running" }, { BackendState: "Stopped", Self: { Online: true } }, { BackendState: "Running", Self: { Online: false } }]) {
    assert.equal(tailnetStatus(status).online, false);
  }
  assert.equal(tailnetStatus({ ...online(), Self: { ...online().Self, DNSName: "attacker.example.com" } }).dnsName, null);
});

test("discovery accepts project-owned loopback/wildcard listeners including IPv6", () => {
  assert.equal(previewOrigin(listener()), "http://127.0.0.1:3000");
  assert.equal(previewOrigin(listener({ address: "00000000" })), "http://127.0.0.1:3000");
  assert.equal(previewOrigin(listener({ address: "0200007F" })), "http://127.0.0.2:3000");
  for (const address of ["00000000000000000000000000000000", "00000000000000000000000001000000"]) {
    assert.equal(previewOrigin(listener({ family: 6, address })), "http://[::1]:3000");
  }
  for (const extra of [{ workspaceId: null }, { address: "0100000A" }, { port: 80 }, { port: 41641 }, { port: 65536 }, { port: "3000" }]) {
    assert.equal(previewOrigin(listener(extra)), null);
  }
});

test("URLs appear only after HTTP and private Serve succeed, and unchanged ports reuse the setup", async () => {
  let release;
  const h = harness({ probe: () => new Promise((resolve) => { release = resolve; }) });
  const pending = h.manager.reconcile([listener()]);
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(h.manager.snapshot(), []);
  release(true);
  await pending;
  assert.equal(h.manager.snapshot()[0].url, `https://${host}:3000/`);
  assert.deepEqual(h.calls.find((args) => args[0] === "serve"), ["serve", "--bg", "--https=3000", "http://127.0.0.1:3000"]);
  const count = h.calls.filter((args) => args[0] === "serve").length;
  await h.manager.reconcile([listener()]);
  assert.equal(h.calls.filter((args) => args[0] === "serve").length, count);
  assert.ok(h.calls.every((args) => !args.includes("funnel")));
});

test("non-HTTP listeners never configure a route", async () => {
  const h = harness({ probe: async () => false });
  await h.manager.reconcile([listener()]);
  assert.deepEqual(h.manager.snapshot(), []);
  assert.equal(h.calls.filter((args) => args[0] === "serve").length, 0);
});

test("an unchanged listener that stops speaking HTTP loses its ready URL", async () => {
  let healthy = true;
  const h = harness({ probe: async () => healthy });
  await h.manager.reconcile([listener()]);
  healthy = false;
  h.time(5001);
  await h.manager.reconcile([listener()]);
  assert.deepEqual(h.manager.snapshot(), []);
  assert.ok(h.calls.some((args) => args.join(" ") === "serve --https=3000 off"));
});

test("HTTPS failure has a verified private HTTP fallback and recovers after configuration changes", async () => {
  let enabled = false;
  const h = harness({ serve: (args) => args.includes("--https=3000") && args.includes("--bg") && !enabled
    ? { code: 1, stdout: "", stderr: "HTTPS disabled" } : success });
  await h.manager.reconcile([listener()]);
  assert.equal(h.manager.snapshot()[0].url, `http://${host}:3000/`);
  enabled = true;
  h.time(60_001);
  await h.manager.reconcile([listener()]);
  assert.equal(h.manager.snapshot()[0].url, `https://${host}:3000/`);
  assert.ok(h.calls.some((args) => args.join(" ") === "serve --http=3000 off"));
});

test("failed Serve, offline nodes, and stale status never advertise a usable URL", async () => {
  const h = harness({ serve: () => ({ code: 1, stdout: "", stderr: "denied" }) });
  await h.manager.reconcile([listener()]);
  assert.deepEqual(h.manager.snapshot(), []);
  const ready = harness();
  await ready.manager.reconcile([listener()]);
  ready.status({ BackendState: "NeedsLogin", Self: { Online: true } });
  await ready.manager.reconcile([listener()]);
  assert.deepEqual(ready.manager.snapshot(), []);
  ready.status(online());
  await ready.manager.reconcile([listener()]);
  assert.equal(ready.manager.snapshot().length, 1);
  assert.equal(ready.calls.filter((args) => args.includes("--bg")).length, 2, "reconnect reapplies the private route");
  ready.status({ ...online(), Self: { ...online().Self, DNSName: "" } });
  await ready.manager.reconcile([listener()]);
  assert.deepEqual(ready.manager.snapshot(), [], "an invalid DNS update cannot preserve an old URL");
});

test("restart cleanup touches only the persistent list of Rudder-owned ports", async () => {
  const calls = [];
  const persisted = [];
  const manager = createTailnetPreviews({ ownedPorts: [3000, 4000, "5000", -1], onOwnedPorts: (ports) => persisted.push(ports),
    execute: async (args) => { calls.push(args); return args[0] === "status" ? { ...success, stdout: JSON.stringify(online()) } : success; } });
  await manager.reconcile([]);
  assert.deepEqual(calls.slice(1), [["serve", "--https=3000", "off"], ["serve", "--http=3000", "off"], ["serve", "--https=4000", "off"], ["serve", "--http=4000", "off"]]);
  assert.deepEqual(persisted.at(-1), []);
});

test("a persisted proxy is revoked if its listener no longer speaks HTTP", async () => {
  const calls = [];
  const persisted = [];
  const manager = createTailnetPreviews({ ownedPorts: [3000], probe: async () => false,
    onOwnedPorts: (ports) => persisted.push(ports),
    execute: async (args) => { calls.push(args); return args[0] === "status" ? { ...success, stdout: JSON.stringify(online()) } : success; } });
  await manager.reconcile([listener()]);
  assert.deepEqual(manager.snapshot(), []);
  assert.deepEqual(calls.slice(1), [["serve", "--https=3000", "off"], ["serve", "--http=3000", "off"]]);
  assert.deepEqual(persisted.at(-1), []);
});

test("certificate setup failure is attempted once per scan, not once per discovered port", async () => {
  const h = harness({ serve: (args) => args.some((arg) => arg.startsWith("--https=")) && args.includes("--bg")
    ? { code: 1, stdout: "", stderr: "HTTPS consent required" } : success });
  await h.manager.reconcile([listener(), listener({ port: 4000 })]);
  assert.equal(h.calls.filter((args) => args.includes("--bg") && args.some((arg) => arg.startsWith("--https="))).length, 1);
  assert.equal(h.manager.snapshot().length, 2);
});

test("port removal or PID/project reuse revokes the old route before the replacement", async () => {
  const h = harness();
  await h.manager.reconcile([listener()]);
  await h.manager.reconcile([listener({ pid: 99, workspaceId: "project-b" })]);
  assert.equal(h.manager.snapshot()[0].workspaceId, "project-b");
  const off = h.calls.findIndex((args) => args.join(" ") === "serve --https=3000 off");
  const replacement = h.calls.findLastIndex((args) => args.includes("--bg"));
  assert.ok(off < replacement);
  await h.manager.reconcile([]);
  assert.deepEqual(h.manager.snapshot(), []);
  assert.deepEqual(h.calls.slice(-2), [["serve", "--https=3000", "off"], ["serve", "--http=3000", "off"]]);
});

test("browser URLs must exactly match the account's tailnet host, port and root", () => {
  assert.equal(canonicalPreviewUrl(`https://${host}:3000`, host, 3000), `https://${host}:3000/`);
  for (const url of ["javascript:alert(1)", "https://example.com:3000/", `https://${host}.evil.com:3000/`, `https://me:secret@${host}:3000/`, `https://${host}:3001/`, `https://${host}:3000/path`, `https://${host}:3000/?token=x`]) {
    assert.equal(canonicalPreviewUrl(url, host, 3000), null);
  }
});

const state = () => ({ home: { connected: true, stateLive: true, tailnet: { online: true, dnsName: host } }, projects: [
  { workspaceId: "a", name: "repo-a", repo: "owner/repo-a", live: true, ports: [{ port: 3000, label: "vite", url: `https://${host}:3000/` }] },
  { workspaceId: "b", name: "repo-b", repo: "owner/repo-b", live: true, ports: [{ port: 4000, label: "next", url: `https://${host}:4000/` }] },
] });

test("CLI discovery is project-scoped and rejects remembered disconnected state", () => {
  assert.deepEqual(cloudPreviews(state(), { repo: "OWNER/REPO-A" }).map((entry) => entry.port), [3000]);
  assert.deepEqual(cloudPreviews(state(), { name: "repo-b" }).map((entry) => entry.port), [4000]);
  for (const field of ["connected", "stateLive"]) {
    const value = state(); value.home[field] = false;
    assert.deepEqual(cloudPreviews(value), []);
  }
  const value = state(); value.projects[0].live = false;
  assert.deepEqual(cloudPreviews(value, { name: "repo-a" }), []);
});

test("opening requires an online local client and the matching visible peer", () => {
  const local = { BackendState: "Running", Self: { Online: true, DNSName: "laptop.example.ts.net." }, Peer: { key: { DNSName: `${host}.`, Online: true } } };
  assert.equal(previewAccess(local, host).ok, true);
  assert.equal(previewAccess(online(), host).ok, true);
  assert.equal(previewAccess(null, host).ok, false);
  assert.equal(previewAccess({ ...local, BackendState: "Stopped" }, host).ok, false);
  assert.equal(previewAccess({ ...local, Peer: {} }, host).ok, false);
  local.Peer.key.Online = false;
  assert.equal(previewAccess(local, host).ok, false);
});

test("cloud preview JSON uses the existing authenticated home API and never wakes or opens a browser", async (t) => {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-preview-cli-"));
  const previousCwd = process.cwd();
  const previousToken = process.env.RUDDER_CLOUD_TOKEN;
  const previousUrl = process.env.RUDDER_CLOUD_URL;
  const previousLog = console.log;
  const output = [];
  const requests = [];
  let payload = state();
  const server = http.createServer((req, res) => {
    requests.push({ method: req.method, path: req.url, auth: req.headers.authorization });
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify(payload));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => {
    console.log = previousLog;
    process.chdir(previousCwd);
    if (previousToken === undefined) delete process.env.RUDDER_CLOUD_TOKEN; else process.env.RUDDER_CLOUD_TOKEN = previousToken;
    if (previousUrl === undefined) delete process.env.RUDDER_CLOUD_URL; else process.env.RUDDER_CLOUD_URL = previousUrl;
    server.closeAllConnections(); server.close();
    fs.rmSync(scratch, { recursive: true, force: true });
  });
  assert.equal(spawnSync("git", ["init", "-q", scratch]).status, 0);
  assert.equal(spawnSync("git", ["-C", scratch, "remote", "add", "origin", "https://github.com/owner/repo-a.git"]).status, 0);
  process.chdir(scratch);
  process.env.RUDDER_CLOUD_TOKEN = "rdr_preview_fixture";
  process.env.RUDDER_CLOUD_URL = `http://127.0.0.1:${server.address().port}`;
  console.log = (text) => output.push(text);
  await runCloudCommand("cloud", ["preview"], { json: true });
  assert.deepEqual(JSON.parse(output.at(-1)).previews.map((entry) => entry.port), [3000]);
  payload.home.connected = false;
  await runCloudCommand("cloud", ["preview", "--list"], { json: true });
  assert.deepEqual(JSON.parse(output.at(-1)).previews, []);
  assert.equal(requests.length, 2);
  assert.ok(requests.every((request) => request.method === "GET" && request.path === "/api/rudder/home" && request.auth === "Bearer rdr_preview_fixture"));
});

test("HTTP readiness and local reachability perform real bounded loopback checks", async (t) => {
  const server = http.createServer((req, res) => { res.writeHead(401); res.end(); });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => server.close());
  const url = `http://127.0.0.1:${server.address().port}/`;
  assert.equal(await probePreview(url), true);
  assert.equal(await previewReachable(url), true);
  const other = net.createServer((socket) => { socket.end("not HTTP\n"); });
  await new Promise((resolve) => other.listen(0, "127.0.0.1", resolve));
  t.after(() => other.close());
  assert.equal(await probePreview(`http://127.0.0.1:${other.address().port}/`, 100), false);
});

test("an incomplete HTTP response is bounded by the total probe deadline", { timeout: 3000 }, async (t) => {
  const sockets = new Set();
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("error", () => {});
    socket.write("HTTP/1.1 200 OK\r\nX-Probe: ");
    const timer = setInterval(() => socket.write("."), 10);
    socket.on("close", () => { clearInterval(timer); sockets.delete(socket); });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => { for (const socket of sockets) socket.destroy(); server.close(); });
  assert.equal(await probePreview(`http://127.0.0.1:${server.address().port}/`, 80), false);
});
