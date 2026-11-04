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
    if (req.url === "/api/rudder/home") {
      res.end(JSON.stringify(payload));
    } else if (req.url === "/api/app/previews?workspace=a") {
      // Current discovery binds the registered process and owner, rather than
      // treating remembered Home port hints as a preview grant.
      res.end(JSON.stringify({ version: 1, previews: payload.home.connected ? [{
        id: "a".repeat(64), tenantId: "account", projectId: "project-a", workspaceId: "execution-a",
        attemptId: "attempt-a", deviceId: "device-a", processStart: "fixture-start", pid: 42,
        dnsName: host, url: `https://${host}:3000/`, port: 3000, protocol: "https", ready: true,
      }] : [] }));
    } else {
      res.writeHead(404);
      res.end(JSON.stringify({ error: "Unexpected preview API request" }));
    }
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
  assert.deepEqual(requests.map(request => request.path), [
    "/api/rudder/home", "/api/app/previews?workspace=a",
    "/api/rudder/home", "/api/app/previews?workspace=a",
  ]);
  assert.ok(requests.every((request) => request.method === "GET" && request.auth === "Bearer rdr_preview_fixture"));
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
  assert.equal(await previewReachable(`http://127.0.0.1:${other.address().port}/`, 100), false);
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
  assert.equal(await previewReachable(`http://127.0.0.1:${server.address().port}/`, 80), false);
});

// v4 uses complete project/attempt/process/device references. Legacy discovery
// tests above remain coverage for the deployed home-state adapter.
const { PrivatePreviewDirectory, loopbackPreviewOrigin, observeLinuxPreviewListeners, previewProcessMatches } = await import("../dist/private-access/previews.js");
const binding = { tenantId: "tenant-a", projectId: "project-a", workspaceId: "workspace-a", attemptId: "attempt-a", workspacePath: "/workspace/project-a" };
const scopedListener = (extra = {}) => ({ ...binding, pid: 42, processStart: "100", port: 3000, protocol: "http", origin: "http://127.0.0.1:3000/", role: "preview", ...extra });
const scopedOnline = () => ({ ...online(), Self: { ...online().Self, ID: "device-a" } });
function privateHarness(options = {}) {
  const calls = [];
  let clock = 0;
  let status = scopedOnline();
  const configured = { TCP: {}, Web: {} };
  const directory = new PrivatePreviewDirectory({ timeoutMs: 30, now: () => clock,
    processMatches: async () => true, probe: async () => true, ...options,
    execute: async (args, signal) => {
      calls.push(args);
      let result = options.execute ? await options.execute(args, signal)
        : { code: 0, stdout: args[0] === "status" ? JSON.stringify(status) : args[1] === "status" ? "{}" : "" };
      if (args[0] === "serve" && args[1] === "status" && result.code === 0 && result.stdout === "{}") {
        result = { ...result, stdout: JSON.stringify(configured) };
      }
      const mode = args.find((arg) => /^--(?:http|https)=/.test(arg));
      if (result.code === 0 && args[0] === "serve" && mode) {
        const port = Number(mode.split("=")[1]);
        const service = `${host}:${port}`;
        if (args.includes("off")) { delete configured.TCP[port]; delete configured.Web[service]; }
        else if (args.includes("--bg")) {
          configured.TCP[port] = mode.startsWith("--https=") ? { HTTPS: true } : { HTTP: true };
          configured.Web[service] = { Handlers: { "/": { Proxy: args.at(-1) } } };
        }
      }
      return result;
    } });
  return { directory, calls, time: (value) => { clock = value; }, status: (value) => { status = value; } };
}
const localScopedStatus = () => ({ BackendState: "Running", Self: { ID: "laptop", Online: true }, Peer: { worker: { ID: "device-a", DNSName: host, Online: true } } });
const openOptions = (extra = {}) => ({ authorize: async () => true, localStatus: async () => localScopedStatus(), probe: async () => true, ...extra });

test("v4 private discovery binds readiness to project, workspace, attempt, process and device", async () => {
  const h = privateHarness();
  await h.directory.reconcile([scopedListener()]);
  const original = h.directory.snapshot(binding)[0];
  assert.equal(original.deviceId, "device-a");
  assert.equal(original.processStart, "100");
  assert.equal(original.attemptId, "attempt-a");
  assert.equal(h.directory.snapshot({ ...binding, projectId: "project-b" }).length, 0);
  await h.directory.reconcile([scopedListener({ attemptId: "attempt-b", processStart: "101" })]);
  const replacement = h.directory.snapshot()[0];
  assert.notEqual(replacement.id, original.id);
  assert.equal(h.directory.snapshot(binding).length, 0);
  assert.ok(h.calls.findIndex((args) => args.join(" ") === "serve --http=3000 off") < h.calls.findLastIndex((args) => args.includes("--bg")));
  await assert.rejects(h.directory.open(original.id, binding, openOptions()), /no longer ready/);
  await h.directory.reconcile([]);
  assert.deepEqual(h.directory.snapshot(), []);
});

test("same-device identical ports and infrastructure listeners fail closed", async () => {
  const h = privateHarness();
  await h.directory.reconcile([scopedListener(), scopedListener({ projectId: "other-project", workspaceId: "other-workspace" })]);
  assert.deepEqual(h.directory.snapshot(), []);
  assert.equal(h.calls.filter((args) => args[0] === "serve" && args[1] !== "status").length, 0);
  await h.directory.reconcile([scopedListener({ role: "control" }), scopedListener({ port: 41641, origin: "http://127.0.0.1:41641/" })]);
  assert.deepEqual(h.directory.snapshot(), []);
  assert.equal(loopbackPreviewOrigin("::1", 3000), "http://[::1]:3000/");
  assert.equal(loopbackPreviewOrigin("[::1]", 3000), "http://[::1]:3000/");
  assert.equal(loopbackPreviewOrigin("127.0.0.2", 3000), "http://127.0.0.2:3000/");
  assert.equal(loopbackPreviewOrigin("10.0.0.1", 3000), null);
});

test("v4 requires live device identity and expires observations using a local monotonic clock", async () => {
  const h = privateHarness();
  h.status(online());
  await h.directory.reconcile([scopedListener()]);
  assert.deepEqual(h.directory.snapshot(), []);
  h.status(scopedOnline());
  await h.directory.reconcile([scopedListener()]);
  assert.equal(h.directory.snapshot().length, 1);
  h.time(10_001);
  assert.deepEqual(h.directory.snapshot(), []);
  await h.directory.reconcile([scopedListener()]);
  assert.equal(h.directory.snapshot().length, 1);
  h.status({ ...scopedOnline(), BackendState: "NeedsLogin" });
  await h.directory.reconcile([scopedListener()]);
  assert.deepEqual(h.directory.snapshot(), []);
});

test("hung Tailscale and readiness probes stay bounded and do not serialize all work forever", { timeout: 2000 }, async () => {
  let calls = 0;
  const h = privateHarness({ execute: async () => { calls++; return new Promise(() => {}); } });
  const before = performance.now();
  await h.directory.reconcile([scopedListener()]);
  await h.directory.reconcile([scopedListener()]);
  assert.equal(calls, 2);
  assert.ok(performance.now() - before < 1000);
  assert.deepEqual(h.directory.snapshot(), []);
  const slow = privateHarness({ probe: () => new Promise(() => {}) });
  await slow.directory.reconcile([scopedListener()]);
  assert.deepEqual(slow.directory.snapshot(), []);
  assert.equal(slow.calls.filter((args) => args[0] === "serve" && args[1] !== "status").length, 0);
});

test("uncertain route effects cannot publish or race a replacement owner", { timeout: 2000 }, async () => {
  let release;
  const calls = [];
  const h = privateHarness({ execute: async (args) => {
    calls.push(args);
    if (args[0] === "status") return { code: 0, stdout: JSON.stringify(scopedOnline()) };
    if (args[1] === "status") return { code: 0, stdout: "{}" };
    if (args.includes("--bg") && !release) return new Promise((resolve) => { release = resolve; });
    return success;
  } });
  await h.directory.reconcile([scopedListener()]);
  assert.deepEqual(h.directory.snapshot(), []);
  await h.directory.reconcile([scopedListener({ attemptId: "attempt-b" })]);
  assert.equal(calls.filter((args) => args.includes("--bg")).length, 1, "hung old mutation prevents replacement effects");
  release(success);
  await new Promise((resolve) => setImmediate(resolve));
  await h.directory.reconcile([scopedListener({ attemptId: "attempt-b" })]);
  assert.equal(h.directory.snapshot()[0].attemptId, "attempt-b");
  const cleanup = calls.findIndex((args) => args.join(" ") === "serve --http=3000 off");
  assert.ok(cleanup >= 0 && cleanup < calls.findLastIndex((args) => args.includes("--bg")));
  assert.ok(calls.every((args) => !args.includes("funnel") && !args.includes("up") && !args.includes("login")));
});

test("private HTTP fallback is explicit and failed retirement blocks reassignment", async () => {
  let cleanupFails = false;
  const h = privateHarness({ execute: async (args) => args[0] === "status"
    ? { code: 0, stdout: JSON.stringify(scopedOnline()) }
    : args[1] === "status" ? { code: 0, stdout: "{}" } : { code: cleanupFails || args.includes("--bg") ? 1 : 0, stdout: "" } });
  await h.directory.reconcile([scopedListener()]);
  assert.deepEqual(h.directory.snapshot(), []);
  const fallback = privateHarness({ allowPrivateHttp: true, execute: async (args) => args[0] === "status"
    ? { code: 0, stdout: JSON.stringify(scopedOnline()) }
    : args[1] === "status" ? { code: 0, stdout: "{}" } : { code: args.includes("--https=3000") && args.includes("--bg") ? 1 : 0, stdout: "" } });
  await fallback.directory.reconcile([scopedListener()]);
  assert.equal(fallback.directory.snapshot()[0].url, `http://${host}:3000/`);
  cleanupFails = true;
  await h.directory.reconcile([scopedListener({ attemptId: "attempt-b" })]);
  assert.deepEqual(h.directory.snapshot(), []);
});

test("opening requires fresh authorization, matching project and device, and an HTTP route", async () => {
  const h = privateHarness();
  await h.directory.reconcile([scopedListener()]);
  const preview = h.directory.snapshot()[0];
  assert.deepEqual(await h.directory.open(preview.id, binding, openOptions()), { url: preview.url, access: "tailnet" });
  await assert.rejects(h.directory.open(preview.id, binding, openOptions({ authorize: async () => false })), /denied/);
  await assert.rejects(h.directory.open(preview.id, { ...binding, workspaceId: "other" }, openOptions()), /no longer ready/);
  await assert.rejects(h.directory.open(preview.id, binding, openOptions({ probe: async () => false })), /unreachable/);
  const wrongDevice = localScopedStatus(); wrongDevice.Peer.worker.ID = "retired-device";
  await assert.rejects(h.directory.open(preview.id, binding, openOptions({ localStatus: async () => wrongDevice })), /unreachable/);
  assert.deepEqual(await h.directory.open(preview.id, binding, openOptions({ localStatus: async () => null,
    relay: async () => "https://relay.example.test/authorized/project-a" })), { url: "https://relay.example.test/authorized/project-a", access: "relay" });
  let checks = 0;
  await assert.rejects(h.directory.open(preview.id, binding, openOptions({ authorize: async () => ++checks === 1 })), /denied/);
});

test("process replacement during an asynchronous open never launches its previous URL", async () => {
  const h = privateHarness();
  await h.directory.reconcile([scopedListener()]);
  const preview = h.directory.snapshot()[0];
  await assert.rejects(h.directory.open(preview.id, binding, openOptions({ probe: async () => {
    await h.directory.reconcile([scopedListener({ processStart: "new-process" })]); return true;
  } })), /unreachable/);
});

test("actual process discovery attributes IPv4/IPv6 inodes only to registered live preview attempts", async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-preview-proc-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const workspacePath = path.join(root, "workspace");
  const procRoot = path.join(root, "proc");
  const pidRoot = path.join(procRoot, "42");
  fs.mkdirSync(workspacePath, { recursive: true });
  fs.mkdirSync(path.join(pidRoot, "fd"), { recursive: true });
  fs.mkdirSync(path.join(pidRoot, "net"), { recursive: true });
  fs.symlinkSync(workspacePath, path.join(pidRoot, "cwd"));
  fs.symlinkSync("socket:[123]", path.join(pidRoot, "fd", "5"));
  fs.symlinkSync("socket:[124]", path.join(pidRoot, "fd", "6"));
  const stat = `42 (name with ) spaces) S ${Array(18).fill("0").join(" ")} 100 0`;
  fs.writeFileSync(path.join(pidRoot, "stat"), stat);
  const row = (address, inode) => `0: ${address}:0BB8 00000000:0000 0A 0 0 0 0 0 ${inode}`;
  fs.writeFileSync(path.join(pidRoot, "net", "tcp"), `header\n${row("0100007F", 123)}\n${row("0100007F", 999)}\n`);
  fs.writeFileSync(path.join(pidRoot, "net", "tcp6"), `header\n${row("00000000000000000000000001000000", 124)}\n`);
  const process = { ...binding, workspacePath, pid: 42, processStart: "100", role: "preview" };
  const observations = await observeLinuxPreviewListeners([process], { procRoot });
  assert.deepEqual(observations.map((row) => row.origin), ["http://127.0.0.1:3000/", "http://[::1]:3000/"]);
  assert.ok(observations.every((row) => row.attemptId === binding.attemptId && row.processStart === "100"));
  assert.deepEqual(await observeLinuxPreviewListeners([{ ...process, role: "control" }], { procRoot }), []);
  assert.deepEqual(await observeLinuxPreviewListeners([{ ...process, processStart: "99" }], { procRoot }), []);
});

test("local status kills a hung fake Tailscale CLI without requiring enrollment", { timeout: 2000 }, async (t) => {
  const { localTailnetStatus } = await import("../dist/tailscale-preview.js");
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-tailscale-hung-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const command = path.join(root, "tailscale");
  fs.writeFileSync(command, `#!${process.execPath}\nsetInterval(() => {}, 1000);\n`, { mode: 0o700 });
  const before = performance.now();
  assert.equal(await localTailnetStatus({ command, timeoutMs: 40 }), null);
  assert.ok(performance.now() - before < 1000);
});

test("private URL readiness is verified separately from local HTTP and Serve configuration", async () => {
  const probes = [];
  const h = privateHarness({ probe: async (url) => { probes.push(url); return url.startsWith("http://127."); } });
  await h.directory.reconcile([scopedListener()]);
  assert.deepEqual(probes, ["http://127.0.0.1:3000/", `https://${host}:3000/`]);
  assert.deepEqual(h.directory.snapshot(), [], "ACL-denied private route is never ready");
  assert.ok(h.calls.some((args) => args.join(" ") === "serve --https=3000 off"));
});

test("discovery never overwrites an unrelated Serve or public Funnel service", async () => {
  for (const config of [{ TCP: { "3000": { TCPForward: "127.0.0.1:9999" } } },
    { Web: { [`${host}:3000`]: { Handlers: { "/": { Proxy: "http://127.0.0.1:9999" } } } } },
    { AllowFunnel: { [`${host}:3000`]: true } }]) {
    const calls = [];
    const h = privateHarness({ execute: async (args) => {
      calls.push(args);
      return { code: 0, stdout: JSON.stringify(args[0] === "status" ? scopedOnline() : config) };
    } });
    await h.directory.reconcile([scopedListener()]);
    assert.deepEqual(h.directory.snapshot(), []);
    assert.equal(calls.filter((args) => args[0] === "serve" && args[1] !== "status").length, 0);
  }
});

test("malformed or hung Serve configuration cannot authorize a route mutation", { timeout: 2000 }, async () => {
  for (const config of ["", "null", "[]", "malformed"]) {
    let effects = 0;
    const h = privateHarness({ execute: async (args) => {
      if (args[0] === "status") return { code: 0, stdout: JSON.stringify(scopedOnline()) };
      if (args[1] === "status") return { code: 0, stdout: config };
      effects++; return success;
    } });
    await h.directory.reconcile([scopedListener()]);
    assert.equal(effects, 0);
    assert.deepEqual(h.directory.snapshot(), []);
  }
  const h = privateHarness({ execute: async (args) => args[0] === "status"
    ? { code: 0, stdout: JSON.stringify(scopedOnline()) } : new Promise(() => {}) });
  await h.directory.reconcile([scopedListener()]);
  assert.deepEqual(h.directory.snapshot(), []);
});

const ts = await import("typescript");
const cloudPreviewSource = new URL("../cloud/src/private-access/preview-access.ts", import.meta.url);
const cloudPreviewCode = ts.transpileModule(fs.readFileSync(cloudPreviewSource, "utf8"), {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 },
}).outputText;
const { AuthorizedPreviewReports } = await import(`data:text/javascript;base64,${Buffer.from(cloudPreviewCode).toString("base64")}`);

test("Cloud preview boundary strictly typechecks without importing the server", () => {
  const program = ts.createProgram([cloudPreviewSource.pathname], {
    target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.NodeNext, moduleResolution: ts.ModuleResolutionKind.NodeNext,
    strict: true, noEmit: true, skipLibCheck: true, types: ["node"],
  });
  const diagnostics = ts.getPreEmitDiagnostics(program);
  assert.deepEqual(diagnostics.map((entry) => ts.flattenDiagnosticMessageText(entry.messageText, "\n")), []);
});

test("Cloud preview reports are tenant/attempt authorized with local receipt freshness and retirement", async () => {
  const h = privateHarness();
  await h.directory.reconcile([scopedListener()]);
  const preview = h.directory.snapshot()[0];
  let clock = 50;
  const reports = new AuthorizedPreviewReports(async (actor, scope) => actor.tenantId === scope.tenantId, { now: () => clock });
  const actor = { tenantId: binding.tenantId };
  reports.receive(binding, [preview, { ...preview, projectId: "other-project" }, null, [], 42, {},
    { ...preview, dnsName: null }, { ...preview, url: {} }, { ...preview, id: "not-a-reference" },
    { ...preview, pid: -1 }, { ...preview, url: "https://public.example.com:3000/" }]);
  assert.equal((await reports.list(actor, binding)).length, 1);
  assert.equal((await reports.resolve(actor, binding, preview.id)).id, preview.id);
  await assert.rejects(reports.list({ tenantId: "other-tenant" }, binding), /denied/);
  await assert.rejects(reports.resolve(actor, { ...binding, attemptId: "other-attempt" }, preview.id), /no longer ready/);
  clock += 10_001;
  assert.deepEqual(await reports.list(actor, binding), []);
  reports.receive(binding, [preview]);
  reports.retire(binding);
  assert.deepEqual(await reports.list(actor, binding), []);
  reports.receive(binding, null);
  assert.deepEqual(await reports.list(actor, binding), []);
});

test("Cloud preview authorization hangs are bounded and denied", { timeout: 2000 }, async () => {
  const reports = new AuthorizedPreviewReports(() => new Promise(() => {}), { timeoutMs: 25 });
  await assert.rejects(reports.list({}, binding), /denied/);
});

test("a previously ready port becoming public is withdrawn without altering unrelated Funnel configuration", async () => {
  let config = {};
  const calls = [];
  const h = privateHarness({ execute: async (args) => {
    calls.push(args);
    return { code: 0, stdout: args[0] === "status" ? JSON.stringify(scopedOnline()) : args[1] === "status" ? JSON.stringify(config) : "" };
  } });
  await h.directory.reconcile([scopedListener()]);
  assert.equal(h.directory.snapshot().length, 1);
  config = { Foreground: { session: { AllowFunnel: { [`${host}:3000`]: true } } } };
  const before = calls.length;
  await h.directory.reconcile([scopedListener()]);
  assert.deepEqual(h.directory.snapshot(), []);
  assert.equal(calls.slice(before).filter((args) => args[0] === "serve" && args[1] !== "status").length, 0);
});

test("an owned ready Serve port repointed to another origin is withdrawn and never overwritten", async () => {
  let override = null;
  const h = privateHarness({ execute: async (args) => ({ code: 0, stdout: args[0] === "status"
    ? JSON.stringify(scopedOnline()) : args[1] === "status" ? JSON.stringify(override ?? {}) : "" }) });
  await h.directory.reconcile([scopedListener()]);
  const preview = h.directory.snapshot()[0];
  assert.ok(preview);
  override = { TCP: { 3000: { HTTPS: true } }, Web: { [`${host}:3000`]: {
    Handlers: { "/": { Proxy: "http://127.0.0.1:9999/" } },
  } } };
  const before = h.calls.length;
  await h.directory.reconcile([scopedListener()]);
  await h.directory.reconcile([scopedListener({ attemptId: "attempt-b" })]);
  assert.deepEqual(h.directory.snapshot(), []);
  assert.equal(h.calls.slice(before).filter((args) => args[0] === "serve" && args[1] !== "status").length, 0);
  await assert.rejects(h.directory.open(preview.id, binding, openOptions()), /no longer ready/);
});

test("a numeric persisted ownership journal cannot overwrite a repointed service after restart", async () => {
  const config = { TCP: { 3000: { HTTPS: true } }, Web: { [`${host}:3000`]: {
    Handlers: { "/": { Proxy: "http://127.0.0.1:9999/" } },
  } } };
  const h = privateHarness({ ownedPorts: [3000], execute: async (args) => ({ code: 0,
    stdout: JSON.stringify(args[0] === "status" ? scopedOnline() : config) }) });
  await h.directory.reconcile([scopedListener()]);
  assert.deepEqual(h.directory.snapshot(), []);
  assert.equal(h.calls.filter((args) => args[0] === "serve" && args[1] !== "status").length, 0);
});

test("listener ownership rechecks inode and exact origin even while PID/start/cwd remain unchanged", async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-preview-inode-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const workspacePath = path.join(root, "workspace");
  const procRoot = path.join(root, "proc");
  const pidRoot = path.join(procRoot, "42");
  fs.mkdirSync(workspacePath, { recursive: true });
  fs.mkdirSync(path.join(pidRoot, "fd"), { recursive: true });
  fs.mkdirSync(path.join(pidRoot, "net"), { recursive: true });
  fs.symlinkSync(workspacePath, path.join(pidRoot, "cwd"));
  fs.symlinkSync("socket:[123]", path.join(pidRoot, "fd", "5"));
  fs.writeFileSync(path.join(pidRoot, "stat"), `42 (preview) S ${Array(18).fill("0").join(" ")} 100 0`);
  const table = (inode) => `header\n0: 0100007F:0BB8 00000000:0000 0A 0 0 0 0 0 ${inode}\n`;
  const tcp = path.join(pidRoot, "net", "tcp");
  fs.writeFileSync(tcp, table(123));
  const listener = scopedListener({ workspacePath });
  assert.equal(await previewProcessMatches(listener, procRoot), true);
  // The original process still owns a different socket, but another process
  // has reused its former port. A PID/start/cwd-only check would accept it.
  fs.writeFileSync(tcp, table(999));
  assert.equal(await previewProcessMatches(listener, procRoot), false);
  fs.writeFileSync(tcp, table(123).replace("0100007F", "0200007F"));
  assert.equal(await previewProcessMatches(listener, procRoot), false);
  fs.writeFileSync(tcp, table(123));
  fs.unlinkSync(path.join(pidRoot, "fd", "5"));
  assert.equal(await previewProcessMatches(listener, procRoot), false);
});

test("local status deadline closes inherited pipes held by a forked CLI descendant", { timeout: 3000 }, async (t) => {
  const { localTailnetStatus } = await import("../dist/tailscale-preview.js");
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-tailscale-fork-"));
  const pidFile = path.join(root, "child.pid");
  t.after(() => {
    if (fs.existsSync(pidFile)) {
      try { process.kill(Number(fs.readFileSync(pidFile, "utf8")), "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") throw error; }
    }
    fs.rmSync(root, { recursive: true, force: true });
  });
  const command = path.join(root, "tailscale");
  fs.writeFileSync(command, `#!${process.execPath}\nconst {spawn}=require('node:child_process');\nconst fs=require('node:fs');\nconst child=spawn(process.execPath,['-e','setInterval(() => {}, 1000)'],{stdio:['ignore','inherit','inherit']});\nfs.writeFileSync(${JSON.stringify(pidFile)},String(child.pid));\nchild.unref();\nprocess.exit(0);\n`, { mode: 0o700 });
  const before = performance.now();
  assert.equal(await localTailnetStatus({ command, timeoutMs: 350 }), null);
  assert.ok(performance.now() - before < 1200);
  assert.ok(fs.existsSync(pidFile), "the forked descendant actually inherited the output pipe");
});
