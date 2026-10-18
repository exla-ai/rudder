// Live check of talking to Rudder, the way the phone does it, against the
// REAL OpenAI realtime API (costs a few cents):
//   OPENAI_API_KEY=sk-... node cloud/scripts/voice-smoke.mjs [outDir]
// Boots this checkout's control plane (cloud/dist) with a fake home machine,
// mints a session through /api/app/voice/session, connects with the client
// secret, speaks synthesized questions into it, relays tool calls through
// /api/app/voice/tool, pushes a live update (an agent finishing) through
// /api/app/voice/updates, and saves every spoken reply as a .wav with its
// transcript. Exits non-zero when a step does not happen.
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.resolve(here, "..");
const require = createRequire(path.join(cloudDir, "package.json"));
const { WebSocket } = require("ws");
const Database = require("better-sqlite3");

const key = process.env.OPENAI_API_KEY;
if (!key) {
  console.error("OPENAI_API_KEY is required");
  process.exit(2);
}
const outDir = path.resolve(process.argv[2] || fs.mkdtempSync(path.join(os.tmpdir(), "rudder-voice-")));
fs.mkdirSync(outDir, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const tokenHash = (token) => createHash("sha256").update(token).digest("hex").slice(0, 32);
const log = (...args) => console.log(`[${new Date().toISOString().slice(11, 19)}]`, ...args);

// --- control plane + fake home ---------------------------------------------

const port = await new Promise((resolve) => {
  const srv = net.createServer();
  srv.listen(0, "127.0.0.1", () => { const { port } = srv.address(); srv.close(() => resolve(port)); });
});
const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-voice-cp-"));
const dbPath = path.join(dataDir, "state.sqlite");
const base = `http://127.0.0.1:${port}`;
const server = spawn(process.execPath, [path.join(cloudDir, "dist", "server.js")], {
  cwd: cloudDir,
  env: { ...process.env, PORT: String(port), BETTER_AUTH_URL: base, BETTER_AUTH_SECRET: "voice-smoke-secret-0000", RUDDER_CLOUD_DATA_DIR: dataDir, RUDDER_CLOUD_DB: dbPath, RUDDER_CLOUD_PERSIST_STATE: "0", RUDDER_S3_BUCKET: "", SLACK_BOT_TOKEN: "", FLY_API_TOKEN: "" },
  stdio: ["ignore", "pipe", "pipe"],
});
let serverLog = "";
server.stdout.on("data", (d) => { serverLog += d; });
server.stderr.on("data", (d) => { serverLog += d; });
const cleanup = () => { server.kill("SIGKILL"); fs.rmSync(dataDir, { recursive: true, force: true }); };
process.on("exit", cleanup);
for (let i = 0; i < 200 && !(await fetch(`${base}/health`).catch(() => null))?.ok; i++) await sleep(50);
while (!fs.existsSync(dbPath)) await sleep(50);
await sleep(200);

const cliToken = "rdr_voice_smoke";
const workerToken = "rdrw_voice_smoke";
const now = new Date().toISOString();
const db = new Database(dbPath);
db.prepare("insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)").run(tokenHash(cliToken), "acct", "me@example.com", now, now);
const insert = db.prepare(`insert into rudder_workspaces (id, account_id, workspace_key, repo_name, status, machine_id, worker_token_hash, source_kind, repo_url, home_id, created_at, updated_at, last_activity_at)
  values (@id, 'acct', @key, @name, 'running', @machine, @hash, @kind, @url, @home, @now, @now, @now)`);
insert.run({ id: "home", key: "home", name: "home", machine: "m", hash: tokenHash(workerToken), kind: "home", url: null, home: null, now });
insert.run({ id: "ws-shop", key: "home:viraat/shop", name: "shop", machine: null, hash: tokenHash(workerToken), kind: "home-repo", url: "https://github.com/viraat/shop.git", home: "home", now });
db.close();

const agents = [
  { id: "run-checkout", title: "Stripe checkout flow", task: "add Stripe checkout to the cart page", status: "working", backend: "claude", lastMessage: "Wiring the webhook handler now." },
  { id: "run-search", title: "Product search", task: "add search to the catalog", status: "waiting", backend: "codex",
    asks: [{ id: "1760000000000-aaaaaaaa", tool: "Bash", kind: "permission", input: { command: "npm install meilisearch" }, suggestions: [] }],
    lastMessage: "I need to install meilisearch to index products." },
];
const rpcs = [];
const control = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/home/home/control`, { headers: { authorization: `Bearer ${workerToken}` } });
const publish = () => control.send(JSON.stringify({ type: "state", projects: [{ workspaceId: "ws-shop", name: "shop", dashboard: "running", error: null, agents }] }));
control.on("message", (data) => {
  const message = JSON.parse(data.toString());
  if (message.type !== "rpc") return;
  rpcs.push(message);
  log("home rpc:", message.method, JSON.stringify(message.params));
  if (message.method === "answer") {
    agents[1].asks = [];
    agents[1].status = "working";
    publish();
  }
  const result = message.method === "transcript" ? { messages: [{ role: "assistant", text: agents.find((a) => a.id === message.params.runId)?.lastMessage ?? "" }] } : { ok: true };
  control.send(JSON.stringify({ type: "rpc-result", id: message.id, result }));
});
await new Promise((resolve, reject) => { control.on("open", resolve); control.on("error", reject); });
publish();
await sleep(300);

const auth = { authorization: `Bearer ${cliToken}`, "content-type": "application/json" };
const post = async (route, body) => {
  const res = await fetch(`${base}${route}`, { method: "POST", headers: auth, body: JSON.stringify(body) });
  const json = await res.json();
  if (!res.ok) throw new Error(`${route} ${res.status}: ${json.error}\n${serverLog.slice(-2000)}`);
  return json;
};

// --- speech in -----------------------------------------------------------------

async function speak(text) {
  const res = await fetch("https://api.openai.com/v1/audio/speech", {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ model: "gpt-4o-mini-tts", voice: "ash", input: text, response_format: "pcm" }),
  });
  if (!res.ok) throw new Error(`tts ${res.status}: ${await res.text()}`);
  return Buffer.from(await res.arrayBuffer()); // pcm16 mono 24 kHz
}

function wav(pcm) {
  const header = Buffer.alloc(44);
  header.write("RIFF", 0); header.writeUInt32LE(36 + pcm.length, 4); header.write("WAVE", 8);
  header.write("fmt ", 12); header.writeUInt32LE(16, 16); header.writeUInt16LE(1, 20); header.writeUInt16LE(1, 22);
  header.writeUInt32LE(24000, 24); header.writeUInt32LE(48000, 28); header.writeUInt16LE(2, 32); header.writeUInt16LE(16, 34);
  header.write("data", 36); header.writeUInt32LE(pcm.length, 40);
  return Buffer.concat([header, pcm]);
}

// --- the conversation ----------------------------------------------------------

const session = await post("/api/app/voice/session", { surface: "carplay" });
log("session:", session.model, session.voice, "secret", session.clientSecret.slice(0, 6) + "…", "greeting:", session.greeting);
const ws = new WebSocket(session.url, ["realtime", `openai-insecure-api-key.${session.clientSecret}`], { headers: { authorization: `Bearer ${session.clientSecret}` } });
await new Promise((resolve, reject) => { ws.on("open", resolve); ws.on("error", reject); ws.on("unexpected-response", (_q, r) => reject(new Error(`realtime refused: ${r.statusCode}`))); });
log("connected to realtime");

const replies = [];
let current = null;
let waiters = [];
const errors = [];
const toolCalls = [];
ws.on("message", async (data) => {
  const event = JSON.parse(data.toString());
  switch (event.type) {
    case "response.created":
      current = { audio: [], text: "", tools: [] };
      break;
    case "response.output_audio.delta":
      current?.audio.push(Buffer.from(event.delta, "base64"));
      break;
    case "response.output_audio_transcript.delta":
      if (current) current.text += event.delta;
      break;
    case "conversation.item.input_audio_transcription.completed":
      log("user said:", JSON.stringify(event.transcript.trim()));
      break;
    case "error":
      errors.push(event.error);
      log("ERROR", JSON.stringify(event.error));
      break;
    case "response.done": {
      const reply = current ?? { audio: [], text: "", tools: [] };
      current = null;
      const calls = (event.response?.output ?? []).filter((item) => item.type === "function_call");
      for (const call of calls) {
        log("tool call:", call.name, call.arguments);
        toolCalls.push(call.name);
        const result = await post("/api/app/voice/tool", { name: call.name, arguments: call.arguments });
        log("tool output:", JSON.stringify(result.output).slice(0, 300));
        ws.send(JSON.stringify({ type: "conversation.item.create", item: { type: "function_call_output", call_id: call.call_id, output: result.output } }));
      }
      if (calls.length) {
        ws.send(JSON.stringify({ type: "response.create" }));
        break;
      }
      if (reply.text || reply.audio.length) {
        const pcm = Buffer.concat(reply.audio);
        const file = path.join(outDir, `reply-${replies.length + 1}.wav`);
        fs.writeFileSync(file, wav(pcm));
        replies.push({ text: reply.text.trim(), seconds: pcm.length / 48000, file });
        log(`assistant (${(pcm.length / 48000).toFixed(1)}s audio):`, JSON.stringify(reply.text.trim()));
      }
      const pending = waiters;
      waiters = [];
      pending.forEach((resolve) => resolve());
      break;
    }
    default:
  }
});
const spoken = (timeoutMs = 45_000) => new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("no spoken reply in time")), timeoutMs);
  waiters.push(() => { clearTimeout(timer); resolve(); });
});

async function say(text) {
  log("user:", JSON.stringify(text));
  const pcm = Buffer.concat([await speak(text), Buffer.alloc(48000 * 2)]); // + 2 s of silence for the turn to end
  for (let i = 0; i < pcm.length; i += 4800) { // 100 ms chunks, in real time
    ws.send(JSON.stringify({ type: "input_audio_buffer.append", audio: pcm.subarray(i, i + 4800).toString("base64") }));
    await sleep(100);
  }
  await spoken();
}

const check = (ok, what) => { if (!ok) { console.error(`FAILED: ${what}`); process.exitCode = 1; } else log(`ok: ${what}`); };

ws.send(JSON.stringify({ type: "response.create", response: { instructions: `Greet the user briefly: ${session.greeting}` } }));
await spoken();
check(replies.length === 1 && replies[0].seconds > 0.5, "it speaks first");

let seen = (await post("/api/app/voice/updates", { seen: null })).seen;

await say("Hey, what's going on with my agents?");
check(toolCalls.includes("get_overview") || /search|checkout/i.test(replies.at(-1).text), "it reports status");
check(/search|meili|install/i.test(replies.at(-1).text), "it mentions the agent that needs me");

await say("Yeah, let the search one install that.");
const answered = rpcs.find((rpc) => rpc.method === "answer");
check(answered?.params?.runId === "run-search" && answered?.params?.answer?.behavior === "allow", "the permission was approved on the home machine");

// The checkout agent finishes while we talk: the phone's poll turns it into speech.
agents[0].status = "review";
agents[0].lastMessage = "Checkout works end to end with Stripe test cards; webhook verified.";
publish();
await sleep(500);
const update = await post("/api/app/voice/updates", { seen });
seen = update.seen;
check(!!update.message, "the finished agent is news");
ws.send(JSON.stringify({ type: "conversation.item.create", item: { type: "message", role: "system", content: [{ type: "input_text", text: update.message }] } }));
ws.send(JSON.stringify({ type: "response.create" }));
await spoken();
check(/checkout|stripe/i.test(replies.at(-1).text), "it announces the finished agent unprompted");

await say("Nice. Merge it.");
check(rpcs.some((rpc) => rpc.method === "action" && rpc.params.action === "merge" && rpc.params.runId === "run-checkout"), "the merge went to the home machine");

await say("Thanks, that's all.");
check(toolCalls.includes("end_conversation"), "it ends the conversation when told");

check(errors.length === 0, "no realtime errors");
fs.writeFileSync(path.join(outDir, "transcript.txt"), replies.map((r, i) => `${i + 1}. (${r.seconds.toFixed(1)}s) ${r.text}`).join("\n") + "\n");
log("replies saved in", outDir);
ws.close();
control.close();
process.exit();
