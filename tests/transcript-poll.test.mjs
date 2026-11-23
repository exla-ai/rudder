// The app's chat poll (cloud/worker/transcripts.mjs transcriptPoll): a kept
// read answers while nothing it depended on changed, and any change to one of
// those (the session, a subagent's log, the suggestion file, the agent's
// process) is a new read with a new ETag. Long tool output, a subagent's work
// and a compaction's summary stay out of the poll and come whole from
// transcriptDetail.
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { transcriptDetail, transcriptPoll } from "../cloud/worker/transcripts.mjs";

const line = (entry) => `${JSON.stringify(entry)}\n`;
const at = (seconds) => new Date(Date.UTC(2026, 10, 23, 9, 0, seconds)).toISOString();

test("the chat poll is kept until a file it read or the agent's process changes", async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-transcript-poll-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const agent = spawn(process.execPath, ["-e", "setTimeout(() => {}, 60000)"], { stdio: "ignore" });
  t.after(() => agent.kill("SIGKILL"));
  const home = path.join(root, "home");
  const repo = path.join(root, "repo");
  const session = "5e551011-0000-4000-8000-0000000000aa";
  const runDir = path.join(repo, ".rudder", "runs", "run_p");
  fs.mkdirSync(runDir, { recursive: true });
  fs.writeFileSync(path.join(runDir, "run.json"), JSON.stringify({ id: "run_p", backend: "claude", status: "running", repoRoot: repo, process: { pid: agent.pid }, session: { nativeSessionId: session } }));
  const project = path.join(home, ".claude", "projects", "-x");
  const subagents = path.join(project, session, "subagents");
  fs.mkdirSync(subagents, { recursive: true });
  const file = path.join(project, `${session}.jsonl`);
  const longOutput = "line of build output\n".repeat(200);
  fs.writeFileSync(file, [
    line({ type: "user", uuid: "u1", parentUuid: null, timestamp: at(0), message: { role: "user", content: "build it" } }),
    line({ type: "assistant", uuid: "a1", parentUuid: "u1", timestamp: at(1), message: { model: "claude-opus-5-5", content: [{ type: "tool_use", id: "toolu_build", name: "Bash", input: { command: "npm run build" } }] } }),
    line({ type: "user", uuid: "r1", parentUuid: "a1", timestamp: at(2), message: { role: "user", content: [{ type: "tool_result", tool_use_id: "toolu_build", content: longOutput }] } }),
    line({ type: "assistant", uuid: "a2", parentUuid: "r1", timestamp: at(3), message: { model: "claude-opus-5-5", content: [{ type: "tool_use", id: "toolu_sub", name: "Agent", input: { description: "Check the tests", subagent_type: "Explore", prompt: "Run the tests.", run_in_background: true } }] } }),
    line({ type: "user", uuid: "r2", parentUuid: "a2", timestamp: at(4), message: { role: "user", content: [{ type: "tool_result", tool_use_id: "toolu_sub", content: [{ type: "text", text: "Async agent launched successfully.\nagentId: asub" }] }] }, toolUseResult: { status: "async_launched", agentId: "asub" } }),
  ].join(""));
  fs.writeFileSync(path.join(subagents, "agent-asub.meta.json"), JSON.stringify({ agentType: "Explore", description: "Check the tests", toolUseId: "toolu_sub" }));
  const subLog = path.join(subagents, "agent-asub.jsonl");
  const side = (uuid, parentUuid, type, content, ts) => line({ type, uuid, parentUuid, isSidechain: true, agentId: "asub", timestamp: at(ts), message: { role: type, content } });
  fs.writeFileSync(subLog, side("s1", null, "user", "Run the tests.", 4));

  const poll = () => transcriptPoll(repo, "run_p", { home });
  const first = poll();
  const build = first.body.messages.find((m) => m.id === "toolu_build");
  assert.equal(build.tool.output, null, "a long output stays on the home");
  assert.equal(build.tool.outputChars, longOutput.length);
  const card = first.body.messages.find((m) => m.subagent).subagent;
  assert.equal(card.status, "running");
  assert.equal(card.messages, undefined, "a card's work stays on the home");
  assert.equal(card.prompt, undefined);
  assert.equal(poll().etag, first.etag, "nothing changed");

  // Opened: the whole message, output and work included.
  assert.equal(transcriptDetail(repo, "run_p", { home, id: "toolu_build" }).message.tool.output, longOutput);
  const opened = transcriptDetail(repo, "run_p", { home, id: "toolu_sub" }).message.subagent;
  assert.equal(opened.prompt, "Run the tests.");
  assert.deepEqual(opened.messages, []);
  assert.throws(() => transcriptDetail(repo, "run_p", { home, id: "nope" }), /no longer in this part of the chat/);

  // The subagent works while the agent's own session is still.
  fs.appendFileSync(subLog, side("s2", "s1", "assistant", [{ type: "text", text: "All 12 pass." }], 9));
  const worked = poll();
  assert.notEqual(worked.etag, first.etag, "a subagent's log is watched");
  assert.equal(poll().etag, worked.etag);
  assert.deepEqual(transcriptDetail(repo, "run_p", { home, id: "toolu_sub" }).message.subagent.messages.map((m) => m.text), ["All 12 pass."]);

  // Claude Code's suggested next prompt lands beside the run.
  fs.mkdirSync(path.join(home, ".rudder", "signals"), { recursive: true });
  fs.writeFileSync(path.join(home, ".rudder", "signals", "run_p-suggestion.json"), JSON.stringify({ text: "ship it", at: 1 }));
  const suggested = poll();
  assert.notEqual(suggested.etag, worked.etag);
  assert.equal(suggested.body.suggestion.text, "ship it");

  // The agent's process ends: its running subagent stopped with it.
  agent.kill("SIGKILL");
  await new Promise((resolve) => agent.once("exit", resolve));
  const stopped = poll();
  assert.notEqual(stopped.etag, suggested.etag);
  assert.equal(stopped.body.messages.find((m) => m.subagent).subagent.status, "stopped");

  // The session grows.
  fs.appendFileSync(file, line({ type: "assistant", uuid: "a3", parentUuid: "r2", timestamp: at(20), message: { model: "claude-opus-5-5", content: [{ type: "text", text: "Built." }] } }));
  const grown = poll();
  assert.notEqual(grown.etag, stopped.etag);
  assert.equal(grown.body.messages.at(-1).text, "Built.");
  assert.equal(poll().etag, grown.etag);
});
