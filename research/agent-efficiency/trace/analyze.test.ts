import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { analyze } from "./analyze.ts";
import { readClaudeSession } from "./claude.ts";
import { classifySegment, redact, splitShell } from "./classify.ts";
import { findSessionTree, readCodexRollout } from "./codex.ts";
import { summarize } from "./survey.ts";

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

test("reads a Claude transcript once across a fork and measures the outer instructions", async () => {
  const dir = mkdtempSync(join(tmpdir(), "trace-claude-"));
  const cwd = "/repo/.rudder-workspaces/repo-1a/agent-1";
  const base = { sessionId: "s1", cwd, entrypoint: "cli" };
  const t = (seconds: number) => new Date(Date.parse("2026-11-10T00:00:00.000Z") + seconds * 1000).toISOString();
  const usage = (input: number) => ({ input_tokens: 1, cache_read_input_tokens: input - 1001, cache_creation_input_tokens: 1000, output_tokens: 40 });
  const records = [
    { ...base, type: "attachment", uuid: "a1", timestamp: t(0), attachment: { type: "instructions", files: [
      { path: "/repo/AGENTS.md", type: "Project", content: "x".repeat(4000) },
      { path: `${cwd}/AGENTS.md`, type: "Project", content: "x".repeat(4000) },
    ] }, rendered: [{ content: "x".repeat(8000) }] },
    { type: "api-request", sessionId: "s1", id: "r1", timestamp: t(1) },
    // One response, split into a thinking record and a tool-use record.
    { ...base, type: "assistant", uuid: "u1", timestamp: t(5), requestRef: "r1", effort: "high", message: { id: "m1", model: "claude-test", usage: usage(50_000), content: [{ type: "thinking", thinking: "" }] } },
    { ...base, type: "assistant", uuid: "u2", timestamp: t(6), requestRef: "r1", message: { id: "m1", model: "claude-test", usage: usage(50_000), content: [{ type: "tool_use", id: "c1", name: "Read", input: { file_path: `${cwd}/a.ts` } }] } },
    { ...base, type: "user", uuid: "u3", timestamp: t(8), message: { content: [{ type: "tool_result", tool_use_id: "c1", content: "y".repeat(400) }] } },
    { ...base, type: "system", uuid: "u4", subtype: "compact_boundary", timestamp: t(9), compactMetadata: { preTokens: 360_000 } },
    { type: "api-request", sessionId: "s1", id: "r2", timestamp: t(10) },
    { ...base, type: "assistant", uuid: "u5", timestamp: t(12), requestRef: "r2", message: { id: "m2", model: "claude-test", usage: usage(30_000), content: [{ type: "text", text: "done" }] } },
  ];
  const write = (path: string, lines: object[]) => writeFileSync(path, lines.map((line) => JSON.stringify(line)).join("\n") + "\n");
  mkdirSync(join(dir, "a")); mkdirSync(join(dir, "b"));
  write(join(dir, "a", "s1.jsonl"), records);
  // A fork copies the history into a new file, then continues.
  write(join(dir, "b", "s2.jsonl"), [...records.map((record) => ({ ...record, sessionId: "s2" })),
    { type: "api-request", sessionId: "s2", id: "r3", timestamp: t(20) },
    { ...base, sessionId: "s2", type: "assistant", uuid: "u6", timestamp: t(22), requestRef: "r3", message: { id: "m3", model: "claude-test", usage: usage(31_000), content: [] } },
  ]);

  const seen = new Set<string>();
  const first = await readClaudeSession(join(dir, "a", "s1.jsonl"), { seen });
  const fork = await readClaudeSession(join(dir, "b", "s2.jsonl"), { seen });
  assert.deepEqual(first.requests.map((request) => request.input), [50_000, 30_000]);
  assert.equal(first.requests[0].modelSeconds, 5);
  assert.equal(first.requests[0].calls[0].wallSeconds, 2);
  assert.equal(first.requests[0].calls[0].outputTokens, 100);
  assert.deepEqual(first.compactions, [1]);
  assert.deepEqual(fork.requests.map((request) => request.input), [31_000]);
  assert.equal(fork.contextLoads?.length, 0);

  const row = summarize(first);
  assert.equal(row.role, "worker");
  assert.equal(row.instructions.outerBytesPerLoad, 4000);
  // The outer copy rides on both requests after the load.
  assert.equal(row.instructions.outerCarried, 1000 * 2);
  assert.equal(row.tokens.cacheWrite, 2000);
  rmSync(dir, { recursive: true });
});
