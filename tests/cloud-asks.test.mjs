import test, { after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { readAsks, askDecision, codexAsks, codexAskSteps } from "../cloud/worker/asks.mjs";

const HOOK = path.resolve("native/src/permission_hook.mjs");

const dirs = [];
after(() => {
  for (const dir of dirs) fs.rmSync(dir, { recursive: true, force: true });
});

function tmpdir() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-asks-"));
  dirs.push(dir);
  return dir;
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

test("a question or plan answered in the terminal is withdrawn though Claude rewrote its input", async () => {
  const dir = tmpdir();
  const asks = path.join(dir, "run.asks");
  const transcript = path.join(dir, "t.jsonl");
  const result = (id) => `${JSON.stringify({ type: "user", message: { content: [{ type: "tool_result", tool_use_id: id }] } })}\n`;
  // The model's key order; the hook gets the schema's (2.1.327).
  const asked = { questions: [{ question: "Which?", header: "Pick", multiSelect: true, options: [{ label: "A", description: "a" }, { label: "B", description: "b" }] }] };
  const parsed = { questions: [{ question: "Which?", header: "Pick", options: [{ label: "A", description: "a" }, { label: "B", description: "b" }], multiSelect: true }] };
  // An older plan, already answered, then the pending one: the transcript has
  // `{}`, the hook the plan file.
  fs.writeFileSync(transcript, toolUseLine("toolu_old", "ExitPlanMode", {}) + "\n" + result("toolu_old")
    + toolUseLine("toolu_q", "AskUserQuestion", asked) + "\n" + toolUseLine("toolu_plan", "ExitPlanMode", {}) + "\n");
  const question = runHook(asks, { transcript_path: transcript, tool_name: "AskUserQuestion", tool_input: parsed });
  const plan = runHook(asks, { transcript_path: transcript, tool_name: "ExitPlanMode", tool_input: { plan: "# Plan", planFilePath: "/p.md" } });
  await waitFor(() => readAsks(asks).length === 2);
  for (const file of fs.readdirSync(asks)) {
    const request = JSON.parse(fs.readFileSync(path.join(asks, file), "utf8"));
    assert.equal(request.toolUseId, request.tool === "ExitPlanMode" ? "toolu_plan" : "toolu_q");
  }
  fs.appendFileSync(transcript, result("toolu_q") + result("toolu_plan"));
  assert.deepEqual(await Promise.all([question.done, plan.done]), [{ code: 0, stdout: "" }, { code: 0, stdout: "" }]);
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
      { question: "Pick a color", header: "Color", multiSelect: false, options: [{ label: "Red" }, { label: "Blue", preview: "#00f" }] },
      { question: "Sizes?", header: "Size", multiSelect: true, options: [{ label: "S" }, { label: "L" }] },
    ],
  };
  const { output } = askDecision({ tool: "AskUserQuestion", input }, { answers: { "Pick a color": "Blue", "Sizes?": ["S", "L"] } });
  const decision = JSON.parse(output).hookSpecificOutput.decision;
  assert.equal(decision.behavior, "allow");
  assert.deepEqual(decision.updatedInput.questions, input.questions);
  assert.deepEqual(decision.updatedInput.answers, { "Pick a color": "Blue", "Sizes?": "S, L" });
  assert.deepEqual(decision.updatedInput.annotations, { "Pick a color": { preview: "#00f" } });
  assert.throws(() => askDecision({ tool: "AskUserQuestion", input }, { answers: { "Pick a color": "Red" } }), /no answer/);
});

test("a plan is approved by key, sent back with a message by deny", () => {
  const plan = { tool: "ExitPlanMode", input: { plan: "1. do it" } };
  assert.deepEqual(askDecision(plan, { approve: true }), { keys: "1" });
  const decision = JSON.parse(askDecision(plan, { approve: false, message: "use plan-b.txt" }).output).hookSpecificOutput.decision;
  assert.equal(decision.behavior, "deny");
  // Said to be the user's, or Claude may take it for an error and go on with its plan.
  assert.match(decision.message, /^The user reviewed this plan in the Rudder app .* the user said:\nuse plan-b\.txt$/s);
});

test("a plain deny carries a reason Claude can act on", () => {
  const decision = JSON.parse(askDecision({ tool: "Bash", input: {} }, { behavior: "deny" }).output).hookSpecificOutput.decision;
  assert.equal(decision.behavior, "deny");
  assert.equal(decision.message, "The user denied this from the Rudder app.");
  const reasoned = JSON.parse(askDecision({ tool: "Bash", input: {} }, { behavior: "deny", message: "Use make clean" }).output).hookSpecificOutput.decision;
  assert.match(reasoned.message, /the user said:\nUse make clean$/);
  assert.throws(() => askDecision({ tool: "Bash", input: {} }, {}), /allow or deny/);
});

// Codex 0.170 rollout records, trimmed from real sessions.
const codexLine = (type, payload, timestamp = "2026-11-18T10:19:53.125Z") => `${JSON.stringify({ timestamp, type, payload })}\n`;
const turnStarted = (turn) => codexLine("event_msg", { type: "task_started", turn_id: turn });
const turnComplete = (turn) => codexLine("event_msg", { type: "task_complete", turn_id: turn });
const askCall = (callId, questions) => codexLine("response_item", { type: "function_call", name: "request_user_input", arguments: JSON.stringify({ questions }), call_id: callId });
const callOutput = (callId) => codexLine("response_item", { type: "function_call_output", call_id: callId, output: "{\"answers\":{}}" });
const planItem = (turn, text) => codexLine("event_msg", { type: "item_completed", turn_id: turn, item: { type: "Plan", id: `${turn}-plan`, text } });
const asyncCall = (callId, questions) => codexLine("response_item", { type: "function_call", name: "request_user_input_async", arguments: JSON.stringify({ questions }), call_id: callId });
const asyncAccepted = (callId, output = "{\"accepted\":true}") => codexLine("response_item", { type: "function_call_output", call_id: callId, output });
const asyncReply = (callId, index, answer) => codexLine("response_item", { type: "message", role: "user", content: [{ type: "input_text", text: `<send_user_message_question_reply>\n${JSON.stringify([{ answer, question: "q", questionItemId: JSON.stringify(["request_user_input_async", callId, index]) }])}\n</send_user_message_question_reply>` }] });
const COLOR = [{ id: "button_color", header: "Button color", question: "Which color should the button be?", options: [{ label: "Red (Recommended)", description: "Use a red button." }, { label: "Blue", description: "Use a blue button." }] }];
const live = { process: { pid: process.pid } };

test("a Codex question waits in the rollout until its call has an output", () => {
  const dir = tmpdir();
  const rollout = path.join(dir, "rollout.jsonl");
  fs.writeFileSync(rollout, turnStarted("t1") + askCall("call_1", COLOR));
  const [ask] = codexAsks(live, rollout);
  assert.equal(ask.kind, "question");
  assert.match(ask.id, /^[0-9]+-[0-9a-f]{8}$/);
  assert.deepEqual(ask.input.questions, [{ question: "Which color should the button be?", header: "Button color", multiSelect: false, options: COLOR[0].options }]);
  // Gone once the Codex process is.
  assert.deepEqual(codexAsks({ process: { pid: 99999999 } }, rollout), []);
  // Answered in the terminal.
  fs.appendFileSync(rollout, callOutput("call_1"));
  assert.deepEqual(codexAsks(live, rollout), []);
  // Esc on the question ends the turn without an output.
  fs.appendFileSync(rollout, turnStarted("t2") + askCall("call_2", COLOR) + codexLine("event_msg", { type: "turn_aborted", turn_id: "t2" }));
  assert.deepEqual(codexAsks(live, rollout), []);
});

test("a Codex plan is a card only while the dashboard sees its Implement menu", () => {
  const dir = tmpdir();
  const rollout = path.join(dir, "rollout.jsonl");
  const menu = { ...live, codexPlanMenu: true };
  fs.writeFileSync(rollout, turnStarted("t1") + planItem("t1", "1. Add README.md\n"));
  assert.deepEqual(codexAsks(menu, rollout), [], "the menu opens when the turn ends");
  fs.appendFileSync(rollout, turnComplete("t1"));
  const [ask] = codexAsks(menu, rollout);
  assert.equal(ask.kind, "plan");
  assert.equal(ask.input.plan, "1. Add README.md\n");
  // Esc on the menu writes nothing to the rollout; the pane no longer shows it.
  assert.deepEqual(codexAsks({ ...live, codexPlanMenu: false }, rollout), []);
  assert.deepEqual(codexAsks(live, rollout), []);
  fs.appendFileSync(rollout, turnStarted("t2"));
  assert.deepEqual(codexAsks(menu, rollout), []);
});

test("an async Codex question waits until each of its questions has a reply or the turn ends", () => {
  const dir = tmpdir();
  const rollout = path.join(dir, "rollout.jsonl");
  const questions = [{ title: "Which editor?", options: ["Vim", "Emacs"] }, { title: "Your motto?" }];
  // Arguments Codex refused never reach the pane.
  fs.writeFileSync(rollout, turnStarted("t1") + asyncCall("call_bad", questions) + asyncAccepted("call_bad", "failed to parse function arguments"));
  assert.deepEqual(codexAsks(live, rollout), []);
  fs.appendFileSync(rollout, asyncCall("call_1", questions) + asyncAccepted("call_1") + asyncCall("call_2", [{ title: "Later?" }]) + asyncAccepted("call_2"));
  const [ask] = codexAsks(live, rollout);
  assert.equal(ask.tool, "request_user_input_async");
  assert.deepEqual(ask.input.questions, [
    { question: "Which editor?", header: "", multiSelect: false, options: [{ label: "Vim", description: "" }, { label: "Emacs", description: "" }] },
    { question: "Your motto?", header: "", multiSelect: false, options: [] },
  ]);
  assert.equal(ask.paneQuestions, 3, "the later call's question sits in the same pane");
  // Answered one at a time, in the terminal or not: the card keeps the rest.
  fs.appendFileSync(rollout, asyncReply("call_1", 0, "Vim"));
  const [rest] = codexAsks(live, rollout);
  assert.notEqual(rest.id, ask.id);
  assert.deepEqual(rest.input.questions.map((q) => q.question), ["Your motto?"]);
  fs.appendFileSync(rollout, asyncReply("call_1", 1, "Ship it"));
  assert.deepEqual(codexAsks(live, rollout).map((a) => a.input.questions[0].question), ["Later?"]);
  // The pane closes with the turn.
  fs.appendFileSync(rollout, turnComplete("t1"));
  assert.deepEqual(codexAsks(live, rollout), []);
});

test("a Codex prompt is answered with keys in its pane", () => {
  const question = { kind: "question", input: { questions: [
    { question: "Color?", options: [{ label: "Red" }, { label: "Blue" }] },
    { question: "Size?", options: [{ label: "S" }, { label: "L" }] },
  ] } };
  const keys = (instruction) => [{ kind: "keys", instruction }];
  assert.deepEqual(codexAskSteps(question, { answers: { "Color?": "Blue", "Size?": "S" } }), keys("down,enter,enter"));
  // Text of your own goes as a note on "None of the above", after the options.
  const hex = Buffer.from("Green, please", "utf8").toString("hex");
  assert.deepEqual(codexAskSteps(question, { answers: { "Color?": "Green, please", "Size?": "L" } }), keys(`down,down,paste:${hex},enter,down,enter`));
  assert.throws(() => codexAskSteps(question, { answers: { "Color?": "Red" } }), /no answer/);
  assert.deepEqual(codexAskSteps({ kind: "plan" }, { approve: true }), keys("1"));
  assert.deepEqual(codexAskSteps({ kind: "plan" }, { approve: false, message: "Use docs/" }), [...keys("esc"), { kind: "steer", instruction: "Use docs/" }]);
  assert.throws(() => codexAskSteps({ kind: "plan" }, { approve: false }), /what should change/);
  // Async questions: back to the main prompt and into the pane first, in a write
  // of its own; then the answers ("Other" after the options, or the only entry).
  const queued = { kind: "question", tool: "request_user_input_async", paneQuestions: 3, input: { questions: [
    { question: "Editor?", options: [{ label: "Vim" }, { label: "Emacs" }] },
    { question: "Motto?", options: [] },
  ] } };
  const motto = Buffer.from("Ship it", "utf8").toString("hex");
  assert.deepEqual(codexAskSteps(queued, { answers: { "Editor?": "Emacs", "Motto?": "Ship it" } }), [
    ...keys("shift-right,shift-right,shift-right,shift-left"),
    ...keys(`down,enter,paste:${motto},enter`),
  ]);
});
