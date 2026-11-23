// A question Claude asked the user (AskUserQuestion) reads in the chat as the
// question and the answer given (cloud/app/app.js paintAsked), not a tool card
// of raw JSON: cloud/worker/transcripts.mjs gives the tool call `ask`, with
// the answers Claude Code records in the result's toolUseResult, keyed by the
// question (a label, several for a multiple choice, or written text).
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { readTranscript } from "../cloud/worker/transcripts.mjs";

const jsonl = (entries) => entries.map((entry) => `${JSON.stringify(entry)}\n`).join("");
const at = (seconds) => new Date(Date.UTC(2026, 10, 23, 9, 0, seconds)).toISOString();

test("an AskUserQuestion call carries its questions, then the answers given", (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-questions-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, "home");
  const repo = path.join(root, "repo");
  const session = "b1c2d3e4-0000-4000-8000-000000000001";
  fs.mkdirSync(path.join(repo, ".rudder", "runs", "run_q"), { recursive: true });
  fs.writeFileSync(path.join(repo, ".rudder", "runs", "run_q", "run.json"), JSON.stringify({ id: "run_q", backend: "claude", status: "running", session: { nativeSessionId: session } }));
  const project = path.join(home, ".claude", "projects", "-x");
  fs.mkdirSync(project, { recursive: true });
  const questions = [
    { question: "Which color?", header: "Color", multiSelect: false, options: [{ label: "Red", description: "" }, { label: "Blue", description: "" }] },
    { question: "Which sizes?", header: "Sizes", multiSelect: true, options: [{ label: "S", description: "" }, { label: "M", description: "" }, { label: "L", description: "" }] },
  ];
  const ask = (id, uuid, parentUuid, ts) => ({ type: "assistant", uuid, parentUuid, timestamp: at(ts), message: { content: [{ type: "tool_use", id, name: "AskUserQuestion", input: { questions } }] } });
  fs.writeFileSync(path.join(project, `${session}.jsonl`), jsonl([
    { type: "user", uuid: "u1", parentUuid: null, timestamp: at(0), message: { role: "user", content: "make a shirt" } },
    ask("toolu_1", "a1", "u1", 1),
    {
      type: "user", uuid: "r1", parentUuid: "a1", timestamp: at(2),
      message: { role: "user", content: [{ type: "tool_result", tool_use_id: "toolu_1", content: "Your questions have been answered: ..." }] },
      toolUseResult: { questions, answers: { "Which color?": "Blue", "Which sizes?": ["S", "L"] } },
    },
    ask("toolu_2", "a2", "r1", 3),
  ]));
  const tools = readTranscript(repo, "run_q", { home }).messages.filter((m) => m.role === "tool").map((m) => m.tool);
  const asked = [{ question: "Which color?", options: ["Red", "Blue"] }, { question: "Which sizes?", options: ["S", "M", "L"] }];
  assert.equal(tools.length, 2);
  assert.equal(tools[0].input, "Which color? Which sizes?");
  assert.deepEqual(tools[0].ask, { questions: asked, answers: { "Which color?": "Blue", "Which sizes?": "S, L" } });
  // Still open: no answers yet.
  assert.deepEqual(tools[1].ask, { questions: asked, answers: null });
});
