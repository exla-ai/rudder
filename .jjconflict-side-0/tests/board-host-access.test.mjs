import "./board-source.mjs";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import net from "node:net";
import fsConstants from "node:fs";
import { spawnSync } from "node:child_process";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { boardHostFiles } from "./board-fixture.mjs";
const { BoardAuthority } = await import("../src/board/authority.ts");
const { serveBoardHostAccess, requestBoardOpen, requestBoardBootstrap, inspectBoardHostAccess, openBoardBrowser } = await import("../src/board/host-access.ts");
const { ensureBoardRunning, daemonPidPath } = await import("../src/daemon.ts");
const { writeJson } = await import("../src/util.ts");

async function fixture(t, longHome = false) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "rudder-board-host-"));
  const repo = path.join(root, "repo"), home = path.join(root, longHome ? "long-home-" + "x".repeat(100) : "home"); await fs.mkdir(repo);
  const previous = process.env.RUDDER_HOME; process.env.RUDDER_HOME = home;
  t.after(async () => {
    if (previous === undefined) delete process.env.RUDDER_HOME; else process.env.RUDDER_HOME = previous;
    await fs.rm(root, { recursive: true, force: true });
  });
  return { root, repo, home };
}

function raw(socketPath, body) {
  return new Promise((resolve, reject) => {
    const socket = net.connect(socketPath); let bytes = "";
    socket.once("connect", () => socket.write(JSON.stringify(body) + "\n"));
    socket.on("data", (chunk) => { bytes += chunk; }); socket.once("end", () => resolve(JSON.parse(bytes)));
    socket.once("error", reject);
  });
}

for (const longHome of [false, true]) test(`private host callbacks bind the project and instance, mint fixed targets and return no browser secret (${longHome ? "long" : "default"} home)`, async (t) => {
  const { repo, home } = await fixture(t, longHome); const opened = [];
  const authority = new BoardAuthority("owner", "projector"); authority.bindOrigin("http://127.0.0.1:4774");
  const service = await serveBoardHostAccess(repo, authority, (url) => { opened.push(url); });
  t.after(async () => { await service.close(); authority.close(); });
  const { descriptor: file, socket } = await boardHostFiles(repo, home);
  if (longHome) assert.equal(path.dirname(file), `/tmp/rudder-board-${process.getuid()}`);
  assert.equal((await fs.stat(file)).mode & 0o077, 0); assert.equal((await fs.stat(socket)).mode & 0o077, 0);
  const info = await inspectBoardHostAccess(repo); assert.equal(info.instance_id, authority.instanceId);
  assert.deepEqual(Object.keys(info).sort(), ["control_mode", "format_version", "instance_id", "origin"]);
  assert.equal(await requestBoardOpen(repo, "usage"), undefined); assert.equal(new URL(opened[0]).pathname, "/usage");
  assert.match(new URL(opened[0]).hash, /^#rudder-board-ticket=[a-f0-9]{64}$/);
  assert.equal(new URL(await requestBoardBootstrap(repo, "project")).pathname, "/rudder/owner");
  const body = { format_version: 1, instance_id: authority.instanceId, action: "open", target: "project" };
  for (const invalid of [{ ...body, instance_id: "old" }, { ...body, target: "https://elsewhere.invalid" },
    { ...body, url: "https://elsewhere.invalid" }, { ...body, action: "exec" }]) {
    assert.equal((await raw(socket, invalid)).error, "Board access is unavailable");
  }
  assert.equal(opened.length, 1); assert.doesNotMatch(await fs.readFile(file, "utf8"), /ticket|token|Bearer/);
  await assert.rejects(serveBoardHostAccess(repo, authority, () => {}), /already owns/);
  await fs.chmod(file, 0o644); await assert.rejects(inspectBoardHostAccess(repo)); await fs.chmod(file, 0o600);
  const original = await fs.readFile(file); await fs.unlink(file); await fs.symlink("missing", file);
  await assert.rejects(requestBoardBootstrap(repo)); await fs.unlink(file); await fs.writeFile(file, original, { mode: 0o600 });
  await service.close(); await assert.rejects(fs.stat(file), { code: "ENOENT" }); await assert.rejects(fs.stat(socket), { code: "ENOENT" });
});

test("browser opener errors are reduced to a fixed private result without argv or URL fallback", async (t) => {
  const { repo } = await fixture(t);
  const authority = new BoardAuthority("owner", "projector"); authority.bindOrigin("http://127.0.0.1:4774");
  const service = await serveBoardHostAccess(repo, authority, (url) => { throw new Error(`failed with private argv ${url}`); });
  t.after(async () => { await service.close(); authority.close(); });
  await assert.rejects(requestBoardOpen(repo), (error) => error.message === "Private board access is unavailable");
  const previous = process.env.PATH; process.env.PATH = "/nonexistent-rudder-board-opener";
  try { await assert.rejects(openBoardBrowser("http://127.0.0.1:4774/#rudder-board-ticket=" + "a".repeat(64)),
    (error) => error.message === "Browser opener is unavailable"); }
  finally { if (previous === undefined) delete process.env.PATH; else process.env.PATH = previous; }
});

test("a private FIFO descriptor settles without a writer and regular-file growth cannot bypass the read cap", async (t) => {
  const { repo, home } = await fixture(t);
  const authority = new BoardAuthority("owner", "projector"); authority.bindOrigin("http://127.0.0.1:4774");
  const service = await serveBoardHostAccess(repo, authority, () => {});
  t.after(async () => { await service.close(); authority.close(); });
  const { descriptor: file } = await boardHostFiles(repo, home);
  const original = await fs.readFile(file);
  await fs.unlink(file);
  assert.equal(spawnSync("mkfifo", ["-m", "600", file], { stdio: "ignore" }).status, 0);
  let timer;
  const pending = inspectBoardHostAccess(repo).then(() => "accepted", () => "rejected");
  const settled = await Promise.race([pending, new Promise((resolve) => { timer = setTimeout(() => resolve("blocked"), 500); })]);
  clearTimeout(timer);
  try {
    // A rescue writer lets the pre-fix reader finish too, so the negative
    // regression can fail without leaving a blocked libuv operation behind.
    if (settled === "blocked") {
      const rescue = await fs.open(file, fsConstants.constants.O_RDWR | fsConstants.constants.O_NONBLOCK);
      try { await pending; } finally { await rescue.close(); }
    }
    assert.equal(settled, "rejected", "a FIFO is rejected without waiting for any writer");
  } finally { await fs.unlink(file); await fs.writeFile(file, original, { mode: 0o600 }); }

  const open = fs.open; let expanded = false;
  fs.open = async (...args) => {
    const handle = await open(...args);
    if (args[0] === file) {
      const stat = handle.stat.bind(handle);
      handle.stat = async (...statArgs) => {
        const before = await stat(...statArgs);
        // Whitespace keeps JSON valid: a readFile after the stale stat would
        // silently accept this enlarged descriptor and reach the endpoint.
        if (!expanded) { expanded = true; await fs.appendFile(file, " ".repeat(8192)); }
        return before;
      };
    }
    return handle;
  };
  try { await assert.rejects(inspectBoardHostAccess(repo), /Invalid board descriptor/); assert.ok(expanded); }
  finally { fs.open = open; await fs.writeFile(file, original, { mode: 0o600 }); }
  assert.equal((await inspectBoardHostAccess(repo)).instance_id, authority.instanceId);
});

test("daemon reuse requires the matching private capability protocol and preserves browser/usage open without token env", async (t) => {
  const { root, repo } = await fixture(t);
  const first = await ensureBoardRunning(repo, { port: 0, scheduler: false, open: false });
  t.after(() => first.handle?.close());
  const reused = await ensureBoardRunning(repo, { port: 0, scheduler: false, open: false });
  assert.equal(reused.started, false); assert.equal(reused.port, first.port);
  const bin = path.join(root, "bin"), opened = path.join(root, "opened"); await fs.mkdir(bin);
  const opener = `#!${process.execPath}\nconst fs = require('node:fs');\nconst url = new URL(process.argv[2]);\nif (!/^#rudder-board-ticket=[a-f0-9]{64}$/.test(url.hash) || Object.keys(process.env).some(k => /BOARD.*(?:TOKEN|TICKET|SESSION)/i.test(k))) process.exit(1);\nfs.appendFileSync(${JSON.stringify(opened)}, JSON.stringify({ path: url.pathname }) + '\\n');\n`;
  for (const name of ["open", "xdg-open"]) await fs.writeFile(path.join(bin, name), opener, { mode: 0o700 });
  const previous = process.env.PATH; process.env.PATH = bin + path.delimiter + (previous ?? "");
  try {
    await ensureBoardRunning(repo, { scheduler: false, open: true });
    await requestBoardOpen(repo, "usage");
    let calls = [];
    for (let i = 0; i < 80; i++) {
      calls = (await fs.readFile(opened, "utf8").catch((error) => { if (error.code === "ENOENT") return ""; throw error; }))
        .trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
      if (calls.length === 2) break;
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
    assert.deepEqual(calls.map((call) => call.path).sort(), ["/usage", first.projectUrl.slice(first.url.length)].sort());
  } finally { if (previous === undefined) delete process.env.PATH; else process.env.PATH = previous; }
  const pidPath = daemonPidPath(repo); const current = JSON.parse(await fs.readFile(pidPath, "utf8"));
  await writeJson(pidPath, { ...current, boardAccessVersion: 0 });
  await assert.rejects(ensureBoardRunning(repo, { scheduler: false, open: false }), /older board/);
  await writeJson(pidPath, { ...current, boardInstanceId: "old" });
  await assert.rejects(ensureBoardRunning(repo, { scheduler: false, open: false }), /identity changed/);
  await writeJson(pidPath, current);
});
