import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { analyze } from "./analyze.ts";
import { classifySegment, redact, splitShell } from "./classify.ts";
import { findSessionTree, readCodexRollout } from "./codex.ts";

const ROOT = "01a00000-0000-7000-8000-000000000001";
const CHILD = "01a00000-0000-7000-8000-000000000002";

function rollout(dir: string, name: string, meta: object, records: object[]): string {
  const path = join(dir, name);
  const lines = [{ timestamp: "2026-10-30T07:00:00.000Z", type: "session_meta", payload: meta }, ...records];
  writeFileSync(path, lines.map((line) => JSON.stringify(line)).join("\n") + "\n");
  return path;
}

let clock = Date.parse("2026-10-30T07:00:00.000Z");
const at = () => new Date((clock += 10_000)).toISOString();

function request(callId: string, script: string, outputs: object[], input: number): object[] {
  return [
    { timestamp: at(), type: "response_item", payload: { type: "custom_tool_call", name: "exec", call_id: callId, input: script } },
    { timestamp: at(), type: "token_usage_record", payload: { turn_id: "t1", usage: { input_tokens: input, cached_input_tokens: input - 100, output_tokens: 50, reasoning_output_tokens: 20 } } },
    { timestamp: at(), type: "response_item", payload: { type: "custom_tool_call_output", call_id: callId, output: [{ type: "input_text", text: "Script completed\nWall time 1.0 seconds\nOutput:\n" }, ...outputs.map((output) => ({ type: "input_text", text: JSON.stringify(output) }))] } },
  ];
}

const result = (output: string, exit = 0) => ({ wall_time_seconds: 0.1, exit_code: exit, output });

test("classifies common agent shell commands", () => {
  const kinds = (script: string) => splitShell(script).map((part) => classifySegment(part.text, part.heredoc).category);
  assert.deepEqual(kinds("sed -n '1,40p' src/main.ts; tail -20 /tmp/build.log"), ["read", "wait"]);
  assert.deepEqual(kinds("cargo test -p rudder-engine && git diff --check"), ["test", "test"]);
  assert.deepEqual(kinds("npm version prerelease --preid alpha && git push origin main v4.0.0-alpha.15"), ["release", "release"]);
  assert.deepEqual(kinds("python3 - <<'PY'\nfrom pathlib import Path\nPath('a.md').write_text('x')\nPY"), ["edit"]);
  assert.deepEqual(kinds("python3 - <<'PY'\nimport json\nprint(json.load(open('a.json')))\nPY"), ["read"]);
  assert.deepEqual(kinds("NO_COLOR=1 timeout 60 flyctl deploy --app x"), ["cloud"]);
});

test("redacts credentials from quoted commands", () => {
  const text = redact("FLY_API_TOKEN=FlyV1 fm2_abc curl -H 'Authorization: Bearer abc.def' https://x && echo ghp_0123456789abcdefABCDEF && git show 13889d1e0860b5a806f5513d32cf0f215747759f");
  assert.doesNotMatch(text, /fm2_abc|abc\.def|ghp_0123/);
  assert.match(text, /13889d1e0860b5a806f5513d32cf0f215747759f/);
});

test("attributes tokens, waits, re-reads and docs-only edits", async () => {
  const dir = mkdtempSync(join(tmpdir(), "trace-test-"));
  const root = rollout(dir, "rollout-root.jsonl", { id: ROOT, session_id: ROOT, cwd: "/repo" }, [
    { timestamp: at(), type: "turn_context", payload: { model: "gpt-test", turn_id: "t1" } },
    ...request("c1", `text(await tools.exec_command({cmd:"sed -n '1,40p' src/a.ts"}));`, [result("x".repeat(12_000))], 30_000),
    ...request("c2", `text(await tools.exec_command({cmd:"tail -5 /tmp/run.log"}));\ntext(await tools.clock__curr_time({}));`, [result("ok")], 31_000),
    ...request("c3", `text(await tools.exec_command({cmd:"sed -n '1,40p' src/a.ts"}));`, [result("x".repeat(12_000))], 200_000),
    { timestamp: at(), type: "compacted", payload: { message: "" } },
    ...request("c4", `text(await tools.apply_patch("*** Begin Patch\\n*** Update File: /repo/docs/notes.md\\n+one\\n+two\\n*** End Patch"));`, [], 32_000),
    ...request("c5", `text(await tools.exec_command({cmd:"cargo test -p x"}));`, [result("fail", 101)], 33_000),
  ]);
  rollout(dir, "rollout-child.jsonl", { id: CHILD, session_id: ROOT, parent_thread_id: ROOT, agent_path: "/root/helper", cwd: "/repo" }, [
    ...request("k1", `text(await tools.exec_command({cmd:"ls"}));`, [result("a")], 20_000),
  ]);

  assert.deepEqual(findSessionTree(dir, root).length, 2);
  const traces = await Promise.all(findSessionTree(dir, ROOT).map((file) => readCodexRollout(file)));
  const report = analyze(traces);

  assert.equal(report.totals.requests, 6);
  assert.equal(report.totals.input, 346_000);
  assert.equal(report.waits.requests, 1);
  assert.equal(report.waits.clockCalls, 1);
  assert.equal(report.rereads.exact, 1);
  assert.equal(report.ceremony.requests, 1);
  assert.deepEqual(report.edits, [{ area: "docs", files: 1, added: 2, removed: 0 }]);
  assert.equal(report.failures.total, 1);
  assert.equal(report.coordination.subagentInput, 20_000);
  assert.equal(report.context.excess, 50_000);
  // The first 3K-token read is resent twice before the compaction.
  assert.equal(report.categories.find((entry) => entry.category === "read")?.carried, 3_000 * 2);
  assert.equal(report.agents.find((agent) => agent.agentPath === "/root")?.floorContext, 30_000);
});
