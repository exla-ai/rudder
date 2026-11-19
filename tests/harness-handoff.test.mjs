import { test } from "node:test";
import assert from "node:assert/strict";
import { handoffPrompt, handoffRefusal, handoffShown } from "../cloud/worker/handoff.mjs";

const cwd = "/work/repo";
const say = (role, text) => ({ role, text });
const tool = (name, input) => ({ role: "tool", tool: { name, input } });

test("a handoff keeps the newest messages that fit, the files changed and where it stopped", () => {
  const old = Array.from({ length: 40 }, (_, i) => say(i % 2 ? "assistant" : "user", `message ${i} ${"x".repeat(1200)}`));
  const messages = [...old, say("user", "fix the flaky test"), tool("Edit", `${cwd}/src/a.ts`), tool("Bash", "npm test")];
  const lines = [
    JSON.stringify({ type: "assistant", message: { content: [{ type: "tool_use", name: "Edit", input: { file_path: `${cwd}/src/old.ts` } }] } }),
    JSON.stringify({ type: "assistant", message: { content: [{ type: "tool_use", name: "Write", input: { file_path: `${cwd}/src/a.ts` } }, { type: "tool_use", name: "Read", input: { file_path: `${cwd}/README.md` } }] } }),
    '{"type":"assistant","message":{"content":[{"type":"tool_use"', // still being written
  ];
  const text = handoffPrompt({
    goal: "Make the tests pass", messages, lines, backend: "claude", cwd,
    from: "Claude · Opus 5.5", to: "Codex · Sol 6.1", instructions: "Focus on the open bug", working: true,
  });
  assert.match(text, /^Rudder handoff: this chat moved to you from Claude · Opus 5\.5\.\nYou are Codex · Sol 6\.1\./);
  assert.match(text, /## Goal\nMake the tests pass/);
  assert.match(text, /## The user's instructions for this handoff\nFocus on the open bug/);
  assert.match(text, /in the middle of a turn[^\n]*Its last step: Bash npm test\./);
  assert.match(text, /## Files it changed, newest first\n- `src\/a\.ts`\n- `src\/old\.ts`\n\n/, "Read is not a change, and paths are the workspace's");
  assert.match(text, /\(Earlier messages left out\.\)/);
  assert.match(text, /User: fix the flaky test\n\n\(Claude ran: Edit src\/a\.ts; Bash npm test\)/);
  assert.doesNotMatch(text, /message 0 /, "the oldest messages are the ones left out");
  assert.ok(text.length < 20_000, `${text.length} characters`);
  assert.match(text, /carry on with that work\.$/);
});

test("Codex patches name their files, also from a code-mode script", () => {
  const patch = (body) => JSON.stringify({ type: "response_item", payload: { type: "custom_tool_call", name: "exec", input: `text(await tools.apply_patch("*** Begin Patch\\n${body}\\n*** End Patch"))` } });
  const lines = [patch(`*** Update File: ${cwd}/a.rs\\n@@`), patch(`*** Add File: ${cwd}/b.rs\\n+x\\n*** Delete File: ${cwd}/c.rs`)];
  const text = handoffPrompt({ goal: "g", messages: [], lines, backend: "codex", cwd, from: "Codex · Sol 6.1", to: "Claude · Opus 5.5", instructions: "", working: false });
  assert.match(text, /## Files it changed, newest first\n- `c\.rs`\n- `b\.rs`\n- `a\.rs`\n/);
  assert.doesNotMatch(text, /instructions for this handoff/);
  assert.match(text, /waiting for the user\./);
  assert.match(text, /Then wait for the user's next message\.$/);
});

test("the chat shows a handoff as where it came from and what was handed over", () => {
  const text = handoffPrompt({ goal: "Ship it", messages: [say("user", "go")], lines: [], backend: "claude", cwd, from: "Claude · Opus 5.5", to: "Codex · Sol 6.1", instructions: "", working: false });
  // As the dashboard frames it for a worker (native tasks.rs execution_prompt).
  const shown = handoffShown(`${text}\n\nRudder: you work in your own jj workspace. Stay in it.`);
  assert.equal(shown.from, "Claude · Opus 5.5");
  assert.match(shown.summary, /^## Goal\nShip it\n/);
  assert.doesNotMatch(shown.summary, /What to do now|Rudder: you work/);
  assert.equal(handoffShown("fix the flaky test"), null);
});

test("a chat that moves again hands on what the last move handed over", () => {
  const first = handoffPrompt({ goal: "Ship it", messages: [say("user", "go")], lines: [], backend: "claude", cwd, from: "Claude · Opus 5.5", to: "Codex · Sol 6.1", instructions: "", working: false });
  const marker = { role: "system", kind: "handoff", handoff: handoffShown(first) };
  const text = handoffPrompt({ goal: "Ship it", messages: [marker, say("assistant", "Summary: ready")], lines: [], backend: "codex", cwd, from: "Codex · Sol 6.1", to: "Claude · Opus 5.5", instructions: "", working: false });
  assert.match(text, /\(Earlier, Codex took this chat over from Claude · Opus 5\.5, which handed it this:\)\n### Goal\nShip it\n[\s\S]*User: go\n\nCodex: Summary: ready/);
});

test("a chat can't move to a harness the machine's sign-in check found signed out", () => {
  const credentials = {
    claude: { ok: true, account: "a@example.com", plan: "max", detail: null },
    codex: { ok: false, account: "a@example.com", plan: "pro", detail: "login expired" },
  };
  assert.equal(handoffRefusal(credentials, "codex", "Codex"), "Codex isn't signed in on your cloud machine.");
  assert.equal(handoffRefusal(credentials, "claude", "Claude"), null);
  assert.equal(handoffRefusal(null, "codex", "Codex"), null, "before the first check nothing is known");
});

test("OpenCode's edit, write and patch calls name the files it changed", () => {
  const part = (tool, input) => JSON.stringify({ type: "tool", tool, callID: `call_${tool}`, state: { status: "completed", input } });
  const lines = [
    part("edit", { filePath: `${cwd}/src/a.ts`, oldString: "x", newString: "y" }),
    part("read", { filePath: `${cwd}/README.md` }),
    part("apply_patch", { patchText: `*** Begin Patch\n*** Add File: ${cwd}/b.rs\n+x\n*** End Patch` }),
    part("write", { filePath: `${cwd}/src/c.ts`, content: "z" }),
  ];
  const text = handoffPrompt({ goal: "g", messages: [], lines, backend: "opencode", cwd, from: "OpenCode · Big Pickle", to: "Claude · Opus 5.5", instructions: "", working: false });
  assert.match(text, /## Files it changed, newest first\n- `src\/c\.ts`\n- `b\.rs`\n- `src\/a\.ts`\n\n/);
});
