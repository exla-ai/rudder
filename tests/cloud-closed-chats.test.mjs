import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { readClosedChats } from "../cloud/worker/closed-chats.mjs";

// The app's Show closed lists what the dashboard moved to .rudder/closed
// (native agent_life.rs close_agent_now), newest first.
test("closed chats are listed newest first, and one still closing waits for its stamp", (t) => {
  const repo = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-closed-chats-"));
  t.after(() => fs.rmSync(repo, { recursive: true, force: true }));
  assert.deepEqual(readClosedChats(repo), [], "no closed dir yet");
  const close = (id, record, closedAt) => {
    const dir = path.join(repo, ".rudder", "closed", id);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, "run.json"), JSON.stringify({ id, ...record }));
    if (closedAt !== undefined) fs.writeFileSync(path.join(dir, "closed.json"), JSON.stringify({ closedAt }));
  };
  close("__main__", { taskSummary: "general", task: "chat", backend: "claude", model: "opus", effort: "high" }, 1_795_000_000);
  close("w1", { task: "fix the flaky test", backend: "codex", model: "gpt-6.1-sol" }, 1_795_003_600);
  close("w2", { task: "still closing" });
  close("w3", { id: "someone-else" }, 1_795_000_000);
  assert.deepEqual(readClosedChats(repo), [
    { id: "w1", title: "fix the flaky test", backend: "codex", model: "gpt-6.1-sol", effort: null, closedAt: 1_795_003_600_000 },
    { id: "__main__", title: "general", backend: "claude", model: "opus", effort: "high", closedAt: 1_795_000_000_000 },
  ]);
});
