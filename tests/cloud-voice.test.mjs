// Talking to Rudder (cloud/src/voice.ts): what the model is told, how spoken
// names land on projects and agents, and what each tool call does on the home
// machine. Then the two routes end to end: the session is minted with a fake
// OpenAI, and a tool call reaches a fake home supervisor.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import fs from "node:fs";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { WebSocket } from "ws";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.join(path.resolve(here, ".."), "cloud");
const built = fs.existsSync(path.join(cloudDir, "dist", "voice.js"));
const cloudReady = built && fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3"));
const skip = "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)";
const unitTest = built ? nodeTest : (name, fn) => nodeTest(name, { skip }, fn);
const serverTest = cloudReady ? nodeTest : (name, fn) => nodeTest(name, { skip }, fn);
const voice = built ? await import(path.join(cloudDir, "dist", "voice.js")) : null;

const permissionAsk = { id: "1700000000000-aaaaaaaa", tool: "Bash", kind: "permission", input: { command: "npm publish" }, suggestions: [] };
const questionAsk = {
  id: "1700000000001-bbbbbbbb",
  tool: "AskUserQuestion",
  kind: "question",
  input: {
    questions: [
      { question: "Which database?", options: [{ label: "Postgres" }, { label: "SQLite" }], multiSelect: false },
      { question: "Name of the table?", options: [], multiSelect: false },
    ],
  },
};
const planAsk = { id: "1700000000002-cccccccc", tool: "ExitPlanMode", kind: "plan", input: { plan: "1. Add a route\n2. Test it" } };

function projects() {
  return [
    {
      workspaceId: "ws-libra",
      name: "libra",
      repo: "exla-ai/libra",
      agents: [
        { id: "run-login", title: "Fix login redirect", task: "fix the login redirect loop", status: "working", updatedAt: 3 },
        { id: "run-billing", title: "Billing page polish", status: "waiting", asks: [permissionAsk], updatedAt: 2, lastMessage: "Ready to publish." },
        { id: "run-old", title: "Old docs cleanup", status: "merged", updatedAt: 1 },
      ],
    },
    {
      workspaceId: "ws-shop",
      name: "shop",
      repo: "viraat/shop",
      agents: [
        { id: "run-db", title: "Pick a database", status: "waiting", asks: [questionAsk], updatedAt: 5 },
        { id: "run-plan", title: "Checkout plan", status: "waiting", asks: [planAsk], updatedAt: 4 },
      ],
    },
  ];
}

function fakeContext(list = projects()) {
  const calls = [];
  return {
    calls,
    ctx: {
      projects: () => list,
      rpc: async (method, params) => {
        calls.push({ method, params });
        if (method === "transcript") {
          return { messages: [
            { role: "user", text: "fix the login redirect loop" },
            { role: "tool", tool: { name: "Edit", input: "src/auth.ts" } },
            { role: "assistant", text: "Fixed: the cookie path was wrong.\n\nTests pass." },
          ] };
        }
        if (method === "repos") {
          return { repos: [{ repo: "exla-ai/libra", open: true }, { repo: "exla-ai/rudder", description: "agents" }] };
        }
        return { ok: true };
      },
      openRepo: async (repo) => { calls.push({ method: "openRepo", params: { repo } }); return {}; },
    },
  };
}

unitTest("the model starts with a spoken-friendly snapshot that leads with what needs the user", () => {
  const text = voice.overviewText(projects());
  const lines = text.split("\n");
  assert.match(lines[0], /^Project libra \(exla-ai\/libra\):/);
  assert.match(lines[1], /"Billing page polish": needs the user/, "the agent waiting on a prompt comes first");
  assert.match(text, /waiting on: permission to use Bash: npm publish/);
  assert.match(text, /plus 1 finished or stopped agent \("Old docs cleanup"\)/);
  assert.match(text, /2 questions: 1\. Which database\? Options: Postgres; SQLite\./);
  const driving = voice.voiceInstructions(projects(), "carplay");
  assert.match(driving, /DRIVING/);
  assert.match(driving, /without asking for confirmation/);
  assert.doesNotMatch(driving, /—/, "no em dashes in anything spoken");
  assert.match(voice.voiceInstructions(projects(), "phone", "libra: Billing page polish"), /opened Rudder on: libra: Billing page polish/);
  assert.match(voice.voiceGreeting(projects()), /3 agents need/);
  const session = voice.voiceSessionConfig({ model: "m", voice: "marin", instructions: "x", transcribeModel: "t" });
  assert.equal(session.type, "realtime");
  assert.deepEqual(session.audio.input.format, { type: "audio/pcm", rate: 24000 });
  assert.equal(session.audio.output.voice, "marin");
  assert.equal(session.audio.input.transcription.model, "t");
  assert.ok(session.tools.every((tool) => tool.type === "function" && tool.parameters.type === "object"));
  const minimal = voice.voiceSessionConfig({ model: "m", voice: "marin", instructions: "x", transcribeModel: "t", minimal: true });
  assert.equal(minimal.audio.input.transcription, undefined);
  assert.equal(minimal.audio.input.noise_reduction, undefined);
});

unitTest("spoken names land on the right project and agent, and ambiguity becomes a question", () => {
  const list = projects();
  assert.equal(voice.resolveProject(list, "Libra").workspaceId, "ws-libra");
  assert.equal(voice.resolveProject(list, "viraat/shop").workspaceId, "ws-shop");
  assert.equal(voice.resolveAgent(list[0], "the login one").id, "run-login");
  assert.equal(voice.resolveAgent(list[0], "billing").id, "run-billing");
  assert.equal(voice.resolveAgent(list[0], "").id, "run-billing", "no name means the one that needs you");
  assert.equal(voice.resolveAgent(list[0], "the latest").id, "run-login");
  assert.throws(() => voice.resolveProject(list, ""), /Which project\? "libra" and "shop"/);
  assert.throws(() => voice.resolveProject(list, "kitchen"), /No project called "kitchen"/);
  assert.throws(() => voice.resolveAgent(list[1], ""), /Which agent in shop\?/);
  assert.throws(() => voice.resolveAgent(list[0], "rocket launcher"), /No agent in libra matches/);
});

unitTest("tool calls do on the home machine what the app's buttons do", async () => {
  const { ctx, calls } = fakeContext();

  let result = await voice.runVoiceTool(ctx, "send_message", JSON.stringify({ project: "libra", agent: "login", text: "also add a test" }));
  assert.equal(result.output, "Sent to Fix login redirect in libra.");
  assert.deepEqual(calls.pop(), { method: "send", params: { workspaceId: "ws-libra", runId: "run-login", text: "also add a test" } });

  result = await voice.runVoiceTool(ctx, "start_task", JSON.stringify({ project: "shop", text: "add dark mode" }));
  assert.deepEqual(calls.pop(), { method: "task", params: { workspaceId: "ws-shop", text: "add dark mode" } });

  result = await voice.runVoiceTool(ctx, "answer_prompt", JSON.stringify({ project: "libra", decision: "allow" }));
  assert.match(result.output, /Answered Billing page polish/);
  assert.deepEqual(calls.pop(), { method: "answer", params: { workspaceId: "ws-libra", runId: "run-billing", askId: permissionAsk.id, answer: { behavior: "allow" } } });

  result = await voice.runVoiceTool(ctx, "answer_prompt", JSON.stringify({ project: "shop", agent: "database", decision: "answer_questions", answers: ["sqlite please"] }));
  assert.match(result.output, /2 questions/, "every question needs an answer");
  assert.equal(calls.length, 0);
  await voice.runVoiceTool(ctx, "answer_prompt", JSON.stringify({ project: "shop", agent: "database", decision: "answer_questions", answers: ["sqlite please", "orders"] }));
  assert.deepEqual(calls.pop().params.answer, { answers: { "Which database?": "SQLite", "Name of the table?": "orders" } });

  await voice.runVoiceTool(ctx, "answer_prompt", JSON.stringify({ project: "shop", agent: "checkout", decision: "approve_plan" }));
  assert.deepEqual(calls.pop().params.answer, { approve: true });
  await voice.runVoiceTool(ctx, "answer_prompt", JSON.stringify({ project: "shop", agent: "checkout", decision: "revise_plan", message: "skip step 2" }));
  assert.deepEqual(calls.pop().params.answer, { message: "skip step 2" });

  result = await voice.runVoiceTool(ctx, "answer_prompt", JSON.stringify({ decision: "allow" }));
  assert.match(result.output, /3 agents are waiting/, "with no names and several prompts, ask which");
  assert.equal(calls.length, 0);

  await voice.runVoiceTool(ctx, "agent_action", JSON.stringify({ project: "libra", agent: "login", action: "interrupt" }));
  assert.deepEqual(calls.pop(), { method: "keys", params: { workspaceId: "ws-libra", runId: "run-login", keys: "esc" } });
  result = await voice.runVoiceTool(ctx, "agent_action", JSON.stringify({ project: "libra", agent: "billing", action: "merge" }));
  assert.equal(result.output, "Merging Billing page polish in libra.");
  assert.deepEqual(calls.pop(), { method: "action", params: { workspaceId: "ws-libra", runId: "run-billing", action: "merge" } });

  result = await voice.runVoiceTool(ctx, "get_agent", JSON.stringify({ project: "libra", agent: "login" }));
  assert.match(result.output, /\[Edit\] src\/auth\.ts/);
  assert.match(result.output, /Agent: Fixed: the cookie path was wrong\. Tests pass\./);
  calls.pop();

  result = await voice.runVoiceTool(ctx, "find_repos", JSON.stringify({ query: "rudder" }));
  assert.equal(result.output, "exla-ai/rudder: agents");
  calls.pop();
  await voice.runVoiceTool(ctx, "open_project", JSON.stringify({ repo: "https://github.com/exla-ai/rudder.git" }));
  assert.deepEqual(calls.pop(), { method: "openRepo", params: { repo: "exla-ai/rudder" } });

  assert.deepEqual(await voice.runVoiceTool(ctx, "end_conversation", "{}"), { output: "Ending the conversation.", end: true });
  assert.match((await voice.runVoiceTool(ctx, "send_message", "{not json")).output, /not valid JSON/);
  assert.match((await voice.runVoiceTool(ctx, "launch_rockets", "{}")).output, /no tool called launch_rockets/);
  assert.equal(calls.length, 0);

  const failing = { ...ctx, rpc: async () => { throw new Error("the libra dashboard is starting"); } };
  assert.equal((await voice.runVoiceTool(failing, "start_task", JSON.stringify({ project: "libra", text: "x" }))).output, "That did not work: the libra dashboard is starting");
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

serverTest("a voice session is minted with the account's state, and a tool call reaches the home machine", async (t) => {
  // A stand-in for OpenAI: the first mint is refused (as for an option the
  // model does not take), the retry without extras succeeds.
  const mints = [];
  const openai = http.createServer((req, res) => {
    let body = "";
    req.on("data", (d) => { body += d; });
    req.on("end", () => {
      mints.push({ url: req.url, auth: req.headers.authorization, body: JSON.parse(body) });
      res.setHeader("content-type", "application/json");
      if (mints.length === 1) {
        res.statusCode = 400;
        res.end(JSON.stringify({ error: { message: "Unknown parameter: noise_reduction" } }));
        return;
      }
      res.end(JSON.stringify({ value: "ek_test_secret", expires_at: 1900000000 }));
    });
  });
  await new Promise((resolve) => openai.listen(0, "127.0.0.1", resolve));
  t.after(() => openai.close());

  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-voice-test-"));
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
  const cliToken = "rdr_voice_test_cli";
  const workerToken = "rdrw_voice_test_worker";
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.prepare("insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)")
    .run(tokenHash(cliToken), "acct-voice", "me@example.com", now, now);
  const insert = db.prepare(`
    insert into rudder_workspaces (id, account_id, workspace_key, repo_name, status, machine_id, worker_token_hash,
      source_kind, repo_url, home_id, created_at, updated_at, last_activity_at)
    values (@id, 'acct-voice', @key, @name, 'running', @machine, @hash, @kind, @url, @home, @now, @now, @now)
  `);
  insert.run({ id: "home-voice", key: "home", name: "home", machine: "m-home", hash: tokenHash(workerToken), kind: "home", url: null, home: null, now });
  insert.run({ id: "libra-voice", key: "home:exla-ai/libra", name: "libra", machine: null, hash: tokenHash(workerToken), kind: "home-repo", url: "https://github.com/exla-ai/libra.git", home: "home-voice", now });
  db.close();

  const control = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/home/home-voice/control`, { headers: { authorization: `Bearer ${workerToken}` } });
  t.after(() => control.close());
  const rpcs = [];
  control.on("message", (data) => {
    const message = JSON.parse(data.toString());
    if (message.type === "rpc") {
      rpcs.push(message);
      control.send(JSON.stringify({ type: "rpc-result", id: message.id, result: { ok: true } }));
    }
  });
  await new Promise((resolve, reject) => { control.on("open", resolve); control.on("error", reject); });
  control.send(JSON.stringify({
    type: "state",
    projects: [{
      workspaceId: "libra-voice",
      name: "libra",
      dashboard: "running",
      error: null,
      agents: [{ id: "run-billing", title: "Billing page polish", status: "waiting", asks: [permissionAsk] }],
    }],
  }));

  const auth = { authorization: `Bearer ${cliToken}`, "content-type": "application/json" };
  assert.equal((await fetch(`${base}/api/app/voice/session`, { method: "POST", body: "{}" })).status, 401);

  await waitFor(async () => {
    const state = await (await fetch(`${base}/api/app/state`, { headers: auth })).json();
    return state.projects?.[0]?.agents?.length === 1;
  });
  const minted = await fetch(`${base}/api/app/voice/session`, { method: "POST", headers: auth, body: JSON.stringify({ surface: "carplay" }) });
  assert.equal(minted.status, 200, log);
  const session = await minted.json();
  assert.equal(session.clientSecret, "ek_test_secret");
  assert.equal(session.url, `ws://127.0.0.1:${openai.address().port}/v1/realtime?model=gpt-realtime-2.1`);
  assert.equal(session.voice, "marin");
  assert.match(session.greeting, /one agent needs/);
  assert.equal(mints.length, 2);
  assert.equal(mints[0].auth, "Bearer sk-test-key");
  assert.equal(mints[0].body.session.audio.input.noise_reduction.type, "far_field");
  assert.equal(mints[1].body.session.audio.input.noise_reduction, undefined, "the retry drops the extras");
  assert.match(mints[1].body.session.instructions, /DRIVING/);
  assert.match(mints[1].body.session.instructions, /"Billing page polish": needs the user/);
  assert.ok(mints[1].body.session.tools.some((tool) => tool.name === "answer_prompt"));

  const tool = await fetch(`${base}/api/app/voice/tool`, { method: "POST", headers: auth, body: JSON.stringify({ name: "answer_prompt", arguments: JSON.stringify({ decision: "allow_always" }) }) });
  assert.equal(tool.status, 200);
  assert.deepEqual(await tool.json(), { output: "Answered Billing page polish." });
  const answer = rpcs.find((rpc) => rpc.method === "answer");
  assert.deepEqual(answer.params, { workspaceId: "libra-voice", runId: "run-billing", askId: permissionAsk.id, answer: { behavior: "allow", always: true } });

  const overview = await (await fetch(`${base}/api/app/voice/tool`, { method: "POST", headers: auth, body: JSON.stringify({ name: "get_overview", arguments: "{}" }) })).json();
  assert.match(overview.output, /^Project libra \(exla-ai\/libra\):/);
});
