import assert from "node:assert/strict";
import { readFileSync, mkdtempSync, rmSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { COMMAND_REGISTRY as registry, commandHelp, commandCompletions, classifyComposerInput } from "../dist/operations/registry.js";
import { applyCommandSurface, printCommandDiscovery } from "../dist/operations/cli.js";
import { agentToolCalls, validateOperationRequest, operationDigest, validateOperationReceipt, dispatchOperation } from "../dist/operations/requests.js";

const fixtures = JSON.parse(readFileSync(new URL("../src/operations/contract-fixtures.json", import.meta.url), "utf8"));
const now = new Date("2026-11-04T12:00:00Z");
const expired = new Date("2027-02-01T00:00:00Z");
const parsed = (command, args = [], flags = {}) => ({ command, args, flags });
const resolve = (command, args = [], flags = {}) => applyCommandSurface(parsed(command, args, flags), now);
const request = (name) => structuredClone(fixtures.find((entry) => entry.name === name).request);
const receipt = (original, state = "accepted") => ({ format_version: 1, operation: original.operation,
  idempotency_key: original.idempotency_key, request_sha256: operationDigest(original),
  kind: registry.operations.find((entry) => entry.id === original.operation).receipt, state, result: null });

test("one versioned registry covers every operation with shared contract vectors", () => {
  assert.equal(registry.format_version, 1);
  assert.equal(new Set(registry.operations.map((entry) => entry.id)).size, registry.operations.length);
  assert.equal(new Set(registry.operations.map((entry) => entry.cli.join(" "))).size, registry.operations.length);
  for (const operation of registry.operations) {
    assert.ok(fixtures.some((entry) => entry.name === operation.id && entry.valid), operation.id);
    assert.ok(operation.capability && operation.scope && operation.arguments && operation.receipt);
  }
  for (const fixture of fixtures) {
    if (fixture.valid) {
      assert.deepEqual(validateOperationRequest(fixture.request), fixture.request, fixture.name);
      assert.equal(operationDigest(fixture.request), fixture.sha256, fixture.name);
    } else assert.throws(() => validateOperationRequest(fixture.request), undefined, fixture.name);
  }
});

test("seven command families and five slash entries advertise canonical actions only", () => {
  assert.equal(registry.families.length, 7);
  assert.equal(registry.slash.length, 5);
  assert.deepEqual(commandCompletions(), ["open", "run", "agent", "change", "project", "auth", "doctor"]);
  for (const alias of registry.aliases) {
    assert.equal(alias.expires, "2027-02-01");
    assert.ok(alias.owner && alias.replacement);
    if (!alias.bare_only) {
      assert.ok(!commandCompletions().includes(alias.name), alias.name);
      assert.ok(!commandHelp().split("\n").some((line) => line.startsWith(`  rudder ${alias.name} `)), alias.name);
    }
  }
  assert.ok(commandCompletions(["agent"]).includes("resume"));
  assert.ok(commandCompletions(["change"]).includes("review"));
  assert.ok(!commandCompletions().includes("engine"));
  assert.match(commandHelp("agent"), /agent send/);
});

test("canonical inspect/control/view/settings/auth adapters retain existing args and flags", () => {
  for (const [family, action, adapter] of [
    ["agent", "list", "runs"], ["agent", "status", "status"], ["agent", "logs", "logs"],
    ["agent", "watch", "watch"], ["agent", "stop", "stop"], ["agent", "attach", "attach"],
    ["agent", "continue", "handoff"], ["agent", "delete", "delete"], ["agent", "report", "report"],
    ["change", "integrate", "merge"], ["change", "sync", "sync"], ["change", "undo", "undo"],
    ["project", "cloud", "cloud"], ["project", "settings", "telemetry"], ["project", "board", "board"],
    ["auth", "login", "login"], ["doctor", "setup", "onboard"], ["doctor", "context", "context"],
  ]) {
    const flags = { json: true, follow: true, force: true, allowDirty: true, format: "json", nonInteractive: true, cwd: "/selected/project" };
    const value = resolve(family, [action, "target", "--existing-option"], flags);
    assert.equal(value.command, adapter);
    assert.deepEqual(value.args, ["target", "--existing-option"]);
    assert.deepEqual(value.flags, flags);
  }
  assert.equal(resolve("open").command, "dashboard");
  assert.deepEqual(resolve("open", ["agent-1"]), parsed("attach", ["agent-1"]));
  assert.equal(resolve("doctor").command, "doctor");
  assert.equal(resolve("auth").command, "login");
});

test("canonical task input chooses an explicit isolated local checkout and retains project flags", () => {
  const flags = { json: true, cwd: "/project", backend: "codex", model: "chosen", detach: true };
  assert.deepEqual(resolve("run", ["fix the typo"], flags), parsed("run", ["fix the typo"], { ...flags, workspace: true }));
  assert.deepEqual(resolve("fix", ["the", "typo"]), parsed("run", ["fix", "the", "typo"], { workspace: true }));
  assert.deepEqual(resolve("run", ["--location", "local", "--checkout=isolated", "fix it"]), parsed("run", ["fix it"], { workspace: true }));
  assert.throws(() => resolve("run", ["--checkout", "main", "fix it"]), /native task owner/);
  assert.throws(() => resolve("run", ["--location", "any", "fix it"]), /Choose --location/);
  assert.throws(() => resolve("run", ["--location"]), /requires a value/);
});

test("Cloud task launch cannot guess a project attachment or discard selected backend", () => {
  const value = resolve("run", ["--location", "cloud", "fix"]);
  assert.equal(value.command, "sail", "task-only dispatch avoids the cloud project-name heuristic");
  assert.deepEqual(value.args, ["fix"]);
  assert.equal(value.flags.workspace, true);
  assert.throws(() => resolve("run", ["--location=cloud", "fix"], { backend: "codex" }), /cannot preserve/);
  assert.throws(() => resolve("run", ["--location=cloud", "fix"], { model: "selected" }), /cannot preserve/);
});

test("a literal task after -- cannot become location or checkout options", () => {
  const value = applyCommandSurface(parsed("run", ["--location", "cloud", "fix"]), now, ["run", "--", "--location", "cloud", "fix"]);
  assert.equal(value.command, "run");
  assert.deepEqual(value.args, ["--location", "cloud", "fix"]);
  assert.deepEqual(applyCommandSurface(parsed(undefined, ["fix the typo"]), now, ["--", "fix the typo"]), parsed("run", ["fix the typo"], { workspace: true }));
});

test("read-only and decomposed tasks cannot enter a direct implementation adapter", () => {
  for (const task of ["plan only: implement the cache", "just discuss the change", "implement this in parallel"]) {
    assert.throws(() => resolve("run", [task]), /native planning and approval owner/);
    assert.throws(() => resolve("run", ["--location=cloud", task]), /native planning and approval owner/);
  }
});

test("backend start aliases migrate physically to run while retaining compatibility checkout policy", () => {
  for (const backend of ["claude", "codex", "acpx"]) {
    assert.deepEqual(resolve(backend, ["fix it"], { json: true }), parsed("run", ["fix it"], { json: true, backend }));
  }
  assert.deepEqual(resolve("acpx", ["codex", "fix it"]).args, ["fix it"]);
  assert.throws(() => resolve("codex", ["fix"], { backend: "claude" }), /Conflicting backend/);
});

test("every compatibility alias has an enforced exclusive UTC deadline", () => {
  for (const alias of registry.aliases) {
    const args = alias.bare_only ? [] : ["fix it"];
    assert.throws(() => applyCommandSurface(parsed(alias.name, args), expired), /expired on 2027-02-01/, alias.name);
    assert.doesNotThrow(() => applyCommandSurface(parsed(alias.name, args), new Date("2027-01-31T23:59:59.999Z")), alias.name);
  }
  assert.throws(() => applyCommandSurface(parsed("run", ["fix"], {workspace:true}), expired, ["run", "--worktree", "fix"]), /--worktree expired/);
  assert.doesNotThrow(() => applyCommandSurface(parsed("run", ["--worktree"]), expired, ["run", "--", "--worktree"]));
});

test("callbacks including version-skewed internal names preserve their exact arguments", () => {
  for (const command of [...registry.callbacks, "__future_callback"]) {
    const original = parsed(command, ["--unknown", "value", "{\"json\":true}"], { json: true, node: "n8", repo: "/chosen" });
    assert.deepEqual(applyCommandSurface(original, expired), original, command);
  }
});

test("retired names, unsupported scoped actions and unknown slash syntax never launch a task", () => {
  for (const command of [...registry.retired, "/nudge", "/ask", "/run", "/unknown"]) assert.throws(() => resolve(command));
  for (const [family, action] of [["agent", "send"], ["agent", "resume"], ["agent", "move"], ["change", "review"], ["change", "check"]]) {
    assert.throws(() => resolve(family, [action]), /selected surface or authenticated engine tool/);
  }
  assert.throws(() => resolve("agent", ["not-an-action"]), /Choose a agent action/);
});

test("backend composers own slash commands and project commands refuse unknown or expired aliases", () => {
  for (const text of ["/compact keep context", "/goal fix the bug", "/model custom", "/help", "/run backend-command"]) {
    assert.deepEqual(classifyComposerInput(text, "backend", expired), { kind: "backend", text });
  }
  assert.equal(classifyComposerInput("fix tests", "project", now).kind, "text");
  assert.equal(classifyComposerInput("/model", "project", now).kind, "rudder");
  for (const alias of registry.slash_aliases) {
    assert.equal(classifyComposerInput(alias.name, "project", now).kind, "rudder");
    assert.equal(classifyComposerInput(alias.name, "project", expired).kind, "refused");
  }
  for (const command of [...registry.retired_slash, "/compact"]) assert.equal(classifyComposerInput(command, "project", now).kind, "refused");
});

test("ordinary send maps only to the addressed mailbox and an explicit group has bounded exact recipients", () => {
  const single = request("agent.send.v1");
  assert.deepEqual(agentToolCalls(single), [{ tool: "agents_send", arguments: { message: "text", idempotency_key: "request-1", recipient_run: "run-1", recipient_epoch: 3, scope: "task-1" } }]);
  const calls = agentToolCalls(request("explicit_cloud_group"));
  assert.equal(calls.length, 2);
  assert.deepEqual(calls.map((call) => call.arguments.recipient_run), ["run-1", "run-2"]);
  assert.ok(calls.every((call) => call.tool === "agents_send" && !Object.hasOwn(call.arguments, "resume")));
  assert.notEqual(calls[0].arguments.idempotency_key, calls[1].arguments.idempotency_key);
});

test("steer, interrupt and resume are distinct deterministic tools bound to the exact turn", () => {
  for (const action of ["steer", "interrupt", "resume"]) {
    const call = agentToolCalls(request(`agent.${action}.v1`))[0];
    assert.equal(call.tool, `agents_${action}`);
    assert.equal(call.arguments.target_run, "run-1");
    assert.equal(call.arguments.target_epoch, 3);
    assert.equal(call.arguments.expected_thread_id, "text");
    assert.equal(call.arguments.expected_turn_id, "text");
  }
});

test("receipt binds operation, arguments, scope, epochs and idempotency; accepted is not deployed", () => {
  const original = request("agent.send.v1");
  const response = receipt(original);
  assert.equal(validateOperationReceipt(original, response).state, "accepted");
  assert.equal(validateOperationReceipt(original, receipt(original, "effect_unknown")).state, "effect_unknown");
  for (const mutate of [
    (r) => { r.scope.agents[0].run_epoch++; }, (r) => { r.scope.owner_epoch++; },
    (r) => { r.arguments.message = "different"; }, (r) => { r.idempotency_key = "other"; },
  ]) { const changed = structuredClone(original); mutate(changed); assert.throws(() => validateOperationReceipt(changed, response), /differs from original/); }
  assert.throws(() => validateOperationReceipt(original, { ...response, state: "deployed" }), /Invalid operation receipt/);
});

test("dispatch requires authenticated capability and exact scope and never retries an uncertain effect", async () => {
  for (const name of ["agent.stop.v1", "agent.resume.v1", "change.review.v1", "project.settings.v1"]) {
    const original = request(name);
    const capability = registry.operations.find((entry) => entry.id === name).capability;
    let calls = 0;
    const handler = async (value) => { calls++; return receipt(value, "effect_unknown"); };
    await assert.rejects(dispatchOperation(original, { scope: original.scope, capabilities: [] }, handler), /not delegated/);
    await assert.rejects(dispatchOperation(original, { scope: { ...original.scope, owner_epoch: 9 }, capabilities: [capability] }, handler), /authenticated execution scope/);
    assert.equal(calls, 0);
    assert.equal((await dispatchOperation(original, { scope: original.scope, capabilities: [capability] }, handler)).state, "effect_unknown");
    assert.equal(calls, 1);
  }
});

test("discovery handles structured machine output before any setup or launch", () => {
  let output = "";
  assert.equal(printCommandDiscovery(parsed("help", [], { json: true }), (value) => { output += value; }), true);
  assert.deepEqual(JSON.parse(output), registry);
  output = "";
  printCommandDiscovery(parsed("completion", ["agent"], { json: true }), (value) => { output += value; });
  assert.deepEqual(JSON.parse(output), commandCompletions(["agent"]));
});

test("real CLI help/completion and inert callbacks keep JSON framing and exit contracts", () => {
  const root = mkdtempSync("/workspace/v4-command-surface-");
  const entry = fileURLToPath(new URL("../dist/index.js", import.meta.url));
  const env = { ...process.env, HOME: root, CODEX_HOME: path.join(root, ".codex"), CLAUDE_CONFIG_DIR: path.join(root, ".claude"), CI: "1", RUDDER_DISABLE_UPDATE_CHECK: "1", RUDDER_DISABLE_AUTO_UPDATE: "1", RUDDER_DISABLE_TELEMETRY: "1", RUDDER_DISABLE_AGENT_SKILLS: "1" };
  for (const key of Object.keys(env)) if (key.startsWith("RUDDER_") && !key.startsWith("RUDDER_DISABLE_")) delete env[key];
  const cli = (args, input = "") => spawnSync(process.execPath, [entry, ...args], { cwd: root, env, input, encoding: "utf8", timeout: 10000 });
  try {
    for (const args of [["help", "--json"], ["--help", "--json"]]) {
      const result = cli(args); assert.equal(result.status, 0, result.stderr); assert.deepEqual(JSON.parse(result.stdout), registry);
    }
    const completion = cli(["completion", "agent", "--json"]);
    assert.equal(completion.status, 0, completion.stderr);
    assert.deepEqual(JSON.parse(completion.stdout), commandCompletions(["agent"]));
    const version = cli(["--version", "--help"]);
    assert.equal(version.status, 0, version.stderr); assert.match(version.stdout, /^4\./);
    const inert = cli(["__future_callback", "--unknown"]);
    assert.equal(inert.status, 0, inert.stderr); assert.equal(inert.stdout, "");
    const routed = cli(["__route-task"], JSON.stringify({ task: "plan only: build the cache" }));
    assert.equal(routed.status, 0, routed.stderr); assert.equal(JSON.parse(routed.stdout).route, "discuss");
    const refused = cli(["agent", "resume", "stopped-agent", "--json"]);
    assert.equal(refused.status, 1); assert.equal(refused.stdout, ""); assert.match(refused.stderr, /authenticated engine tool/);
    for (const [legacy, canonical] of [["status", ["agent", "status"]], ["runs", ["agent", "list"]]]) {
      const old = cli([legacy, "--json"]);
      const current = cli([...canonical, "--json"]);
      assert.equal(current.status, 0, current.stderr);
      JSON.parse(current.stdout);
      if (Date.now() < expired.getTime()) {
        assert.equal(old.status, current.status, old.stderr);
        assert.equal(old.stdout, current.stdout, "canonical adapter preserves the original JSON bytes");
      } else { assert.equal(old.status, 1); assert.match(old.stderr, /expired on/); }
    }
    const missingTarget = cli(["agent", "stop", "--json"]);
    assert.equal(missingTarget.status, 1); assert.equal(missingTarget.stdout, ""); assert.match(missingTarget.stderr, /Missing run id/);
    if (Date.now() < expired.getTime()) {
      const oldMissingTarget = cli(["stop", "--json"]);
      assert.equal(oldMissingTarget.status, missingTarget.status);
      assert.equal(oldMissingTarget.stdout, missingTarget.stdout);
      assert.equal(oldMissingTarget.stderr, missingTarget.stderr);
    }
  } finally { rmSync(root, { recursive: true, force: true }); }
});
