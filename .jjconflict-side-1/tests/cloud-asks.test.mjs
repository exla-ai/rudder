import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { readAsks, askDecision } from "../cloud/worker/asks.mjs";

const HOOK = path.resolve("native/src/permission_hook.mjs");

function tmpdir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "rudder-asks-"));
}

function waitFor(check, timeoutMs = 5000) {
  return new Promise((resolve, reject) => {
    const started = Date.now();
    const tick = () => {
      const value = check();
      if (value) return resolve(value);
      if (Date.now() - started > timeoutMs) return reject(new Error("timed out"));
      setTimeout(tick, 50);
    };
    tick();
  });
}

function runHook(asks, input) {
  const child = spawn(process.execPath, [HOOK, asks], { stdio: ["pipe", "pipe", "pipe"] });
  let stdout = "";
  child.stdout.on("data", (chunk) => { stdout += chunk; });
  const done = new Promise((resolve) => child.on("exit", (code) => resolve({ code, stdout })));
  child.stdin.end(JSON.stringify(input));
  return { child, done };
}

function toolUseLine(id, name, input) {
  return JSON.stringify({ type: "assistant", message: { role: "assistant", content: [{ type: "tool_use", id, name, input }] } });
}

test("the phone's answer becomes the hook's decision, and the request is withdrawn", async () => {
  const dir = tmpdir();
  const asks = path.join(dir, "run.asks");
  const transcript = path.join(dir, "t.jsonl");
  const input = { command: "rm -rf build", description: "Clean the build" };
  fs.writeFileSync(transcript, `${toolUseLine("toolu_1", "Bash", input)}\n`);
  const { done } = runHook(asks, {
    hook_event_name: "PermissionRequest",
    transcript_path: transcript,
    tool_name: "Bash",
    tool_input: input,
    permission_suggestions: [{ type: "addRules", rules: [{ toolName: "Bash" }], behavior: "allow", destination: "session" }],
  });
  const [ask] = await waitFor(() => readAsks(asks).length && readAsks(asks));
  assert.equal(ask.kind, "permission");
  assert.equal(ask.tool, "Bash");
  assert.deepEqual(ask.input, input);
  assert.equal(ask.suggestions.length, 1);

  const request = JSON.parse(fs.readFileSync(path.join(asks, `${ask.id}.json`), "utf8"));
  assert.equal(request.toolUseId, "toolu_1");
  const { output } = askDecision(request, { behavior: "allow", always: true });
  fs.writeFileSync(path.join(asks, `${ask.id}.answer.json`), output);
  const { code, stdout } = await done;
  assert.equal(code, 0);
  const decision = JSON.parse(stdout).hookSpecificOutput.decision;
  assert.equal(decision.behavior, "allow");
  assert.deepEqual(decision.updatedPermissions, request.suggestions);
  assert.deepEqual(fs.readdirSync(asks), []);
});

test("answered in the terminal: the hook sees the tool's result and leaves without a decision", async () => {
  const dir = tmpdir();
  const asks = path.join(dir, "run.asks");
  const transcript = path.join(dir, "t.jsonl");
  const input = { command: "ls" };
  fs.writeFileSync(transcript, `${toolUseLine("toolu_9", "Bash", input)}\n`);
  const { done } = runHook(asks, { transcript_path: transcript, tool_name: "Bash", tool_input: input });
  await waitFor(() => readAsks(asks).length);
  // A result for another call does not count.
  fs.appendFileSync(transcript, `${JSON.stringify({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "toolu_other" }] } })}\n`);
  await new Promise((resolve) => setTimeout(resolve, 600));
  assert.equal(readAsks(asks).length, 1);
  fs.appendFileSync(transcript, `${JSON.stringify({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: "toolu_9" }] } })}\n`);
  const { code, stdout } = await done;
  assert.equal(code, 0);
  assert.equal(stdout, "");
  assert.deepEqual(fs.readdirSync(asks), []);
});

test("a request whose hook is gone is dropped", () => {
  const dir = tmpdir();
  fs.writeFileSync(path.join(dir, "1700000000000-abcdef12.json"), JSON.stringify({ pid: 99999999, tool: "Bash", input: {} }));
  assert.deepEqual(readAsks(dir), []);
  assert.deepEqual(fs.readdirSync(dir), []);
});

test("questions are answered with the labels, multi-select joined", () => {
  const input = {
    questions: [
      { question: "Pick a color", header: "Color", multiSelect: false, options: [{ label: "Red" }, { label: "Blue" }] },
      { question: "Sizes?", header: "Size", multiSelect: true, options: [{ label: "S" }, { label: "L" }] },
    ],
  };
  const { output } = askDecision({ tool: "AskUserQuestion", input }, { answers: { "Pick a color": "Blue", "Sizes?": ["S", "L"] } });
  const decision = JSON.parse(output).hookSpecificOutput.decision;
  assert.equal(decision.behavior, "allow");
  assert.deepEqual(decision.updatedInput.questions, input.questions);
  assert.deepEqual(decision.updatedInput.answers, { "Pick a color": "Blue", "Sizes?": "S, L" });
  assert.throws(() => askDecision({ tool: "AskUserQuestion", input }, { answers: { "Pick a color": "Red" } }), /no answer/);
});

test("a plan is approved by key, sent back with a message by deny", () => {
  const plan = { tool: "ExitPlanMode", input: { plan: "1. do it" } };
  assert.deepEqual(askDecision(plan, { approve: true }), { keys: "1" });
  const decision = JSON.parse(askDecision(plan, { approve: false, message: "use plan-b.txt" }).output).hookSpecificOutput.decision;
  assert.deepEqual(decision, { behavior: "deny", message: "use plan-b.txt" });
});

test("a plain deny carries a reason Claude can act on", () => {
  const decision = JSON.parse(askDecision({ tool: "Bash", input: {} }, { behavior: "deny" }).output).hookSpecificOutput.decision;
  assert.equal(decision.behavior, "deny");
  assert.match(decision.message, /denied/);
  assert.throws(() => askDecision({ tool: "Bash", input: {} }, {}), /allow or deny/);
});
