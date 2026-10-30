import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Readable, Writable } from "node:stream";
import test from "node:test";
import { classifyTaskRoute, deterministicTaskRoute, runTaskRoutingCommand } from "../dist/task-routing.js";

const fixtures = JSON.parse(await readFile(new URL("./fixtures/task-routing.json", import.meta.url), "utf8"));
const configured = { RUDDER_TASK_ROUTING: "decisions", OPENAI_API_KEY: "test-key-not-a-credential" };
const decisionResponse = (choice, confidence = 0.95) => new Response(JSON.stringify({ answers: [{ type: "choice", name: "task_route", choice, confidence }] }));

test("local routing preserves discussion scope and keeps vague work in one worker", () => {
  for (const { task, route } of fixtures) assert.equal(deterministicTaskRoute(task).route, route, task);
});

test("default and Claude-only routing never need credentials or a network request", async () => {
  const failFetch = async () => { throw new Error("network must not be used"); };
  for (const env of [{}, { OPENAI_API_KEY: "unused" }, { RUDDER_TASK_ROUTING: "decisions" }]) {
    const result = await classifyTaskRoute("fix the typo", { env, fetch: failFetch });
    assert.equal(result.route, "execute");
    assert.equal(result.source, "deterministic");
  }
});

test("explicit discussion and plan-only intent cannot be promoted by the model", async () => {
  let called = false;
  for (const task of ["just discuss implementing the system", "plan only: implement in parallel"]) {
    const result = await classifyTaskRoute(task, { env: configured, fetch: async () => { called = true; return decisionResponse("plan"); } });
    assert.equal(result.route, "discuss");
  }
  assert.equal(called, false);
});

test("opted-in Decisions routing uses typed choice, original task, and bounded deadline", async () => {
  const task = "implement a substantial multi-module feature";
  let calls = 0;
  const result = await classifyTaskRoute(task, {
    env: configured,
    fetch: async (url, options) => {
      calls++;
      assert.equal(url, "https://api.openai.com/v1/decisions");
      assert.equal(options.method, "POST");
      assert.ok(options.signal);
      const body = JSON.parse(options.body);
      assert.equal(body.model, "gpt-6-luna");
      assert.equal(body.input, task);
      assert.equal(body.questions[0].type, "choice");
      assert.deepEqual(body.questions[0].choices.map((x) => x.value), ["discuss", "execute", "plan"]);
      return decisionResponse("plan");
    },
  });
  assert.equal(calls, 1, "ordinary local fallback does not bypass an explicitly enabled classifier");
  assert.equal(result.route, "plan");
  assert.equal(result.source, "decisions");
  assert.match(result.reason, /approval/);
});

test("service failure, low confidence, invalid choices and oversized responses fall back without losing work", async () => {
  for (const fetch of [
    async () => { throw new Error("offline"); },
    async () => new Response("unavailable", { status: 503 }),
    async () => decisionResponse("plan", 0.7),
    async () => decisionResponse("deploy"),
    async () => decisionResponse("plan", 1.5),
    async () => new Response("not json"),
    async () => new Response("x".repeat(40 * 1024)),
    async () => new Response(JSON.stringify({ answers: [{ type: "refusal", name: "task_route" }] })),
  ]) {
    const result = await classifyTaskRoute("fix a typo", { env: configured, fetch });
    assert.equal(result.route, "execute");
    assert.equal(result.source, "deterministic");
  }
});

test("classifier evidence redacts embedded credentials while preserving the original local task", async () => {
  const apiKey = "sk-test-0123456789abcdefghijklmnop";
  const bearer = "abcdefghijklmnopqrstuvwx123456789";
  const task = `fix auth using OPENAI_API_KEY=${apiKey} and Authorization: Bearer ${bearer}`;
  const before = deterministicTaskRoute(task);
  const result = await classifyTaskRoute(task, {
    env: configured,
    fetch: async (_url, options) => {
      const body = JSON.parse(options.body);
      assert.ok(!body.input.includes(apiKey), "embedded key is not classifier evidence");
      assert.ok(!body.input.includes(bearer), "embedded bearer is not classifier evidence");
      assert.match(body.input, /redacted/i);
      return new Response("offline", { status: 503 });
    },
  });
  assert.deepEqual(result, before);
  assert.ok(task.includes(apiKey) && task.includes(bearer), "original task remains available for execution");
});

test("timeout bounds even a transport that ignores AbortSignal", async () => {
  const result = await classifyTaskRoute("fix a typo", { env: configured, timeoutMs: 5, fetch: () => new Promise(() => {}) });
  assert.equal(result.route, "execute");
  assert.equal(result.source, "deterministic");
});

async function command(raw) {
  let body = "";
  const sink = new Writable({ write(chunk, _encoding, callback) { body += chunk.toString(); callback(); } });
  await runTaskRoutingCommand(Readable.from([raw]), sink);
  return JSON.parse(body);
}

test("internal command accepts exactly one bounded plain-task request", async () => {
  assert.equal((await command(JSON.stringify({ task: "just discuss caching" }))).route, "discuss");
  for (const value of [{}, [], null, { task: 5 }, { task: "" }, { task: "fix", route: "deploy" }, { task: "/plan fix" }, { task: "$ rm file" }, { task: "a\0b" }, { task: "x".repeat(65537) }]) {
    await assert.rejects(command(JSON.stringify(value)));
  }
  await assert.rejects(command("x".repeat(512 * 1024 + 1)), /too large/);
});

test("wired internal CLI command runs without jj installation, auth, or network", () => {
  const home = mkdtempSync(path.join(tmpdir(), "rudder-route-command-"));
  try {
    const env = { ...process.env, PATH: "", RUDDER_HOME: home, RUDDER_DISABLE_AUTO_UPDATE: "1", RUDDER_TELEMETRY: "0", RUDDER_TASK_ROUTING: "local" };
    delete env.OPENAI_API_KEY;
    const cli = fileURLToPath(new URL("../dist/index.js", import.meta.url));
    const invoke = (input) => spawnSync(process.execPath, [cli, "__route-task"], { input, env, encoding: "utf8", timeout: 4000 });
    const result = invoke(JSON.stringify({ task: "fix the typo" }));
    assert.equal(result.status, 0, result.stderr);
    assert.equal(JSON.parse(result.stdout).route, "execute");
    const invalid = invoke(JSON.stringify({ task: "/gam fix the typo" }));
    assert.notEqual(invalid.status, 0);
    assert.match(invalid.stderr, /plain tasks/);
    assert.equal(invalid.stdout, "");
  } finally {
    rmSync(home, { recursive: true, force: true });
  }
});
