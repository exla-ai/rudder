// The burst build pool's worker side: the builder server (builder.mjs), the
// supervisor broker (burst-broker.mjs) and `rudder-burst` (burst.mjs), run
// locally with the machine's own rsync.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { createBuilderServer } from "../cloud/worker/builder.mjs";
import { startBurstBroker } from "../cloud/worker/burst-broker.mjs";
import { FRAME, buildDir, encodeFrame, frameDecoder, safeKey, sha256Hex, shellJoin, tokenMatches } from "../cloud/worker/burst-protocol.mjs";
import { EXIT, commandLine, execRemote, parseArgs, pullArgs, repoInfo, uploadArgs } from "../cloud/worker/burst.mjs";

const BURST = new URL("../cloud/worker/burst.mjs", import.meta.url).pathname;
const TOKEN = "rdrb_test-token-for-builder";
const rsyncVersion = spawnSync("rsync", ["--version"], { encoding: "utf8" });
const hasRsync = rsyncVersion.status === 0;
// macOS's openrsync ignores receiver-side protection of filtered paths;
// homes and builders run GNU rsync (Debian), which keeps them.
const gnuRsync = hasRsync && !/openrsync/.test(rsyncVersion.stdout);

function tmp(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

async function startBuilder(options = {}) {
  const root = tmp("rudder-builder-");
  const builder = createBuilderServer({ root, tokenSha256: sha256Hex(TOKEN), onIdle() {}, log() {}, ...options });
  await new Promise((resolve) => builder.server.listen(0, "127.0.0.1", resolve));
  const url = `http://127.0.0.1:${builder.server.address().port}`;
  return { root, builder, url, lease: { url, token: TOKEN } };
}

function sink() {
  const chunks = [];
  return { write: (chunk) => chunks.push(Buffer.from(chunk)), text: () => Buffer.concat(chunks).toString("utf8") };
}

test("frames survive arbitrary chunking", () => {
  const frames = [];
  const push = frameDecoder((type, payload) => frames.push([type, payload.toString()]));
  const wire = Buffer.concat([encodeFrame(FRAME.stdout, "hello "), encodeFrame(FRAME.stderr, "warn"), encodeFrame(FRAME.exit, '{"code":0}')]);
  for (let i = 0; i < wire.length; i += 3) push(wire.subarray(i, i + 3));
  assert.deepEqual(frames, [[1, "hello "], [2, "warn"], [3, '{"code":0}']]);
});

test("build paths stay inside the build root and tokens compare by hash", () => {
  assert.equal(safeKey("my repo/../x"), "my-repo-..-x");
  assert.throws(() => safeKey(""), /empty/);
  assert.equal(buildDir("/b", "k", "sub/dir"), "/b/k/sub/dir");
  assert.throws(() => buildDir("/b", "k", "../other"), /leaves/);
  assert.throws(() => buildDir("/b", "k", "/etc"), /leaves/);
  assert.ok(tokenMatches(TOKEN, sha256Hex(TOKEN)));
  assert.ok(!tokenMatches("wrong", sha256Hex(TOKEN)));
  assert.ok(!tokenMatches(TOKEN, "not-a-hash"));
  assert.equal(shellJoin(["cargo", "test", "--", "it's"]), "cargo test -- 'it'\\''s'");
});

test("arguments: options, the command, and refusals", () => {
  const opts = parseArgs(["--pull", "target/release/app", "--env", "RUST_LOG=debug", "--git", "--minutes", "30", "cargo", "build", "--release"]);
  assert.deepEqual(opts.pull, ["target/release/app"]);
  assert.deepEqual(opts.env, { RUST_LOG: "debug" });
  assert.equal(opts.git, true);
  assert.equal(opts.minutes, 30);
  assert.deepEqual(opts.command, ["cargo", "build", "--release"]);
  assert.equal(commandLine(opts.command), "cargo build --release");
  assert.equal(commandLine(["make test && make lint"]), "make test && make lint", "one word is a shell line");
  assert.deepEqual(parseArgs(["--", "--weird-command"]).command, ["--weird-command"]);
  assert.equal(parseArgs(["status"]).action, "status");
  for (const bad of [[], ["--pull", "../escape", "make"], ["--pull", "/abs", "make"], ["--bogus", "make"], ["--env", "NOEQUALS", "make"]]) {
    assert.throws(() => parseArgs(bad), (error) => error.code === EXIT.usage, JSON.stringify(bad));
  }
});

test("worktrees of one repo share a build key; rsync args protect builder outputs", () => {
  const answers = {
    "git rev-parse --show-toplevel": "/home/rudder/.rudder/ws/agent-1",
    "git remote get-url origin": "git@github.com:exla-ai/rudder.git",
  };
  const run = (command, args) => answers[`${command} ${args.join(" ")}`] || "";
  const a = repoInfo("/home/rudder/.rudder/ws/agent-1/native/src", run);
  answers["git rev-parse --show-toplevel"] = "/home/rudder/.rudder/ws/agent-2";
  const b = repoInfo("/home/rudder/.rudder/ws/agent-2", run);
  assert.equal(a.rel, "native/src");
  assert.equal(a.key, b.key);
  assert.match(a.key, /^rudder-[0-9a-f]{8}$/);
  const jjOnly = repoInfo("/w/x", (command, args) => (command === "jj" && args[0] === "root" ? "/w/x" : command === "jj" ? "origin https://github.com/exla-ai/rudder.git\nupstream x" : ""));
  assert.equal(jjOnly.key, a.key.replace(/-[0-9a-f]{8}$/, `-${sha256Hex("https://github.com/exla-ai/rudder.git").slice(0, 8)}`));

  const up = uploadArgs({ root: "/r/", key: "k", git: false, exclude: ["*.log"] }, "RSH");
  assert.ok(up.includes("--delete"));
  assert.ok(up.includes("--filter=:- .gitignore"));
  assert.ok(up.includes("--exclude=/.git"));
  assert.ok(up.includes("--exclude=*.log"));
  assert.deepEqual(up.slice(-2), ["/r/", "burst:k/"]);
  assert.ok(!uploadArgs({ root: "/r", key: "k", git: true, exclude: [] }, "RSH").includes("--exclude=/.git"));
  assert.deepEqual(pullArgs({ root: "/r", key: "k" }, "target/release/app", "RSH"), { dest: "/r/target/release", args: ["-a", "-e", "RSH", "burst:k/target/release/app", "/r/target/release/"] });
  assert.equal(pullArgs({ root: "/r", key: "k" }, "dist/", "RSH").dest, "/r/dist/");
});

test("the builder refuses a missing or wrong token", async () => {
  const { builder, url } = await startBuilder();
  try {
    for (const authorization of [undefined, "Bearer nope"]) {
      const res = await fetch(`${url}/v1/health`, { headers: authorization ? { authorization } : {} });
      assert.equal(res.status, 401);
    }
    const ok = await fetch(`${url}/v1/health`, { headers: { authorization: `Bearer ${TOKEN}` } });
    assert.equal(ok.status, 200);
    assert.equal((await ok.json()).toolchains, "ready");
  } finally {
    builder.close();
  }
});

test("exec streams stdout and stderr separately and returns the exit code", async () => {
  const { builder, root, lease } = await startBuilder();
  try {
    fs.mkdirSync(path.join(root, "k", "sub"), { recursive: true });
    const out = sink();
    const err = sink();
    const result = await execRemote(lease, { key: "k", cwd: "sub", command: 'pwd; echo "$GREETING"; echo oops >&2; exit 7', env: { GREETING: "hi", PATH: "/nope" } }, { stdout: out, stderr: err });
    assert.equal(result.code, 7);
    assert.deepEqual(out.text().trim().split("\n").map((line) => path.basename(line)), ["sub", "hi"]);
    assert.equal(err.text(), "oops\n");
    await assert.rejects(execRemote(lease, { key: "k", cwd: "../..", command: "true" }), /leaves the build directory/);
    await assert.rejects(execRemote(lease, { key: "never-synced", command: "true" }), /upload first/);
  } finally {
    builder.close();
  }
});

test("exec waits for first-boot toolchains and says so", async () => {
  let finish;
  const toolchains = new Promise((resolve) => { finish = resolve; });
  const { builder, root, lease } = await startBuilder({ toolchains });
  try {
    fs.mkdirSync(path.join(root, "k"));
    const err = sink();
    const running = execRemote(lease, { key: "k", command: "echo done" }, { stdout: sink(), stderr: err });
    await new Promise((resolve) => setTimeout(resolve, 100));
    finish();
    assert.equal((await running).code, 0);
    assert.match(err.text(), /installing toolchains/);
  } finally {
    builder.close();
  }
});

test("a dropped client stops the remote command's whole process group", async () => {
  const { builder, root, lease } = await startBuilder();
  try {
    fs.mkdirSync(path.join(root, "k"));
    const marker = path.join(root, "k", "child.pid");
    const abort = new AbortController();
    const running = execRemote(lease, { key: "k", command: `sh -c 'echo $$ > child.pid; sleep 30' & wait` }, { stdout: sink(), stderr: sink(), signal: abort.signal }).catch((error) => error);
    for (let i = 0; i < 50 && !fs.existsSync(marker); i += 1) await new Promise((resolve) => setTimeout(resolve, 50));
    const pid = Number(fs.readFileSync(marker, "utf8"));
    abort.abort();
    await running;
    let alive = true;
    for (let i = 0; i < 50 && alive; i += 1) {
      await new Promise((resolve) => setTimeout(resolve, 50));
      try {
        process.kill(pid, 0);
      } catch {
        alive = false;
      }
    }
    assert.equal(alive, false, "the grandchild was killed with its group");
  } finally {
    builder.close();
  }
});

test("the rsync endpoint only runs rsync's server side", async () => {
  const { builder, url } = await startBuilder();
  try {
    const attempt = (argv, token = TOKEN) => new Promise((resolve) => {
      const req = http.request(`${url}/v1/rsync`, { headers: { connection: "Upgrade", upgrade: "rudder-rsync", authorization: `Bearer ${token}`, "x-rudder-rsync-argv": Buffer.from(JSON.stringify(argv)).toString("base64url") } });
      req.on("upgrade", (_res, socket) => { socket.destroy(); resolve(101); });
      req.on("response", (res) => resolve(res.statusCode));
      req.on("error", () => resolve("error"));
      req.end();
    });
    assert.equal(await attempt(["sh", "-c", "id"]), 400);
    assert.equal(await attempt(["rsync", "--server", "."], "bad"), 401);
    assert.equal(await attempt(["rsync", "--server", "-logDtpre.iLsfx", ".", "k/"]), 101);
  } finally {
    builder.close();
  }
});

test("the broker forwards with the worker token and passes only lease fields", async () => {
  const seen = [];
  const fetchImpl = async (url, init) => {
    seen.push({ url, method: init.method, authorization: init.headers.authorization, body: init.body ? JSON.parse(init.body) : undefined });
    return new Response(JSON.stringify(url.endsWith("/lease") ? { kind: "granted", leaseId: "l1" } : { ok: true }), { status: url.endsWith("/missing/renew") ? 404 : 200 });
  };
  const socketPath = path.join(fs.mkdtempSync("/tmp/rb-"), "b.sock");
  const server = startBurstBroker({ socketPath, cloudUrl: "https://control.example/", homeId: "home-53c2", workerToken: "rdrw_secret", fetchImpl });
  await new Promise((resolve) => server.once("listening", resolve));
  const call = (method, pathname, body) => new Promise((resolve, reject) => {
    const text = body ? JSON.stringify(body) : "";
    const req = http.request({ socketPath, method, path: pathname, headers: text ? { "content-type": "application/json" } : {} }, (res) => {
      let data = "";
      res.on("data", (chunk) => (data += chunk));
      res.on("end", () => resolve({ status: res.statusCode, body: JSON.parse(data) }));
    });
    req.on("error", reject);
    req.end(text);
  });
  try {
    assert.equal((fs.statSync(socketPath).mode & 0o777), 0o600);
    const granted = await call("POST", "/lease", { seconds: 600, holder: "x", sneaky: "drop me" });
    assert.equal(granted.body.leaseId, "l1");
    assert.equal((await call("POST", "/leases/missing/renew", { seconds: 60 })).status, 404);
    assert.equal((await call("POST", "/leases/l1/release")).status, 200);
    assert.equal((await call("GET", "/builders")).status, 200);
    assert.equal((await call("POST", "/leases/../../x/release")).status, 404);
    assert.deepEqual(seen.map((s) => `${s.method} ${s.url}`), [
      "POST https://control.example/api/rudder/workspace/home-53c2/builders/lease",
      "POST https://control.example/api/rudder/workspace/home-53c2/builders/missing/renew",
      "POST https://control.example/api/rudder/workspace/home-53c2/builders/l1/release",
      "GET https://control.example/api/rudder/workspace/home-53c2/builders",
    ]);
    assert.ok(seen.every((s) => s.authorization === "Bearer rdrw_secret"));
    assert.deepEqual(seen[0].body, { seconds: 600, holder: "x" });
  } finally {
    server.close();
  }
});

test("rudder-burst end to end: upload, run, pull back, release", { skip: hasRsync ? false : "no rsync" }, async () => {
  const { builder, root, url } = await startBuilder();
  const repo = tmp("rudder-burst-repo-");
  fs.mkdirSync(path.join(repo, "src"));
  fs.mkdirSync(path.join(repo, "target"));
  fs.writeFileSync(path.join(repo, ".gitignore"), "target/\n");
  fs.writeFileSync(path.join(repo, "src", "input.txt"), "source\n");
  fs.writeFileSync(path.join(repo, "target", "stale"), "never uploaded\n");
  spawnSync("git", ["init", "-q"], { cwd: repo });
  const control = [];
  const controlServer = http.createServer((req, res) => {
    control.push(`${req.method} ${req.url}`);
    req.resume();
    req.on("end", () => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(req.url.endsWith("/lease") ? { kind: "granted", leaseId: "lease-1", builderId: "bld-t", url, token: TOKEN, warm: true, cpus: 16, memoryMb: 32768 } : { ok: true }));
    });
  });
  await new Promise((resolve) => controlServer.listen(0, "127.0.0.1", resolve));
  const socketPath = path.join(fs.mkdtempSync("/tmp/rb-"), "b.sock");
  const broker = startBurstBroker({ socketPath, cloudUrl: `http://127.0.0.1:${controlServer.address().port}`, homeId: "home-t", workerToken: "rdrw_x" });
  await new Promise((resolve) => broker.once("listening", resolve));
  try {
    const run = (args) => new Promise((resolve) => {
      const child = spawn(process.execPath, [BURST, ...args], { cwd: path.join(repo, "src"), env: { ...process.env, RUDDER_BURST_SOCKET: socketPath } });
      let stdout = "";
      let stderr = "";
      child.stdout.on("data", (chunk) => (stdout += chunk));
      child.stderr.on("data", (chunk) => (stderr += chunk));
      child.on("close", (code) => resolve({ code, stdout, stderr }));
    });
    const first = await run(["--pull", "target/out.txt", "--", "sh", "-c", "cat input.txt; ls ..; mkdir -p ../target && echo built > ../target/out.txt; exit 4"]);
    assert.equal(first.code, 4, first.stderr);
    assert.match(first.stdout, /^source\n/);
    assert.ok(!first.stdout.includes("stale"));
    const key = fs.readdirSync(root)[0];
    assert.ok(!fs.existsSync(path.join(root, key, "target", "stale")), ".gitignored paths stay home");
    assert.equal(fs.readFileSync(path.join(repo, "target", "out.txt"), "utf8"), "built\n");
    assert.deepEqual(control, ["POST /api/rudder/workspace/home-t/builders/lease", "POST /api/rudder/workspace/home-t/builders/lease-1/release"]);

    // A second run deletes sources removed at home and (GNU rsync) keeps the
    // builder's own outputs, which are ignored and so never deleted.
    fs.rmSync(path.join(repo, "src", "input.txt"));
    const second = await run(["ls", ".."]);
    assert.equal(second.code, 0, second.stderr + second.stdout);
    assert.ok(!fs.existsSync(path.join(root, key, "src", "input.txt")));
    if (gnuRsync) assert.equal(fs.readFileSync(path.join(root, key, "target", "out.txt"), "utf8"), "built\n");
  } finally {
    builder.close();
    broker.close();
    controlServer.close();
  }
});
