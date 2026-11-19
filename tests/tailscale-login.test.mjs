// The app's Tailscale step on the cloud machine (cloud/worker/tailscale-login.mjs),
// against stand-ins for tailscaled and the tailscale CLI that print what the
// real ones do (checked against tailscale 1.104.7): `up` with no auth key
// prints "To authenticate, visit:" and a login.tailscale.com/a/ link on
// stderr, waits, and exits 0 once signed in, or 1 with "timeout waiting for
// Tailscale service to enter a Running state" after --timeout.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { createTailscale } from "../cloud/worker/tailscale-login.mjs";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(fn, timeout = 8000) {
  const start = Date.now();
  for (;;) {
    const value = await fn();
    if (value) return value;
    if (Date.now() - start > timeout) throw new Error("waitFor timed out");
    await sleep(40);
  }
}

// The stand-ins keep the daemon's state in $TS_STUB: daemon-up while
// tailscaled runs, status.json for `status --json`, and the files a test
// touches to play the user (signed-in) or a tailnet admin (approved).
const TAILSCALED = String.raw`
const fs = require("node:fs"), path = require("node:path");
const stub = process.env.TS_STUB;
const statedir = process.argv.find((a) => a.startsWith("--statedir=")).slice(11);
fs.writeFileSync(path.join(stub, "tailscaled.args"), process.argv.slice(2).join(" "));
if (fs.existsSync(path.join(stub, "daemon-fails"))) {
  console.error("2026/11/19 10:00:00 logtail started");
  console.error("2026/11/19 10:00:00 listen udp4 :41641: bind: address already in use");
  process.exit(1);
}
fs.writeFileSync(path.join(statedir, "tailscaled.state"), "{}");
fs.writeFileSync(path.join(stub, "tailscaled.pid"), String(process.pid));
if (!fs.existsSync(path.join(stub, "status.json"))) fs.writeFileSync(path.join(stub, "status.json"), JSON.stringify({ BackendState: "NeedsLogin", Self: { HostName: "box", Online: false } }));
fs.writeFileSync(path.join(stub, "daemon-up"), "");
process.on("SIGTERM", () => { fs.rmSync(path.join(stub, "daemon-up"), { force: true }); process.exit(0); });
setInterval(() => {}, 1000);
`;

const TAILSCALE = String.raw`
const fs = require("node:fs"), path = require("node:path");
const stub = process.env.TS_STUB;
const file = (name) => path.join(stub, name);
let args = process.argv.slice(2);
if (args[0] === "--socket") args = args.slice(2);
fs.appendFileSync(file("calls.log"), args.join(" ") + "\n");
if (!fs.existsSync(file("daemon-up"))) {
  console.error("failed to connect to local tailscaled; it doesn't appear to be running");
  process.exit(1);
}
const status = () => JSON.parse(fs.readFileSync(file("status.json"), "utf8"));
const setStatus = (value) => fs.writeFileSync(file("status.json"), JSON.stringify(value));
const running = { BackendState: "Running", Self: { HostName: "rudder-home", DNSName: "rudder-home.tail1234.ts.net.", Online: true }, CurrentTailnet: { Name: "example.com", MagicDNSSuffix: "tail1234.ts.net" } };
const [cmd, ...rest] = args;
if (cmd === "status") { process.stdout.write(JSON.stringify(status())); process.exit(0); }
if (cmd === "logout") { setStatus({ BackendState: "NeedsLogin", Self: { HostName: "rudder-home", Online: false } }); process.exit(0); }
if (cmd !== "up") { console.error("unexpected " + args.join(" ")); process.exit(2); }
if (status().BackendState === "Running") process.exit(0);
const timeout = parseInt(rest.find((a) => a.startsWith("--timeout=")).slice(10), 10) * 1000;
const mode = fs.existsSync(file("mode")) ? fs.readFileSync(file("mode"), "utf8").trim() : "login";
process.stderr.write("\nTo authenticate, visit:\n\n\thttps://login.tailscale.com/a/" + Math.random().toString(16).slice(2, 14) + "\n\n");
const started = Date.now();
let asked = false;
setInterval(() => {
  if (Date.now() - started > timeout) {
    process.stderr.write("timeout waiting for Tailscale service to enter a Running state; check health with \"tailscale status\"\n");
    process.exit(1);
  }
  if (!fs.existsSync(file("signed-in"))) return;
  if (mode === "refused") { process.stderr.write("backend error: tailnet refused this device\n"); process.exit(1); }
  if (mode === "approval" && !fs.existsSync(file("approved"))) {
    if (!asked) {
      asked = true;
      setStatus({ BackendState: "NeedsMachineAuth", Self: { HostName: "rudder-home", Online: false } });
      process.stderr.write("\nTo approve your machine, visit (as admin):\n\n\thttps://login.tailscale.com/admin/machines\n\n");
    }
    return;
  }
  setStatus(running);
  process.stderr.write("Success.\n");
  process.exit(0);
}, 30);
`;

function setup(t, { tailscaled = true } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-tailscale-"));
  const bin = path.join(dir, "bin");
  const stub = path.join(dir, "stub");
  fs.mkdirSync(bin);
  fs.mkdirSync(stub);
  const script = (name, source) => {
    fs.writeFileSync(path.join(dir, `${name}.cjs`), source);
    fs.writeFileSync(path.join(bin, name), `#!/bin/sh\nexec "${process.execPath}" "${path.join(dir, `${name}.cjs`)}" "$@"\n`, { mode: 0o755 });
  };
  script("tailscale", TAILSCALE);
  if (tailscaled) script("tailscaled", TAILSCALED);
  const saved = { PATH: process.env.PATH, TS_STUB: process.env.TS_STUB };
  process.env.PATH = `${bin}:/usr/bin:/bin`;
  process.env.TS_STUB = stub;
  const instances = [];
  const make = (options = {}) => {
    const changes = [];
    const ts = createTailscale({
      socket: path.join(dir, "tailscaled.sock"),
      stateDir: path.join(dir, "volume", ".tailscale"),
      hostname: "rudder-home",
      onChange: () => changes.push(Date.now()),
      ...options,
    });
    instances.push(ts);
    return ts;
  };
  t.after(() => {
    for (const ts of instances) ts.dispose();
    try { process.kill(Number(fs.readFileSync(path.join(stub, "tailscaled.pid"), "utf8"))); } catch { /* not started */ }
    Object.assign(process.env, saved);
    fs.rmSync(dir, { recursive: true, force: true });
  });
  return {
    make,
    touch: (name, text = "") => fs.writeFileSync(path.join(stub, name), text),
    calls: () => (fs.existsSync(path.join(stub, "calls.log")) ? fs.readFileSync(path.join(stub, "calls.log"), "utf8").trim().split("\n") : []),
    daemonArgs: () => fs.readFileSync(path.join(stub, "tailscaled.args"), "utf8"),
  };
}

test("Connect starts tailscaled, hands back the sign-in link once, and finishes on its own", async (t) => {
  const env = setup(t);
  const ts = env.make();
  await ts.boot();
  // Never signed in: no tailscaled yet, and nothing wrong.
  assert.deepEqual({ ...ts.snapshot(), login: null }, { hostname: "rudder-home", dnsName: null, online: false, backendState: null, tailnetName: null, error: null, login: null });

  const started = await ts.connect();
  assert.equal(started.status, "waiting");
  assert.match(started.url, /^https:\/\/login\.tailscale\.com\/a\/[0-9a-f]+$/);
  assert.match(env.daemonArgs(), /--tun=userspace-networking --statedir=\S+\/volume\/\.tailscale --socket=\S+ --port=41641/);
  const up = env.calls().find((line) => line.startsWith("up"));
  assert.equal(up, "up --hostname=rudder-home --accept-dns=false --timeout=600s");
  // The link goes to the caller only; the state everyone polls says where it is.
  assert.equal(ts.snapshot().login.status, "waiting");
  assert.equal(JSON.stringify(ts.snapshot()).includes("login.tailscale.com"), false);
  // Connect again while it waits: the same sign-in and link, no second `up`.
  const again = await ts.connect();
  assert.equal(again.id, started.id);
  assert.equal(again.url, started.url);
  assert.equal(env.calls().filter((line) => line.startsWith("up")).length, 1);

  env.touch("signed-in");
  await waitFor(() => ts.snapshot().login.status === "done");
  assert.deepEqual({ ...ts.snapshot(), login: null }, {
    hostname: "rudder-home", dnsName: "rudder-home.tail1234.ts.net", online: true, backendState: "Running", tailnetName: "example.com", error: null, login: null,
  });

  // Disconnect logs out, and the step is back to not connected.
  await ts.disconnect();
  assert.equal(env.calls().at(-2), "logout");
  assert.equal(ts.snapshot().online, false);
  assert.equal(ts.snapshot().backendState, "NeedsLogin");

  // A restart rejoins on the node key from the volume: tailscaled again, no `up`.
  env.touch("status.json", JSON.stringify({ BackendState: "Running", Self: { HostName: "rudder-home", DNSName: "rudder-home.tail1234.ts.net.", Online: true }, CurrentTailnet: { Name: "example.com" } }));
  process.kill(Number(fs.readFileSync(path.join(process.env.TS_STUB, "tailscaled.pid"), "utf8")));
  await waitFor(() => !fs.existsSync(path.join(process.env.TS_STUB, "daemon-up")));
  const restarted = env.make();
  const ups = env.calls().filter((line) => line.startsWith("up")).length;
  await restarted.boot();
  assert.equal(restarted.snapshot().online, true);
  assert.equal(env.calls().filter((line) => line.startsWith("up")).length, ups);
});

test("a tailnet that approves devices holds the step at approval until an admin does", async (t) => {
  const env = setup(t);
  env.touch("mode", "approval");
  const ts = env.make();
  await ts.connect();
  env.touch("signed-in");
  await waitFor(() => ts.snapshot().login.status === "approval");
  await waitFor(async () => { await ts.refresh(); return ts.snapshot().backendState === "NeedsMachineAuth"; });
  env.touch("approved");
  await waitFor(() => ts.snapshot().login.status === "done");
  assert.equal(ts.snapshot().online, true);
});

test("an expired link and a refusing tailnet fail with their own reason", async (t) => {
  const env = setup(t);
  const expiring = env.make({ loginTimeoutMs: 1000 });
  await expiring.connect();
  await waitFor(() => expiring.snapshot().login.status === "failed");
  assert.equal(expiring.snapshot().login.error, "The sign-in link was not used within 1 second, so it expired. Connect again for a new one.");

  env.touch("mode", "refused");
  const refused = env.make();
  await refused.connect();
  env.touch("signed-in");
  await waitFor(() => refused.snapshot().login.status === "failed");
  assert.equal(refused.snapshot().login.error, "backend error: tailnet refused this device");
});

test("Connect says why tailscaled is not running", async (t) => {
  const missing = setup(t, { tailscaled: false }).make();
  await assert.rejects(missing.connect(), { message: "tailscaled is not installed on this machine" });
  assert.equal(missing.snapshot().login.error, "tailscaled is not installed on this machine");
});

test("a tailscaled that exits reports its last words", async (t) => {
  const env = setup(t);
  env.touch("daemon-fails");
  const ts = env.make();
  await assert.rejects(ts.connect(), { message: "tailscaled exited (1): listen udp4 :41641: bind: address already in use" });
  await ts.refresh();
  assert.equal(ts.snapshot().error, "tailscaled exited (1): listen udp4 :41641: bind: address already in use");
});
