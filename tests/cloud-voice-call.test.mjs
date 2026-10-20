// Calling Rudder (cloud/src/voice-call.ts): the signed incoming-call webhook,
// who may call, and a whole call against a fake OpenAI: accept with the SIP
// session, greeting, a tool call answered from the home machine, and the
// hangup after end_conversation.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, createHmac } from "node:crypto";
import { createRequire } from "node:module";
import fs from "node:fs";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { WebSocket, WebSocketServer } from "ws";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.join(path.resolve(here, ".."), "cloud");
const built = fs.existsSync(path.join(cloudDir, "dist", "voice-call.js"));
const cloudReady = built && fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3"));
const skip = "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)";
const unitTest = built ? nodeTest : (name, fn) => nodeTest(name, { skip }, fn);
const serverTest = cloudReady ? nodeTest : (name, fn) => nodeTest(name, { skip }, fn);
const call = built ? await import(path.join(cloudDir, "dist", "voice-call.js")) : null;
const voice = built ? await import(path.join(cloudDir, "dist", "voice.js")) : null;

const secret = `whsec_${Buffer.from("rudder-test-webhook-secret").toString("base64")}`;

function sign(body, { id = "wh_1", timestamp = Math.floor(Date.now() / 1000), key = secret } = {}) {
  const raw = Buffer.from(key.replace(/^whsec_/, ""), "base64");
  const signature = createHmac("sha256", raw).update(`${id}.${timestamp}.${body}`).digest("base64");
  return { "webhook-id": id, "webhook-timestamp": String(timestamp), "webhook-signature": `v1,${signature}` };
}

unitTest("the webhook signature is checked, including its age", () => {
  const body = JSON.stringify({ type: "realtime.call.incoming" });
  const headers = sign(body);
  const asOptions = { id: headers["webhook-id"], timestamp: headers["webhook-timestamp"], signature: headers["webhook-signature"] };
  assert.equal(call.verifyOpenAiWebhook(secret, asOptions, body), true);
  assert.equal(call.verifyOpenAiWebhook(secret, { ...asOptions, signature: `v1,bogus ${asOptions.signature}` }, body), true, "any listed signature may match");
  assert.equal(call.verifyOpenAiWebhook(secret, asOptions, `${body} `), false, "a changed body");
  assert.equal(call.verifyOpenAiWebhook(`whsec_${Buffer.from("other").toString("base64")}`, asOptions, body), false, "another secret");
  assert.equal(call.verifyOpenAiWebhook(secret, { ...asOptions, signature: undefined }, body), false);
  assert.equal(call.verifyOpenAiWebhook("", asOptions, body), false, "no secret, no calls");
  const old = sign(body, { timestamp: Math.floor(Date.now() / 1000) - 600 });
  assert.equal(call.verifyOpenAiWebhook(secret, { id: old["webhook-id"], timestamp: old["webhook-timestamp"], signature: old["webhook-signature"] }, body), false, "a replay");
});

unitTest("the caller comes from the From header and must be on the list", () => {
  const from = (value) => [{ name: "From", value }];
  assert.equal(call.callerNumber(from('"Viraat" <sip:+14255550100@twilio.example>;tag=abc')), "+14255550100");
  assert.equal(call.callerNumber(from("<sip:4255550100@1.2.3.4>")), "+14255550100", "a US number without +1");
  assert.equal(call.callerNumber(from("tel:+44 20 7946 0958")), "+442079460958");
  assert.equal(call.callerNumber(from("<sip:anonymous@anonymous.invalid>")), null);
  assert.equal(call.callerNumber([]), null);

  const phones = call.parseVoicePhones(" +14255550100=github:1 , 2065550111=better-auth:x,broken,=y");
  assert.deepEqual([...phones], [["+14255550100", "github:1"], ["+12065550111", "better-auth:x"]]);

  assert.equal(call.callerIdTrusted([]), true, "no verdict: the number alone");
  assert.equal(call.callerIdTrusted([{ name: "X-Twilio-VerStat", value: "TN-Validation-Passed-A" }]), true);
  assert.equal(call.callerIdTrusted([{ name: "x-twilio-verstat", value: "TN-Validation-Failed" }]), false);

  assert.equal(call.incomingCall({ type: "realtime.call.ended", data: { call_id: "x" } }), null);
  assert.deepEqual(call.incomingCall({ type: "realtime.call.incoming", data: { call_id: "rtc_1", sip_headers: [{ name: "From", value: "a" }, { bad: 1 }] } }),
    { callId: "rtc_1", headers: [{ name: "From", value: "a" }] });
});

unitTest("a call names no audio format and is told it is a phone call", () => {
  const config = voice.voiceSessionConfig({ model: "m", voice: "marin", instructions: "hi", sip: true });
  assert.equal(config.audio.input.format, undefined);
  assert.equal(config.audio.output.format, undefined);
  assert.equal(config.audio.output.voice, "marin");
  assert.equal(config.audio.input.transcription, undefined);
  assert.deepEqual(voice.voiceSessionConfig({ model: "m", voice: "marin", instructions: "hi" }).audio.input.format, { type: "audio/pcm", rate: 24000 });
  assert.match(voice.voiceInstructions([], "call"), /PHONED you/);
});

// ---------------------------------------------------------------------------

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

async function waitFor(fn, timeout = 8000) {
  const start = Date.now();
  for (;;) {
    try {
      const v = await fn();
      if (v) return v;
    } catch {
      // keep polling
    }
    if (Date.now() - start > timeout) throw new Error("waitFor timed out");
    await sleep(50);
  }
}

serverTest("a call from a listed number is answered, runs a tool on the home machine and hangs up after goodbye", async (t) => {
  // A stand-in for OpenAI: the call-control REST routes and the call's socket.
  const requests = [];
  const socketEvents = [];
  let callSocket = null;
  const openai = http.createServer((req, res) => {
    let body = "";
    req.on("data", (d) => { body += d; });
    req.on("end", () => {
      requests.push({ url: req.url, auth: req.headers.authorization, body: body ? JSON.parse(body) : null });
      res.setHeader("content-type", "application/json");
      res.end("{}");
    });
  });
  const wss = new WebSocketServer({ server: openai });
  wss.on("connection", (socket, req) => {
    callSocket = socket;
    socket.upgradeUrl = req.url;
    socket.upgradeAuth = req.headers.authorization;
    socket.on("message", (data) => socketEvents.push(JSON.parse(data.toString())));
  });
  await new Promise((resolve) => openai.listen(0, "127.0.0.1", resolve));
  t.after(() => { wss.close(); openai.close(); });

  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-call-test-"));
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
      OPENAI_API_KEY: "sk-test-key",
      RUDDER_OPENAI_BASE_URL: `http://127.0.0.1:${openai.address().port}`,
      RUDDER_VOICE_CALL_WEBHOOK_SECRET: secret,
      RUDDER_VOICE_PHONES: "+14255550100=acct-call",
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
  const workerToken = "rdrw_call_test_worker";
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  const insert = db.prepare(`
    insert into rudder_workspaces (id, account_id, workspace_key, repo_name, status, machine_id, worker_token_hash,
      source_kind, repo_url, home_id, created_at, updated_at, last_activity_at)
    values (@id, 'acct-call', @key, @name, 'running', @machine, @hash, @kind, @url, @home, @now, @now, @now)
  `);
  insert.run({ id: "home-call", key: "home", name: "home", machine: "m-home", hash: tokenHash(workerToken), kind: "home", url: null, home: null, now });
  insert.run({ id: "libra-call", key: "home:exla-ai/libra", name: "libra", machine: null, hash: tokenHash(workerToken), kind: "home-repo", url: "https://github.com/exla-ai/libra.git", home: "home-call", now });
  db.close();

  const control = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/home/home-call/control`, { headers: { authorization: `Bearer ${workerToken}` } });
  t.after(() => control.close());
  await new Promise((resolve, reject) => { control.on("open", resolve); control.on("error", reject); });
  control.send(JSON.stringify({
    type: "state",
    projects: [{ workspaceId: "libra-call", name: "libra", dashboard: "running", error: null,
      agents: [{ id: "run-billing", title: "Billing page polish", status: "waiting", asks: [] }] }],
  }));
  await sleep(200);

  const post = (event, headers) => {
    const body = JSON.stringify(event);
    return fetch(`${base}/api/voice/call`, { method: "POST", headers: { "content-type": "application/json", ...(headers ?? sign(body)) }, body });
  };
  const incoming = (callId, from, extra = []) => ({
    type: "realtime.call.incoming",
    data: { call_id: callId, sip_headers: [{ name: "From", value: from }, ...extra] },
  });

  // Unsigned, and a stranger: neither reaches the model.
  assert.equal((await post(incoming("rtc_forged", "<sip:+14255550100@x>"), { "webhook-id": "a", "webhook-timestamp": "1", "webhook-signature": "v1,AAAA" })).status, 401);
  assert.equal((await post(incoming("rtc_stranger", "<sip:+19995550100@x>"))).status, 200);
  await waitFor(() => requests.some((r) => r.url === "/v1/realtime/calls/rtc_stranger/reject"));
  assert.ok(!requests.some((r) => r.url.includes("rtc_forged")));
  // The right number with a failed caller-id check is refused too.
  await post(incoming("rtc_spoofed", "<sip:+14255550100@x>", [{ name: "X-Twilio-VerStat", value: "TN-Validation-Failed" }]));
  await waitFor(() => requests.some((r) => r.url === "/v1/realtime/calls/rtc_spoofed/reject"));
  assert.ok(!requests.some((r) => r.url.includes("/accept")));

  // The user calls.
  assert.equal((await post(incoming("rtc_mine", '"V" <sip:+14255550100@twilio>;tag=1'))).status, 200);
  const accept = await waitFor(() => requests.find((r) => r.url === "/v1/realtime/calls/rtc_mine/accept"));
  assert.equal(accept.auth, "Bearer sk-test-key");
  assert.equal(accept.body.type, "realtime");
  assert.equal(accept.body.audio.input.format, undefined, "SIP negotiates the format");
  assert.match(accept.body.instructions, /PHONED you/);
  assert.match(accept.body.instructions, /"Billing page polish"/);
  assert.ok(accept.body.tools.some((tool) => tool.name === "get_overview"));

  await waitFor(() => callSocket);
  assert.equal(callSocket.upgradeUrl, "/v1/realtime?call_id=rtc_mine");
  assert.equal(callSocket.upgradeAuth, "Bearer sk-test-key");
  // The greeting, then a response.
  await waitFor(() => socketEvents.some((e) => e.type === "response.create"));
  assert.equal(socketEvents[0].type, "conversation.item.create");
  assert.equal(socketEvents[0].item.role, "system");

  const say = (event) => callSocket.send(JSON.stringify(event));
  say({ type: "response.created", response: { id: "r1" } });
  say({ type: "response.done", response: { id: "r1", status: "completed", output: [] } });
  // "What's going on?" -> the model looks.
  say({ type: "input_audio_buffer.speech_started" });
  say({ type: "input_audio_buffer.speech_stopped" });
  say({ type: "response.created", response: { id: "r2" } });
  say({ type: "response.done", response: { id: "r2", status: "completed", output: [
    { type: "function_call", status: "completed", name: "get_overview", call_id: "call_1", arguments: "{}" },
  ] } });
  const overview = await waitFor(() => socketEvents.find((e) => e.item?.type === "function_call_output" && e.item.call_id === "call_1"));
  assert.match(overview.item.output, /libra/);
  assert.equal(socketEvents.filter((e) => e.type === "response.create").length, 2, "one response per answered batch");

  // "That's all, bye." -> goodbye plays, then the line hangs up.
  say({ type: "response.created", response: { id: "r3" } });
  say({ type: "output_audio_buffer.started" });
  say({ type: "response.done", response: { id: "r3", status: "completed", output: [
    { type: "function_call", status: "completed", name: "end_conversation", call_id: "call_2", arguments: "{}" },
  ] } });
  await waitFor(() => socketEvents.some((e) => e.item?.call_id === "call_2"));
  await sleep(100);
  assert.ok(!requests.some((r) => r.url === "/v1/realtime/calls/rtc_mine/hangup"), "not over the goodbye");
  say({ type: "output_audio_buffer.stopped" });
  await waitFor(() => requests.some((r) => r.url === "/v1/realtime/calls/rtc_mine/hangup"));
  assert.match(log, /voice call rtc_mine: answering/);
});

serverTest("a deploy mid-call: the caller is told, and the next server rejoins the same call", async (t) => {
  const requests = [];
  const sockets = [];
  const openai = http.createServer((req, res) => {
    let body = "";
    req.on("data", (d) => { body += d; });
    req.on("end", () => {
      requests.push({ url: req.url, body: body ? JSON.parse(body) : null });
      res.setHeader("content-type", "application/json");
      res.end("{}");
    });
  });
  const wss = new WebSocketServer({ server: openai });
  wss.on("connection", (socket, req) => {
    const entry = { socket, url: req.url, events: [] };
    socket.on("message", (data) => entry.events.push(JSON.parse(data.toString())));
    sockets.push(entry);
  });
  await new Promise((resolve) => openai.listen(0, "127.0.0.1", resolve));
  t.after(() => { wss.close(); openai.close(); });

  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-call-restart-"));
  const dbPath = path.join(dataDir, "state.sqlite");
  const base = `http://127.0.0.1:${port}`;
  const children = [];
  t.after(() => {
    for (const child of children) child.kill("SIGKILL");
    fs.rmSync(dataDir, { recursive: true, force: true });
  });
  let log = "";
  const start = async () => {
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
        OPENAI_API_KEY: "sk-test-key",
        RUDDER_OPENAI_BASE_URL: `http://127.0.0.1:${openai.address().port}`,
        RUDDER_VOICE_CALL_WEBHOOK_SECRET: secret,
        RUDDER_VOICE_PHONES: "+14255550100=acct-call",
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    child.stdout.on("data", (d) => { log += d; });
    child.stderr.on("data", (d) => { log += d; });
    children.push(child);
    await waitFor(async () => (await fetch(`${base}/health`).catch(() => null))?.ok);
    return child;
  };

  const first = await start();
  await waitFor(() => fs.existsSync(dbPath));
  await sleep(150);
  const require = createRequire(path.join(cloudDir, "package.json"));
  const Database = require("better-sqlite3");
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.prepare(`
    insert into rudder_workspaces (id, account_id, workspace_key, repo_name, status, machine_id, worker_token_hash,
      source_kind, repo_url, home_id, created_at, updated_at, last_activity_at)
    values ('home-r', 'acct-call', 'home', 'home', 'running', 'm-home', @hash, 'home', null, null, @now, @now, @now)
  `).run({ hash: tokenHash("rdrw_restart"), now });
  db.close();

  const body = JSON.stringify({ type: "realtime.call.incoming", data: { call_id: "rtc_restart", sip_headers: [{ name: "From", value: "<sip:+14255550100@x>" }] } });
  assert.equal((await fetch(`${base}/api/voice/call`, { method: "POST", headers: { "content-type": "application/json", ...sign(body) }, body })).status, 200);
  await waitFor(() => sockets.length === 1 && sockets[0].events.some((e) => e.type === "response.create"));
  const before = sockets[0];
  before.socket.send(JSON.stringify({ type: "response.created", response: { id: "r1" } }));
  before.socket.send(JSON.stringify({ type: "response.done", response: { id: "r1", status: "completed", output: [] } }));
  await sleep(100);

  // Fly stops the machine for a deploy.
  const exited = new Promise((resolve) => first.on("exit", resolve));
  first.kill("SIGINT");
  const notice = await waitFor(() => before.events.find((e) => e.item?.role === "system" && /restarting right now/.test(e.item.content[0].text)));
  assert.ok(notice, "the caller hears why it goes quiet");
  await exited;
  assert.ok(!requests.some((r) => r.url.endsWith("/hangup")), "the call is not hung up");

  // The new server rejoins the call without accepting it again.
  await start();
  await waitFor(() => sockets.length === 2);
  await sleep(1500);
  assert.equal(sockets.length, 2, "rejoined once, not twice");
  const after = sockets[1];
  assert.equal(after.url, "/v1/realtime?call_id=rtc_restart");
  const back = await waitFor(() => after.events.find((e) => e.item?.role === "system"));
  assert.match(back.item.content[0].text, /reconnected now/);
  assert.equal(requests.filter((r) => r.url.endsWith("/accept")).length, 1);
  assert.match(log, /voice call _restart: rejoining after a restart/);

  // When it ends normally it is forgotten: a third server would not rejoin it.
  after.socket.terminate();
  await waitFor(() => /voice call _restart: ended/.test(log.split("rejoining")[1] ?? ""));
  await sleep(100);
  const check = new Database(dbPath, { readonly: true });
  assert.equal(check.prepare("select count(*) as n from rudder_voice_calls").get().n, 0);
  check.close();
});
