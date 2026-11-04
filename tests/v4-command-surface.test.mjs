import assert from "node:assert/strict";
import { readFileSync, mkdtempSync, rmSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { v4ScratchPrefix } from './helpers/v4-scratch.mjs';
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

test("about ten public commands; help fits one screen and completion derives from the registry", () => {
  assert.deepEqual(commandCompletions(), ["ls", "attach", "logs", "stop", "login", "logout", "cloud", "doctor", "update"]);
  assert.equal(registry.slash.length, 5);
  const help = commandHelp();
  const lines = help.trimEnd().split("\n");
  assert.ok(lines.length <= 20, help);
  assert.ok(lines.every((line) => line.length <= 80), help);
  assert.ok(!/—/.test(help), "no em dashes");
  assert.match(help, /rudder "task"/);
  assert.match(help, /--cloud/);
  assert.match(help, /--continue/);
  assert.match(help, /rudder --version/);
  for (const hidden of [...registry.developer, ...registry.callbacks, "merge", "improve", "byoc", "run", "watch", "agent"]) {
    assert.ok(!commandCompletions().includes(hidden), hidden);
    assert.ok(!lines.some((line) => line.startsWith(`  rudder ${hidden} `)), hidden);
  }
  assert.deepEqual(commandCompletions(["doctor"]), ["setup", "mouse", "feedback", "context"]);
  assert.deepEqual(commandCompletions(["stop"]), ["--delete"]);
  assert.deepEqual(commandCompletions(["agent"]), []);
});

test("public commands map to existing adapters and keep args and flags", () => {
  for (const [command, adapter] of [["ls", "runs"], ["attach", "attach"], ["logs", "logs"], ["stop", "stop"],
    ["login", "login"], ["logout", "logout"], ["cloud", "cloud"], ["update", "update"]]) {
    const flags = { json: true, cwd: "/selected/project" };
    const value = resolve(command, ["target", "--existing-option"], flags);
    assert.equal(value.command, adapter, command);
    assert.deepEqual(value.args, ["target", "--existing-option"]);
    assert.deepEqual(value.flags, flags);
  }
  assert.equal(resolve("stop", ["a1"], { delete: true }).command, "delete");
  assert.equal(resolve("cloud", ["move"]).command, "cloud");
  assert.deepEqual(resolve("cloud", ["move"]).args, ["move"]);
  assert.equal(resolve("doctor").command, "doctor");
  for (const [action, adapter] of [["setup", "onboard"], ["mouse", "mouse-test"], ["feedback", "feedback"], ["context", "context"]]) {
    assert.deepEqual(resolve("doctor", [action, "x"]), parsed(adapter, ["x"]));
  }
  assert.throws(() => resolve("doctor", ["nope"]), /Unknown doctor action/);
  assert.deepEqual(applyCommandSurface(parsed(undefined, [], { continue: true }), now), parsed("handoff", [], { continue: true }));
  assert.deepEqual(applyCommandSurface(parsed(undefined, []), now), parsed(undefined, []));
});

test("task input starts an isolated agent, locally or with --cloud", () => {
  const flags = { json: true, backend: "codex", model: "chosen", detach: true };
  assert.deepEqual(resolve("fix the typo", [], flags), parsed("run", ["fix the typo"], { ...flags, workspace: true }));
  assert.deepEqual(resolve("fix", ["the", "typo"]), parsed("run", ["fix", "the", "typo"], { workspace: true }));
  assert.deepEqual(resolve("fix it", [], { cloud: true }), parsed("sail", ["fix it"], { cloud: true, workspace: true }));
  assert.throws(() => resolve("fix", [], { cloud: true, backend: "codex" }), /cannot keep/);
  assert.deepEqual(resolve("keep going", [], { continue: true }), parsed("handoff", ["keep going"], { continue: true }));
  for (const task of ["plan only: implement the cache", "just discuss the change", "implement this in parallel"]) {
    assert.throws(() => resolve(task), /plan it in the dashboard/);
  }
});

test("old spellings print one line and run the new behavior", () => {
  const cases = [
    ["run", ["fix it"], "run", ["fix it"], 'rudder "task"'],
    ["claude", ["fix it"], "run", ["fix it"], "--backend claude"],
    ["codex", ["fix it"], "run", ["fix it"], "--backend codex"],
    ["sail", ["fix it"], "sail", ["fix it"], "--cloud"],
    ["watch", ["a1"], "attach", ["a1"], "rudder attach"],
    ["status", [], "runs", [], "rudder ls"],
    ["runs", [], "runs", [], "rudder ls"],
    ["report", [], "runs", [], "rudder ls"],
    ["handoff", ["next"], "handoff", ["next"], "--continue"],
    ["onboard", [], "onboard", [], "doctor setup"],
    ["mouse-test", [], "mouse-test", [], "doctor mouse"],
    ["dashboard", [], "dashboard", [], "use rudder instead"],
    ["open", ["a1"], "attach", ["a1"], "use rudder instead"],
    ["agent", ["list"], "runs", [], "rudder ls"],
    ["agent", ["stop", "a1"], "stop", ["a1"], "rudder stop"],
    ["auth", [], "login", [], "rudder login"],
  ];
  for (const [command, args, adapter, rest, message] of cases) {
    let out = "";
    const value = applyCommandSurface(parsed(command, args), now, undefined, (text) => { out += text; });
    assert.equal(value.command, adapter, command);
    assert.deepEqual(value.args, rest, command);
    assert.equal(out.split("\n").length, 2, command);
    assert.ok(out.startsWith("use ") && out.includes(message) && out.endsWith(" instead\n"), `${command}: ${out}`);
  }
  assert.equal(resolve("codex", ["fix"]).flags.backend, "codex");
  assert.throws(() => resolve("codex", ["fix"], { backend: "claude" }), /Conflicting backend/);
  assert.throws(() => resolve("agent", ["send"]), /Unknown command/);
  assert.throws(() => resolve("change", ["review"]), /Unknown command/);
});

test("every old spelling has an enforced exclusive UTC deadline", () => {
  for (const alias of registry.aliases) {
    const [command, ...first] = alias.name.split(" ");
    const args = alias.bare_only ? [] : [...first, "fix it"];
    const quiet = () => {};
    assert.throws(() => applyCommandSurface(parsed(command, args), expired, undefined, quiet), /expired on 2027-02-01/, alias.name);
    assert.doesNotThrow(() => applyCommandSurface(parsed(command, args), new Date("2027-01-31T23:59:59.999Z"), undefined, quiet), alias.name);
  }
  assert.throws(() => applyCommandSurface(parsed("fix", [], { workspace: true }), expired, ["fix", "--worktree"]), /--worktree expired/);
});

test("callbacks and developer commands pass through with their exact arguments", () => {
  for (const command of [...registry.callbacks, ...registry.developer, "__future_callback"]) {
    const original = parsed(command, ["--unknown", "value", "{\"json\":true}"], { json: true, node: "n8", repo: "/chosen" });
    assert.deepEqual(applyCommandSurface(original, expired), original, command);
  }
});

test("removed names and unknown slash syntax never launch a task", () => {
  for (const command of [...registry.retired, "/nudge", "/ask", "/run", "/unknown"]) assert.throws(() => resolve(command), undefined, command);
  assert.throws(() => resolve("improve"), /was removed/);
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
  const root = mkdtempSync(v4ScratchPrefix('command-surface'));
  const entry = fileURLToPath(new URL("../dist/index.js", import.meta.url));
  const env = { ...process.env, HOME: root, CODEX_HOME: path.join(root, ".codex"), CLAUDE_CONFIG_DIR: path.join(root, ".claude"), CI: "1", RUDDER_DISABLE_UPDATE_CHECK: "1", RUDDER_DISABLE_AUTO_UPDATE: "1", RUDDER_DISABLE_TELEMETRY: "1", RUDDER_DISABLE_AGENT_SKILLS: "1" };
  for (const key of Object.keys(env)) if (key.startsWith("RUDDER_") && !key.startsWith("RUDDER_DISABLE_")) delete env[key];
  const cli = (args, input = "") => spawnSync(process.execPath, [entry, ...args], { cwd: root, env, input, encoding: "utf8", timeout: 10000 });
  try {
    for (const args of [["help", "--json"], ["--help", "--json"]]) {
      const result = cli(args); assert.equal(result.status, 0, result.stderr); assert.deepEqual(JSON.parse(result.stdout), registry);
    }
    const help = cli(["--help"]);
    assert.equal(help.status, 0, help.stderr); assert.equal(help.stdout, commandHelp());
    const completion = cli(["completion", "doctor", "--json"]);
    assert.equal(completion.status, 0, completion.stderr);
    assert.deepEqual(JSON.parse(completion.stdout), commandCompletions(["doctor"]));
    const version = cli(["--version", "--help"]);
    assert.equal(version.status, 0, version.stderr); assert.match(version.stdout, /^4\./);
    const inert = cli(["__future_callback", "--unknown"]);
    assert.equal(inert.status, 0, inert.stderr); assert.equal(inert.stdout, "");
    const routed = cli(["__route-task"], JSON.stringify({ task: "plan only: build the cache" }));
    assert.equal(routed.status, 0, routed.stderr); assert.equal(JSON.parse(routed.stdout).route, "discuss");
    const refused = cli(["agent", "send", "a1", "--json"]);
    assert.equal(refused.status, 1); assert.equal(refused.stdout, ""); assert.match(refused.stderr, /Unknown command/);
    const current = cli(["ls", "--json"]);
    assert.equal(current.status, 0, current.stderr);
    JSON.parse(current.stdout);
    for (const legacy of ["status", "runs"]) {
      const old = cli([legacy, "--json"]);
      if (Date.now() < expired.getTime()) {
        assert.equal(old.status, 0, old.stderr);
        assert.equal(old.stdout, current.stdout, "old spelling runs the new behavior");
        assert.match(old.stderr, /use rudder ls instead/);
      } else { assert.equal(old.status, 1); assert.match(old.stderr, /expired on/); }
    }
    const missingTarget = cli(["stop", "--json"]);
    assert.equal(missingTarget.status, 1); assert.equal(missingTarget.stdout, ""); assert.match(missingTarget.stderr, /Missing run id/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});
