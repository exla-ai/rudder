// A home on an external host (v4.2): the control plane's host switch, fence,
// refusals and update RPC against the real server with a fake Fly API that
// must never hear about the home (cloud/src/home-host.ts, server.ts); the
// worker's desired.json (cloud/worker/home-host.mjs); the host scripts with a
// fake docker (cloud/host/); and the CLI's pure parts (src/cloud-host.ts).
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import { fileURLToPath } from "node:url";
import { WebSocket } from "ws";

import { hostInfo, hostUpdatePath, validHostImage, validWorkerToken, writeHostUpdate } from "../cloud/worker/home-host.mjs";
import { HOST_FILES, copyFromFlySteps, hostFilesDir, parseHostArgs, renderHomeEnv, validImage } from "../dist/cloud-host.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..");
const cloudDir = path.join(repoRoot, "cloud");
const hostDir = path.join(cloudDir, "host");
const builtHomeHost = new URL("../cloud/dist/home-host.js", import.meta.url);
const lib = fs.existsSync(builtHomeHost) ? await import(builtHomeHost.href) : null;
const libTest = lib ? nodeTest : (name, fn) => nodeTest(name, { skip: "cloud not built" }, fn);
const cloudReady =
  fs.existsSync(path.join(cloudDir, "dist", "server.js")) &&
  fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3"));
const serverTest = cloudReady
  ? nodeTest
  : (name, fn) => nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const Database = cloudReady ? createRequire(path.join(cloudDir, "package.json"))("better-sqlite3") : null;

const tokenHash = (token) => createHash("sha256").update(token).digest("hex").slice(0, 32);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const OLD_IMAGE = "registry.fly.io/workers:old";
const NEW_IMAGE = "registry.fly.io/workers:new";
const tmpdir = (prefix) => fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));

async function waitFor(fn, { timeout = 8000, interval = 50 } = {}) {
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

async function bootServer(t, extraEnv) {
  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-host-test-"));
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
      ...extraEnv,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  child.stdout.on("data", (d) => { log += d.toString(); });
  child.stderr.on("data", (d) => { log += d.toString(); });
  t.after(() => {
    child.kill("SIGKILL");
    try { fs.rmSync(dataDir, { recursive: true, force: true }); } catch { /* ignore */ }
  });
  await waitFor(async () => (await fetch(`${base}/health`).catch(() => null))?.ok);
  await waitFor(() => fs.existsSync(dbPath));
  await sleep(150);
  return { port, base, dbPath, log: () => log };
}

serverTest("an external home never touches Fly: switch, fence, refusals, update RPC, deletion and back", async (t) => {
  const flyCalls = [];
  const fly = http.createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    flyCalls.push({ method: req.method, url: req.url });
    if (req.method === "POST" && req.url === "/v1/apps/workers/machines") {
      res.end(JSON.stringify({ id: "m-new", state: "started" }));
      return;
    }
    res.end("{}");
  });
  await new Promise((resolve) => fly.listen(0, "127.0.0.1", resolve));
  t.after(() => { fly.closeAllConnections(); fly.close(); });
  const bootedAt = Date.now();
  const { port, base, dbPath, log } = await bootServer(t, {
    FLY_API_TOKEN: "fly-fixture-only",
    FLY_API_HOSTNAME: `http://127.0.0.1:${fly.address().port}`,
    RUDDER_FLY_APP_NAME: "workers",
    RUDDER_WORKER_IMAGE: NEW_IMAGE,
    // Only so starting a Fly home passes its configuration check; homes keep no snapshots.
    RUDDER_S3_BUCKET: "rudder-host-fixture-bucket",
    RUDDER_HOME_IDLE_UPDATE_MS: "1",
    RUDDER_HOME_UPDATE_SWEEP_MS: "1000",
    // The idle stop sweep runs every 10 s and would stop a 1 ms idle Fly home.
    RUDDER_WORKSPACE_IDLE_MS: "60000",
    RUDDER_WORKSPACE_SWEEP_MS: "10000",
    RUDDER_HOME_IDLE_MS: "1",
  });
  const old = new Date(Date.now() - 60 * 60 * 1000).toISOString();
  const db = new Database(dbPath);
  t.after(() => db.close());
  const insertToken = db.prepare("insert into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)");
  const insertRow = db.prepare(`
    insert into rudder_workspaces (id, account_id, workspace_key, repo_name, status, machine_id, volume_id, worker_token_hash,
      source_kind, repo_url, home_id, created_at, updated_at, last_activity_at)
    values (@id, @account, @key, @name, 'running', @machine, @volume, @hash, @kind, @url, @home, @old, @old, @old)
  `);
  const tokenA = "rdrw_host_fixture_a_old_token";
  const tokenB = "rdrw_host_fixture_b_old_token";
  for (const [account, cli] of [["acct-a", "rdr_host_cli_a"], ["acct-b", "rdr_host_cli_b"]]) {
    insertToken.run(tokenHash(cli), account, `${account}@example.com`, old, old);
  }
  insertRow.run({ id: "home-a", account: "acct-a", key: "home", name: "home", machine: "m-a", volume: "vol-a", hash: tokenHash(tokenA), kind: "home", url: null, home: null, old });
  insertRow.run({ id: "repo-a", account: "acct-a", key: "home:a/b", name: "b", machine: null, volume: null, hash: tokenHash(tokenA), kind: "home-repo", url: "https://github.com/a/b.git", home: "home-a", old });
  insertRow.run({ id: "home-b", account: "acct-b", key: "home", name: "home", machine: "m-b", volume: "vol-b", hash: tokenHash(tokenB), kind: "home", url: null, home: null, old });

  const api = (cli) => (route, method = "GET", body) => fetch(`${base}${route}`, {
    method,
    headers: { authorization: `Bearer ${cli}`, "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const a = api("rdr_host_cli_a");
  const b = api("rdr_host_cli_b");
  const row = (id) => db.prepare("select status, host_kind, machine_id, worker_token_hash as hash, worker_token_next_hash as next from rudder_workspaces where id = ?").get(id);
  const opens = (homeId, token) => new Promise((resolve) => {
    const ws = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/home/${homeId}/control`, { headers: { authorization: `Bearer ${token}` } });
    ws.on("open", () => { ws.close(); resolve(true); });
    ws.on("error", () => resolve(false));
  });
  // A fake supervisor: answers `update` with `reply`, records every RPC.
  const supervisor = async (homeId, token, reply) => {
    const rpcs = [];
    const ws = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/home/${homeId}/control`, { headers: { authorization: `Bearer ${token}` } });
    t.after(() => ws.close());
    ws.on("message", (data) => {
      const message = JSON.parse(data.toString());
      if (message.type !== "rpc") return;
      rpcs.push(message);
      ws.send(JSON.stringify({ type: "rpc-result", id: message.id, ...reply(message) }));
    });
    await new Promise((resolve, reject) => { ws.once("open", resolve); ws.once("error", reject); });
    const publish = (image) => ws.send(JSON.stringify({
      type: "state",
      host: { kind: "external", image },
      projects: [{ workspaceId: "r", repo: "a/b", name: "b", agents: [{ id: "1", status: "done", asks: [] }] }],
    }));
    return { ws, rpcs, publish };
  };

  assert.equal((await a("/api/rudder/home/host", "POST", { kind: "aws" })).status, 400);
  assert.equal((await (await a("/api/rudder/home/host")).json()).hostKind, "fly");

  // Switch: a new token, handed out once in the env bundle with the image.
  const switched = await (await a("/api/rudder/home/host", "POST", { kind: "external" })).json();
  assert.equal(switched.hostKind, "external");
  assert.equal(switched.image, NEW_IMAGE);
  const { RUDDER_WORKER_TOKEN: hostToken, ...env } = switched.env;
  assert.deepEqual(env, {
    RUDDER_WORKSPACE_ID: "home-a",
    RUDDER_ACCOUNT_ID: "acct-a",
    RUDDER_CLOUD_URL: base,
    RUDDER_HOME: "1",
    RUDDER_REPO_NAME: "home",
    RUDDER_HOST_KIND: "external",
  });
  assert.match(hostToken, /^rdrw_[A-Za-z0-9_-]{40,}$/);
  assert.ok(validWorkerToken(hostToken), "the worker and host scripts accept the token shape");
  assert.equal(switched.home.hostKind, "external");
  assert.ok(!log().includes(hostToken), "the token is never logged");
  assert.deepEqual(row("home-a"), { status: "stopped", host_kind: "external", machine_id: "m-a", hash: tokenHash(hostToken), next: null });
  assert.equal(row("repo-a").hash, tokenHash(hostToken), "the home's repos move to the new token too");
  assert.equal(await opens("home-a", tokenA), false, "the Fly machine's token no longer connects");

  // Nothing to start from here, and nothing Fly-shaped to do.
  const offline = await a("/api/rudder/home/start", "POST", {});
  assert.equal(offline.status, 503);
  assert.match((await offline.json()).error, /external host is offline/);
  const region = await a("/api/rudder/home/region", "POST", { region: "iad" });
  assert.equal(region.status, 409);
  assert.match((await region.json()).error, /external host/);
  assert.equal((await a("/api/rudder/home/stop", "POST", {})).status, 409);
  assert.equal((await a("/api/rudder/workspace/home-a/stop", "POST", {})).status, 409);

  // The host's supervisor connecting is what makes the home running.
  const hostA = await supervisor("home-a", hostToken, () => ({ result: { ok: true } }));
  const summary = await waitFor(async () => {
    const { home } = await (await a("/api/rudder/home")).json();
    return home.connected && home.status === "running" ? home : null;
  });
  assert.equal(summary.hostKind, "external");
  assert.equal((await a("/api/rudder/home/start", "POST", {})).status, 200);

  // Idle on an older image: the update travels over the socket with a
  // replacement token accepted alongside the current one.
  hostA.publish(OLD_IMAGE);
  const update = await waitFor(() => hostA.rpcs.find((m) => m.method === "update"));
  assert.equal(update.params.image, NEW_IMAGE);
  assert.ok(validWorkerToken(update.params.workerToken));
  assert.notEqual(update.params.workerToken, hostToken);
  await waitFor(() => row("home-a").next === tokenHash(update.params.workerToken));
  assert.equal(row("home-a").hash, tokenHash(hostToken));
  assert.ok(!log().includes(update.params.workerToken));
  assert.equal(await opens("home-a", update.params.workerToken), true, log());
  assert.deepEqual([row("home-a").hash, row("home-a").next], [tokenHash(update.params.workerToken), null]);
  hostA.publish(NEW_IMAGE);
  await sleep(1500);
  assert.equal(hostA.rpcs.filter((m) => m.method === "update").length, 1, "once is enough");

  // A Fly home switching drops the socket its machine holds. A refused
  // update leaves no pending token behind.
  const flyB = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/home/home-b/control`, { headers: { authorization: `Bearer ${tokenB}` } });
  await new Promise((resolve) => flyB.once("open", resolve));
  const closed = new Promise((resolve) => flyB.once("close", resolve));
  const switchedB = await (await b("/api/rudder/home/host", "POST", { kind: "external" })).json();
  await closed;
  const hostB = await supervisor("home-b", switchedB.env.RUDDER_WORKER_TOKEN, () => ({ error: "update: this home is not on an external host" }));
  hostB.publish(OLD_IMAGE);
  await waitFor(() => hostB.rpcs.some((m) => m.method === "update"));
  await waitFor(() => log().includes("home-b external update failed"));
  assert.equal(row("home-b").next, null);
  assert.equal(row("home-b").hash, tokenHash(switchedB.env.RUDDER_WORKER_TOKEN));

  // Deleting an external home destroys nothing on Fly.
  const deleted = await (await b("/api/rudder/workspace/home-b", "DELETE")).json();
  assert.equal(deleted.status, "deleted", JSON.stringify(deleted));

  // Through at least one idle stop sweep: still not a single call about either home.
  await sleep(Math.max(0, bootedAt + 11_500 - Date.now()));
  const touched = flyCalls.filter((call) => /m-a|m-b|vol-a|vol-b/.test(call.url));
  assert.deepEqual(touched, [], log());

  // Back to Fly: the old machine goes, a new one on the same volume gets a
  // new token, and the external host is fenced.
  const back = await a("/api/rudder/home/host", "POST", { kind: "fly" });
  assert.equal(back.status, 200, log());
  assert.equal((await back.json()).hostKind, "fly");
  assert.ok(flyCalls.some((call) => call.method === "DELETE" && call.url.startsWith("/v1/apps/workers/machines/m-a")));
  assert.ok(flyCalls.some((call) => call.method === "POST" && call.url === "/v1/apps/workers/machines"));
  const after = row("home-a");
  assert.equal(after.host_kind, null);
  assert.equal(after.machine_id, "m-new");
  assert.notEqual(after.hash, tokenHash(update.params.workerToken));
  assert.equal(await opens("home-a", update.params.workerToken), false, "the external host's token stops working");
});

libTest("home-host.ts: host kind, env bundle, refusals and the update decision", () => {
  assert.equal(lib.homeHostKind(null), "fly");
  assert.equal(lib.homeHostKind("fly"), "fly");
  assert.equal(lib.homeHostKind("external"), "external");
  assert.equal(lib.homeHostKind("EXTERNAL"), "fly");
  assert.equal(lib.parseHomeHostKind("external"), "external");
  assert.equal(lib.parseHomeHostKind("aws"), null);
  assert.deepEqual(Object.keys(lib.homeHostEnv({ homeId: "h", accountId: "a", workerToken: "rdrw_x", cloudUrl: "https://c" })).sort(), [
    "RUDDER_ACCOUNT_ID", "RUDDER_CLOUD_URL", "RUDDER_HOME", "RUDDER_HOST_KIND", "RUDDER_REPO_NAME", "RUDDER_WORKER_TOKEN", "RUDDER_WORKSPACE_ID",
  ]);
  for (const action of ["region", "stop", "resize"]) {
    assert.match(lib.externalHomeRefusal(action), /external host/);
  }
  const decide = (reportedImage, pendingNextToken = false) => lib.externalHomeNeedsUpdate({ reportedImage, targetImage: NEW_IMAGE, pendingNextToken });
  assert.equal(decide(OLD_IMAGE), true);
  assert.equal(decide(NEW_IMAGE), false);
  assert.equal(decide(OLD_IMAGE, true), false, "a pending token is still unresolved");
  assert.equal(decide(null), false, "a home that never said what it runs is left alone");
  assert.equal(lib.externalHomeNeedsUpdate({ reportedImage: OLD_IMAGE, targetImage: "", pendingNextToken: false }), false);
});

nodeTest("the worker writes desired.json atomically, 0600, and only with sane values", () => {
  const volume = tmpdir("rudder-host-volume-");
  const token = "rdrw_worker_fixture_token_0123456789";
  const file = writeHostUpdate({ volume, image: NEW_IMAGE, workerToken: token });
  assert.equal(file, hostUpdatePath(volume));
  assert.equal(fs.statSync(file).mode & 0o777, 0o600);
  assert.equal(fs.statSync(path.dirname(file)).mode & 0o777, 0o700);
  const desired = JSON.parse(fs.readFileSync(file, "utf8"));
  assert.equal(desired.image, NEW_IMAGE);
  assert.equal(desired.workerToken, token);
  assert.deepEqual(fs.readdirSync(path.dirname(file)), ["desired.json"], "no temp file left behind");
  for (const image of ["", "a b", "img;rm -rf /", "$(id)", "-oops"]) {
    assert.throws(() => writeHostUpdate({ volume, image, workerToken: token }), /image/);
  }
  for (const workerToken of ["", "rdr_user_token_0123456789", "rdrw_short", "rdrw_$(id)0123456789abc", 42]) {
    assert.throws(() => writeHostUpdate({ volume, image: NEW_IMAGE, workerToken }), /workerToken/);
  }
  assert.ok(validHostImage("registry.fly.io/rudder-workers-exla:4.2.0@sha256:abc"));
  assert.deepEqual(hostInfo({}), { kind: "fly", image: null });
  assert.deepEqual(hostInfo({ FLY_IMAGE_REF: OLD_IMAGE }), { kind: "fly", image: OLD_IMAGE });
  assert.deepEqual(hostInfo({ RUDDER_HOST_KIND: "external", RUDDER_HOST_IMAGE: NEW_IMAGE }), { kind: "external", image: NEW_IMAGE });
});

// A stand-in docker that logs its argv and answers from FAKE_* variables.
function fakeHost() {
  const root = tmpdir("rudder-host-scripts-");
  const bin = path.join(root, "bin");
  const conf = path.join(root, "conf");
  const data = path.join(root, "data");
  fs.mkdirSync(bin);
  fs.mkdirSync(conf);
  fs.mkdirSync(data);
  const dockerLog = path.join(root, "docker.log");
  fs.writeFileSync(path.join(bin, "docker"), `#!/bin/sh
printf '%s\\n' "$*" >> "${dockerLog}"
case "$1" in
  inspect) [ -n "\${FAKE_INSPECT:-}" ] || exit 1; printf '%s\\n' "$FAKE_INSPECT" ;;
  container) [ "\${FAKE_EXISTS:-0}" = 1 ] ;;
  pull) [ "\${FAKE_PULL_FAIL:-0}" != 1 ] ;;
esac
`, { mode: 0o755 });
  const startLog = path.join(root, "start.log");
  const start = path.join(bin, "fake-start");
  fs.writeFileSync(start, `#!/bin/sh\necho started >> "${startLog}"\n`, { mode: 0o755 });
  const run = (script, args = [], env = {}) => spawnSync(path.join(hostDir, script), args, {
    encoding: "utf8",
    env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, RUDDER_HOST_CONF: conf, RUDDER_HOST_DATA: data, RUDDER_HOME_START: start, ...env },
  });
  const read = (file) => (fs.existsSync(file) ? fs.readFileSync(file, "utf8") : "");
  return { conf, data, run, dockerLog: () => read(dockerLog), started: () => read(startLog), reset: () => { fs.rmSync(dockerLog, { force: true }); fs.rmSync(startLog, { force: true }); } };
}

nodeTest("rudder-home-start recreates the container with the env file and volume, and --keep leaves a current one", () => {
  const host = fakeHost();
  const missing = host.run("rudder-home-start");
  assert.equal(missing.status, 1);
  assert.match(missing.stderr, /image is missing/);
  fs.writeFileSync(path.join(host.conf, "image"), `${NEW_IMAGE}\n`);
  fs.writeFileSync(path.join(host.conf, "home.env"), "RUDDER_WORKER_TOKEN=rdrw_x\n", { mode: 0o600 });
  fs.writeFileSync(path.join(host.conf, "hostname"), "8edd225a7d0378\n");

  const kept = host.run("rudder-home-start", ["--keep"], { FAKE_INSPECT: `true ${NEW_IMAGE}` });
  assert.equal(kept.status, 0, kept.stderr);
  assert.doesNotMatch(host.dockerLog(), /^run /m, "a running container on the right image is left alone");

  host.reset();
  const recreated = host.run("rudder-home-start", ["--keep"], { FAKE_INSPECT: `true ${OLD_IMAGE}`, FAKE_EXISTS: "1" });
  assert.equal(recreated.status, 0, recreated.stderr);
  const calls = host.dockerLog().trim().split("\n");
  assert.ok(calls.includes("stop -t 90 rudder-home"), calls.join("\n"));
  assert.ok(calls.includes("rm rudder-home"));
  const run = calls.find((line) => line.startsWith("run "));
  for (const part of ["--name rudder-home", "--restart unless-stopped", "--privileged", "--hostname 8edd225a7d0378",
    `--env-file ${host.conf}/home.env`, `RUDDER_HOST_IMAGE=${NEW_IMAGE}`, `-v ${host.data}:/workspace`]) {
    assert.ok(run.includes(part), `${part} in: ${run}`);
  }
  assert.ok(run.endsWith(` ${NEW_IMAGE}`));
  assert.ok(!run.includes("rdrw_"), "the token reaches docker through the env file only");
});

nodeTest("rudder-home-apply-update pulls, swaps only the token, and sets bad requests aside", () => {
  const host = fakeHost();
  const envPath = path.join(host.conf, "home.env");
  const imagePath = path.join(host.conf, "image");
  const original = "RUDDER_WORKSPACE_ID=home-x\nRUDDER_WORKER_TOKEN=rdrw_previous_token_0123456789\nRUDDER_HOST_KIND=external\n";
  fs.writeFileSync(envPath, original, { mode: 0o600 });
  fs.writeFileSync(imagePath, `${OLD_IMAGE}\n`);
  const desired = hostUpdatePath(host.data);
  const failed = path.join(path.dirname(desired), "desired.failed.json");

  assert.equal(host.run("rudder-home-apply-update").status, 0, "nothing asked, nothing done");
  assert.equal(host.dockerLog(), "");

  const token = "rdrw_applied_token_abcdefghijklmnop";
  writeHostUpdate({ volume: host.data, image: NEW_IMAGE, workerToken: token });
  const applied = host.run("rudder-home-apply-update");
  assert.equal(applied.status, 0, applied.stderr);
  assert.ok(!`${applied.stdout}${applied.stderr}`.includes(token), "the token is never printed");
  assert.equal(host.dockerLog().trim(), `pull -q ${NEW_IMAGE}`);
  assert.equal(fs.readFileSync(envPath, "utf8"), `RUDDER_WORKSPACE_ID=home-x\nRUDDER_HOST_KIND=external\nRUDDER_WORKER_TOKEN=${token}\n`);
  assert.equal(fs.statSync(envPath).mode & 0o777, 0o600);
  assert.equal(fs.readFileSync(imagePath, "utf8").trim(), NEW_IMAGE);
  assert.equal(host.started().trim(), "started");
  assert.ok(!fs.existsSync(desired));

  const refused = (write, pattern, env = {}) => {
    host.reset();
    const before = fs.readFileSync(envPath, "utf8");
    write();
    const result = host.run("rudder-home-apply-update", [], env);
    assert.equal(result.status, 1);
    assert.match(result.stderr, pattern);
    assert.ok(!fs.existsSync(desired) && fs.existsSync(failed), "set aside so the path unit does not loop");
    assert.equal(fs.readFileSync(envPath, "utf8"), before, "the env file is untouched");
    assert.equal(host.started(), "");
    fs.rmSync(failed);
  };
  // The volume is writable from the container: another repository is refused.
  refused(() => writeHostUpdate({ volume: host.data, image: "evil.example/miner:1", workerToken: token }), /not the same repository/);
  refused(() => fs.writeFileSync(desired, JSON.stringify({ image: NEW_IMAGE, workerToken: "rdrw_$(touch /tmp/x)abcdefgh" })), /malformed/);
  refused(() => fs.writeFileSync(desired, JSON.stringify({ image: "a b", workerToken: token })), /valid reference/);
  refused(() => writeHostUpdate({ volume: host.data, image: "registry.fly.io/workers:4.2.1", workerToken: token }), /docker pull/, { FAKE_PULL_FAIL: "1" });
});

nodeTest("host scripts parse, and the units point at what setup installs", () => {
  for (const script of ["rudder-host-setup", "rudder-home-start", "rudder-home-apply-update"]) {
    const result = spawnSync("sh", ["-n", path.join(hostDir, script)], { encoding: "utf8" });
    assert.equal(result.status, 0, `${script}: ${result.stderr}`);
    assert.ok(fs.statSync(path.join(hostDir, script)).mode & 0o100, `${script} is executable`);
  }
  const unit = (name) => fs.readFileSync(path.join(hostDir, name), "utf8");
  assert.match(unit("rudder-home.service"), /^ExecStart=\/usr\/local\/bin\/rudder-home-start --keep$/m);
  assert.match(unit("rudder-home.service"), /^RequiresMountsFor=\/srv\/workspace$/m);
  assert.match(unit("rudder-home-update.path"), /^PathExists=\/srv\/workspace\/\.rudder-host\/desired\.json$/m);
  assert.match(unit("rudder-home-update.service"), /^ExecStart=\/usr\/local\/bin\/rudder-home-apply-update$/m);
  const setup = unit("rudder-host-setup");
  for (const file of HOST_FILES.filter((name) => name !== "rudder-host-setup")) {
    assert.ok(setup.includes(`$HERE/${file}`), `setup installs ${file}`);
  }
});

nodeTest("rudder cloud host: arguments, env file and the shipped files", () => {
  assert.deepEqual(parseHostArgs(["ubuntu@44.0.0.1", "--data-disk", "/dev/nvme1n1", "--hostname=8edd225a7d0378", "--yes"]), {
    target: "ubuntu@44.0.0.1", dataDisk: "/dev/nvme1n1", hostname: "8edd225a7d0378", copyFromFly: false, skipSetup: false, yes: true,
  });
  assert.equal(parseHostArgs(["box", "--copy-from-fly", "--skip-setup"]).copyFromFly, true);
  for (const bad of [[], ["-oProxyCommand=sh"], ["box;rm"], ["box", "extra"], ["box", "--data-disk"], ["box", "--data-disk", "/dev/x;y"], ["box", "--hostname", "a.b"], ["box", "--nope"]]) {
    assert.throws(() => parseHostArgs(bad), undefined, JSON.stringify(bad));
  }
  assert.equal(renderHomeEnv({ RUDDER_HOME: "1", RUDDER_WORKER_TOKEN: "rdrw_x" }), "RUDDER_HOME=1\nRUDDER_WORKER_TOKEN=rdrw_x\n");
  assert.throws(() => renderHomeEnv({ RUDDER_WORKER_TOKEN: "rdrw_x\nEVIL=1" }));
  assert.throws(() => renderHomeEnv({ lower: "x", RUDDER_WORKER_TOKEN: "rdrw_x" }));
  assert.throws(() => renderHomeEnv({ RUDDER_HOME: "1" }), /worker token/);
  assert.ok(validImage(NEW_IMAGE));
  assert.ok(!validImage("img; reboot"));
  assert.equal(hostFilesDir(), hostDir);
  for (const name of HOST_FILES) {
    assert.ok(fs.existsSync(path.join(hostDir, name)), name);
  }
  assert.ok(JSON.parse(fs.readFileSync(path.join(repoRoot, "package.json"), "utf8")).files.includes("cloud/host/"), "the npm package ships cloud/host");
  assert.match(copyFromFlySteps("ubuntu@box"), /rudder cloud host ubuntu@box --skip-setup/);
});

nodeTest("no em dashes in the new product copy", () => {
  const files = [...HOST_FILES.map((name) => path.join(hostDir, name)), path.join(cloudDir, "src", "home-host.ts"),
    path.join(cloudDir, "worker", "home-host.mjs"), path.join(repoRoot, "src", "cloud-host.ts")];
  for (const file of files) {
    assert.ok(!fs.readFileSync(file, "utf8").includes(String.fromCharCode(0x2014)), file);
  }
});
