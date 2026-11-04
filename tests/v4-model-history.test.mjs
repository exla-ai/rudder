import assert from "node:assert/strict";
import test from "node:test";
import { mkdtemp, mkdir, writeFile, readFile, rm, readdir } from "node:fs/promises";
import path from "node:path";
import { randomUUID } from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import { prepareSelectedModelProfile, codexProtocolCapabilities, claudeProtocolCapabilities,
  bindModelSession, assertResumeModelSession, saveModelSessionReceipt } from "../dist/model-profile/identity.js";
import { discoverInstalledCodexModels } from "../dist/model-profile/codex-discovery.js";

async function fixture(t) {
  const base = "/workspace/rudder-scratch/n13-model-receipts";
  await mkdir(base, { recursive: true });
  const root = await mkdtemp(path.join(base, "identity-"));
  const old = process.env.RUDDER_HOME; process.env.RUDDER_HOME = root;
  t.after(async () => { if (old === undefined) delete process.env.RUDDER_HOME; else process.env.RUDDER_HOME = old; await rm(root, { recursive: true, force: true }); });
  await writeFile(path.join(root, "auth-profiles.json"), JSON.stringify({ version: 1, profiles: {
    selected: { type: "api_key", provider: "openai", key: "fixture-key-never-in-receipt" },
    claude: { type: "api_key", provider: "anthropic", key: "fixture-anthropic-key" },
  } }), { mode: 0o600 });
  return root;
}
function execution() {
  return { project_id: "project", request_id: randomUUID(), run_id: "run", run_epoch: 1,
    execution_id: randomUUID(), operation_id: randomUUID(), view_id: randomUUID(),
    owner: { owner: "owner", epoch: 1 }, request_digest: "a".repeat(64), supervisor_pid: 123 };
}
const codex = () => codexProtocolCapabilities({ data: [
  { model: "account-model", supportedReasoningEfforts: [{ reasoningEffort: "none" }, { reasoningEffort: "minimal" }, { reasoningEffort: "high" }, { reasoningEffort: "xhigh" }], defaultReasoningEffort: "high" },
  { model: "hidden-model", hidden: true },
] });
const input = { profileId: "selected", backend: "codex", model: "account-model", effort: "max", backendVersion: "installed-version" };

test("selected profile/model/effort/session survive durable restart and reject resume drift", async t => {
  const root = await fixture(t);
  const selected = await prepareSelectedModelProfile(input, codex());
  assert.throws(() => bindModelSession({ ...selected, secret: "never-serialize" }, execution(), { session_id: "session", model: "account-model" }), /fields/);
  assert.throws(() => bindModelSession(selected, { ...execution(), secret: "never-serialize" }, { session_id: "session", model: "account-model" }), /fields/);
  assert.equal(selected.requested_effort, "max"); assert.equal(selected.effort, "xhigh");
  const exec = execution();
  const receipt = bindModelSession(selected, exec, { session_id: "backend-session", model: "account-model", effort: "xhigh" });
  const file = path.join(root, "sessions", "receipt.json");
  await saveModelSessionReceipt(file, receipt);
  const bytes = await readFile(file, "utf8"); assert.ok(!bytes.includes("fixture-key"));
  const restored = JSON.parse(bytes);
  assertResumeModelSession(restored, selected, exec, "backend-session");
  await saveModelSessionReceipt(file, receipt);
  for (const changed of [{ ...selected, profile_id: "other" }, { ...selected, model: "fallback" },
    { ...selected, requested_effort: "xhigh" }, { ...selected, backend_version: "new-version" }]) {
    assert.throws(() => assertResumeModelSession(restored, changed, exec, "backend-session"), /changed/);
  }
  assert.throws(() => assertResumeModelSession(restored, selected, { ...exec, owner: { ...exec.owner, epoch: 2 } }, "backend-session"), /changed/);
  assert.throws(() => assertResumeModelSession(restored, selected, exec, "other-session"), /changed/);
  const replacement = bindModelSession(selected, exec, { session_id: "another-session", model: "account-model", effort: "xhigh" });
  await assert.rejects(saveModelSessionReceipt(file, replacement), /changed/);
  assert.equal(await readFile(file, "utf8"), bytes, "failed resume preserves original recoverable receipt");
  exec.owner.epoch = 99; selected.model = "mutated";
  assert.equal(receipt.execution.owner.epoch, 1); assert.equal(receipt.selection.model, "account-model");
});

test("backend capability and private profile mismatches fail before session use", async t => {
  await fixture(t);
  assert.deepEqual(codex().map(row => row.model), ["account-model"]);
  await assert.rejects(prepareSelectedModelProfile({ ...input, model: "hidden-model" }, codex()), /unavailable/);
  await assert.rejects(prepareSelectedModelProfile({ ...input, effort: "low" }, codex()), /unsupported/);
  await assert.rejects(prepareSelectedModelProfile({ ...input, profileId: "claude" }, codex()), /another backend/);
  await assert.rejects(prepareSelectedModelProfile({ ...input, profileId: "missing" }, codex()), /unavailable/);
  const selected = await prepareSelectedModelProfile(input, codex());
  assert.throws(() => bindModelSession(selected, execution(), { session_id: "session", model: "fallback-model" }), /differs/);
  assert.throws(() => bindModelSession(selected, execution(), { session_id: "session", model: "account-model", effort: "high" }), /differs/);
  const receipt = bindModelSession(selected, execution(), { session_id: "session", model: "account-model" });
  assert.equal(receipt.observed_effort, null, "missing backend effort report stays unproven");
});

test("Claude SDK alias resolution pins actual model and supported effort", async t => {
  await fixture(t);
  const caps = claudeProtocolCapabilities([{ value: "sonnet", resolvedModel: "claude-concrete", supportsEffort: true, supportedEffortLevels: ["low", "high"] }]);
  const selected = await prepareSelectedModelProfile({ ...input, backend: "claude", profileId: "claude", model: "sonnet", effort: "low" }, caps);
  assert.equal(selected.requested_model, "sonnet"); assert.equal(selected.model, "claude-concrete");
  const receipt = bindModelSession(selected, execution(), { session_id: "claude-session", model: "claude-concrete" });
  assert.throws(() => assertResumeModelSession(receipt, { ...selected, model: "claude-next-release" }, receipt.execution, receipt.session_id), /changed/);
  assert.throws(() => claudeProtocolCapabilities([{ value: "sonnet", supportedEffortLevels: ["ultra"] }]), /Invalid/);
});

test("standalone profile helper binds exact private selection to protocol capabilities", async t => {
  const root = await fixture(t);
  const invoke = body => spawnSync(process.execPath, [path.resolve("dist/managed-task-profile-cli.js"), "inspect-model", "selected", "codex"], {
    env: { RUDDER_HOME: root, PATH: "/usr/local/bin:/usr/bin:/bin" }, input: JSON.stringify(body), encoding: "utf8", timeout: 5000,
  });
  const request = { model: "account-model", effort: "max", backendVersion: "installed-version",
    models: { data: [{ model: "account-model", supportedReasoningEfforts: [{ reasoningEffort: "xhigh" }] }] } };
  const result = invoke(request); assert.equal(result.status, 0); assert.equal(result.stderr, "");
  const response = JSON.parse(result.stdout); assert.equal(response.ok, true);
  assert.equal(response.selection.profile_id, "selected"); assert.equal(response.selection.effort, "xhigh");
  assert.ok(!result.stdout.includes("fixture-key"));
  const rejected = invoke({ ...request, effort: "low" });
  assert.equal(rejected.status, 1); assert.equal(JSON.parse(rejected.stdout).ok, false); assert.equal(rejected.stderr, "");
});

test("installed Codex exposes model capabilities without starting a session", { skip: !process.env.MODEL_DISCOVERY_CODEX_BIN }, async t => {
  const root = await fixture(t);
  const home = path.join(root, "empty-codex-home"); await mkdir(home, { mode: 0o700 });
  const result = await discoverInstalledCodexModels(process.env.MODEL_DISCOVERY_CODEX_BIN, home, 15000);
  assert.ok(result.backendVersion.length > 0);
  assert.ok(result.capabilities.length > 0);
  assert.ok(result.capabilities.every(row => row.model && Array.isArray(row.efforts)));
  // This qualifies the installed discovery protocol only. No model turn or
  // production credential was used, so it is not real model execution evidence.
});

test("installed Claude initialization exposes actual SDK model capabilities", { skip: !process.env.MODEL_DISCOVERY_CLAUDE_BIN }, async t => {
  const root = await fixture(t);
  const home = path.join(root, "empty-claude-home"); await mkdir(home, { mode: 0o700 });
  const child = spawn(process.env.MODEL_DISCOVERY_CLAUDE_BIN,
    ["--print", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"],
    { cwd: home, env: { HOME: home, CLAUDE_CONFIG_DIR: home, PATH: "/usr/local/bin:/usr/bin:/bin" }, stdio: ["pipe", "pipe", "ignore"] });
  let finish, reject, bytes = 0, pending = "";
  const result = new Promise((resolve, fail) => { finish = resolve; reject = fail; });
  const stopped = new Promise(resolve => child.once("close", resolve));
  child.on("error", reject); child.stdin.on("error", reject);
  child.once("close", () => reject(new Error("Claude initialization closed without model capabilities.")));
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", data => {
    bytes += Buffer.byteLength(data); if (bytes > 1024 * 1024) { reject(new Error("Claude initialization exceeded its bound.")); return; }
    pending += data;
    let newline;
    while ((newline = pending.indexOf("\n")) >= 0) {
      const line = pending.slice(0, newline); pending = pending.slice(newline + 1);
      try {
        const message = JSON.parse(line);
        if (message.type === "control_response" && message.response?.request_id === "model-discovery") {
          const models = message.response?.response?.models;
          if (!Array.isArray(models)) reject(new Error("Claude initialization did not expose models."));
          else finish(models);
        }
      } catch { reject(new Error("Claude initialization protocol was malformed.")); }
    }
  });
  const timer = setTimeout(() => reject(new Error("Claude initialization timed out.")), 15000);
  child.stdin.write(`${JSON.stringify({ type: "control_request", request_id: "model-discovery", request: { subtype: "initialize" } })}\n`);
  try {
    const capabilities = claudeProtocolCapabilities(await result);
    assert.ok(capabilities.length > 0);
    assert.ok(capabilities.every(row => row.model && Array.isArray(row.efforts)));
  } finally { clearTimeout(timer); child.kill("SIGKILL"); await stopped; }
  // No user prompt/turn or authentication was supplied. This is discovery
  // evidence only, not an authenticated Claude execution qualification.
});


test("managed installed-model helper discovers the protected protocol and removes its private probe", async t => {
  const root = await fixture(t);
  const program = path.join(root, "installed-codex");
  await writeFile(program, `#!${process.execPath}
import readline from 'node:readline';
if (process.argv[2] !== 'app-server') process.exit(2);
for await (const line of readline.createInterface({ input: process.stdin })) {
  const call = JSON.parse(line);
  if (!call.id) continue;
  const result = call.method === 'initialize' ? { userAgent: 'codex/0.166.0' }
    : call.method === 'model/list' ? { data: [{ model: 'actual-installed-model', supportedReasoningEfforts: [{ reasoningEffort: 'high' }] }], nextCursor: null }
    : undefined;
  if (!result) process.exit(3);
  process.stdout.write(JSON.stringify({ id: call.id, result }) + '\\n');
}
`, { mode: 0o700 });
  const invoke = model => spawnSync(process.execPath, [path.resolve("dist/managed-task-profile-cli.js"),
    "discover-model", "selected", "codex", model, program], {
    env: { RUDDER_HOME: root, PATH: "/usr/local/bin:/usr/bin:/bin" }, encoding: "utf8", timeout: 15000,
  });
  const result = invoke("actual-installed-model");
  assert.equal(result.status, 0, result.stdout); assert.equal(result.stderr, "");
  const { selection } = JSON.parse(result.stdout);
  assert.equal(selection.model, "actual-installed-model"); assert.equal(selection.effort, "high");
  assert.equal(selection.backend_version, "codex/0.166.0"); assert.equal(selection.profile_id, "selected");
  assert.ok(!result.stdout.includes("fixture-key"));
  assert.equal(invoke("unavailable-model").status, 1);
  assert.ok(!(await readdir(root)).some(name => name.startsWith("model-probe-")));
});
