// The files an agent's tool calls read, change or find (cloud/worker/tool-files.mjs),
// which the app's tool cards open beside the chat (cloud/app/tool-files.js).
// Fixtures are synthetic, shaped like Claude Code 2.1.328, Codex 0.169 and
// OpenCode 1.19 logs.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { readTranscript } from "../cloud/worker/transcripts.mjs";

const jsonl = (entries) => entries.map((entry) => `${JSON.stringify(entry)}\n`).join("");
const at = (seconds) => new Date(Date.UTC(2026, 10, 19, 6, 0, seconds)).toISOString();

function setup(t, record) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-tool-files-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, "home");
  const repo = path.join(root, "repo");
  fs.mkdirSync(path.join(repo, ".rudder", "runs", record.id), { recursive: true });
  fs.writeFileSync(path.join(repo, ".rudder", "runs", record.id, "run.json"), JSON.stringify(record));
  return { home, repo };
}

const toolsOf = (page) => Object.fromEntries(page.messages.filter((m) => m.role === "tool").map((m) => [m.id, { files: m.tool.files, found: m.tool.found }]));

test("Claude Code: Read, Edit, Write and NotebookEdit name their file, an edit its changed lines, Grep and Glob the paths they list", (t) => {
  const session = "a8caccf0-0000-4000-8000-0000000000f1";
  const { home, repo } = setup(t, { id: "run_c", backend: "claude", status: "done", session: { nativeSessionId: session } });
  const project = path.join(home, ".claude", "projects", "-x");
  fs.mkdirSync(project, { recursive: true });
  let n = 0;
  const call = (id, name, input) => ({ type: "assistant", uuid: `a${++n}`, timestamp: at(n), message: { role: "assistant", content: [{ type: "tool_use", id, name, input }] } });
  const result = (id, content, toolUseResult, isError = false) => ({ type: "user", uuid: `u${++n}`, timestamp: at(n), toolUseResult, message: { role: "user", content: [{ type: "tool_result", tool_use_id: id, content, ...(isError ? { is_error: true } : {}) }] } });
  // Rows of 41 characters: the cut at 12,000 falls inside one.
  const glob = Array.from({ length: 400 }, (_, i) => `/repo/src/generated/module-${String(i).padStart(4, "0")}/index.ts`);
  fs.writeFileSync(path.join(project, `${session}.jsonl`), jsonl([
    { type: "user", uuid: "u0", timestamp: at(0), message: { role: "user", content: "fix the parser" } },
    call("read", "Read", { file_path: "/repo/src/parse.ts", offset: 40, limit: 20 }),
    result("read", "40\tconst x = 1;", { type: "text", file: { filePath: "/repo/src/parse.ts", content: "const x = 1;", numLines: 1, startLine: 40, totalLines: 90 } }),
    call("edit", "Edit", { file_path: "src/parse.ts", old_string: "d", new_string: "e\nf" }),
    result("edit", "The file src/parse.ts has been updated successfully.", { filePath: "src/parse.ts", structuredPatch: [
      { oldStart: 10, oldLines: 7, newStart: 10, newLines: 8, lines: [" a", " b", " c", "-d", "+e", "+f", " g", " h"] },
      { oldStart: 70, oldLines: 1, newStart: 71, newLines: 1, lines: ["-y", "+z"] },
    ] }),
    call("cut", "Edit", { file_path: "src/parse.ts", old_string: "old\n", new_string: "" }),
    result("cut", "The file src/parse.ts has been updated successfully.", { filePath: "src/parse.ts", structuredPatch: [{ oldStart: 20, oldLines: 4, newStart: 20, newLines: 3, lines: [" p", "-old", " q", " r"] }] }),
    call("write", "Write", { file_path: "/repo/src/new.ts", content: "export {};\n" }),
    result("write", "File created successfully at: /repo/src/new.ts", { type: "create", filePath: "/repo/src/new.ts", structuredPatch: [] }),
    call("pending", "Write", { file_path: "/repo/src/later.ts", content: "x" }),
    call("nb", "NotebookEdit", { notebook_path: "/repo/nb.ipynb", cell_id: "c1", new_source: "1" }),
    result("nb", "Updated cell c1", {}),
    call("missing", "Read", { file_path: "/repo/gone.ts" }),
    result("missing", "File does not exist.", "Error: File does not exist.", true),
    call("files", "Grep", { pattern: "parse" }),
    result("files", "Found 2 files\nsrc/parse.ts\nsrc/lex.ts", { mode: "files_with_matches", numFiles: 2, filenames: [] }),
    call("content", "Grep", { pattern: "parse", path: "src", output_mode: "content", "-C": 1 }),
    result("content", "src/parse.ts:12:export function parse() {\nsrc/parse.ts-13-  return 1;\n--\nsrc/lex.ts:4:import { parse } from \"./parse\";\n\n[Showing results with pagination = limit: 250]", { mode: "content", numFiles: 0, filenames: [] }),
    call("one", "Grep", { pattern: "parse", path: "src/parse.ts", output_mode: "content" }),
    result("one", "12:export function parse() {\n30:  parse(rest);", { mode: "content", numFiles: 0, filenames: [] }),
    call("count", "Grep", { pattern: "parse", output_mode: "count" }),
    result("count", "src/parse.ts:3\nsrc/lex.ts:1\n\nFound 4 total occurrences across 2 files.", { mode: "count", numFiles: 2, filenames: [] }),
    call("glob", "Glob", { pattern: "src/**/*.ts" }),
    result("glob", ["The search stopped before it finished, most likely at its time limit, so these results are incomplete: more matches may exist. Search a more specific path or pattern.", "/repo/src/parse.ts", "/repo/src/lex.ts", "(Results are truncated. Consider using a more specific path or pattern.)"].join("\n"), { filenames: [], numFiles: 2, truncated: true }),
    call("big", "Glob", { pattern: "src/generated/**" }),
    result("big", glob.join("\n"), { filenames: [], numFiles: 300, truncated: false }),
  ]));
  const page = readTranscript(repo, "run_c", { home });
  const tools = toolsOf(page);
  const file = (p, line = 0, end = 0) => ({ path: p, line, end });
  assert.deepEqual(tools.read, { files: [file("/repo/src/parse.ts", 40)], found: undefined }, "a Read opens at its range's first line");
  assert.deepEqual(tools.edit.files, [file("src/parse.ts", 13, 14)], "an Edit opens at the lines its first change touched");
  assert.deepEqual(tools.cut.files, [file("src/parse.ts", 21)], "lines only removed open where they were");
  assert.deepEqual(tools.write.files, [file("/repo/src/new.ts")]);
  assert.equal(tools.pending.files, undefined, "a Write's file is not there until its result is in");
  assert.deepEqual(tools.nb.files, [file("/repo/nb.ipynb")]);
  assert.deepEqual(tools.missing, { files: undefined, found: undefined }, "a failed call opens nothing");
  assert.deepEqual(tools.files.found, [
    { row: 1, text: "src/parse.ts", path: "src/parse.ts", line: 0 },
    { row: 2, text: "src/lex.ts", path: "src/lex.ts", line: 0 },
  ]);
  assert.deepEqual(tools.content.found, [
    { row: 0, text: "src/parse.ts:12", path: "src/parse.ts", line: 12 },
    { row: 3, text: "src/lex.ts:4", path: "src/lex.ts", line: 4 },
  ], "matches open at their line; context rows and the pagination note are not links");
  assert.deepEqual(tools.one.found, [
    { row: 0, text: "12", path: "src/parse.ts", line: 12 },
    { row: 1, text: "30", path: "src/parse.ts", line: 30 },
  ], "a search of one file lists its lines without the path");
  assert.deepEqual(tools.count.found.map((f) => f.path), ["src/parse.ts", "src/lex.ts"]);
  assert.deepEqual(tools.glob.found.map((f) => [f.row, f.path]), [[1, "/repo/src/parse.ts"], [2, "/repo/src/lex.ts"]], "the notes before and after are not paths");

  // Output past what the chat carries is cut: only the rows it shows whole are links.
  const big = page.messages.find((m) => m.id === "big").tool;
  const shown = big.output.split("\n");
  assert.ok(big.found.length > 100 && big.found.length < glob.length, `${big.found.length} rows`);
  for (const f of big.found) assert.equal(shown[f.row], f.path);
  assert.notEqual(shown[big.found.length], glob[big.found.length], "the row the cut falls in is not one");
});

test("Codex: a patch's FileChange items name the files it changed on the call that applied it", (t) => {
  const hex = Date.parse(at(0)).toString(16).padStart(12, "0");
  const thread = `${hex.slice(0, 8)}-${hex.slice(8)}-7000-8000-0000000000f2`;
  const { home, repo } = setup(t, { id: "run_x", backend: "codex", status: "done", session: { nativeSessionId: thread } });
  const day = path.join(home, ".codex", "sessions", "2026", "11", "19");
  fs.mkdirSync(day, { recursive: true });
  let n = 0;
  const entry = (type, payload) => ({ timestamp: at(++n), ordinal: n, type, payload });
  const exec = (id, input) => entry("response_item", { type: "custom_tool_call", call_id: id, name: "exec", input });
  const done = (id) => entry("response_item", { type: "custom_tool_call_output", call_id: id, output: [{ type: "input_text", text: "Script completed\nWall time 0.0 seconds\nOutput:\n" }, { type: "input_text", text: "{}" }] });
  const change = (changes, status = "completed") => entry("event_msg", { type: "item_completed", thread_id: thread, item: { type: "FileChange", id: `exec-${n}`, status, changes } });
  fs.writeFileSync(path.join(day, `rollout-2026-11-19T06-00-00-${thread}.jsonl`), jsonl([
    entry("session_meta", { id: thread, cwd: "/ws", cli_version: "0.169.0" }),
    entry("response_item", { type: "message", role: "user", content: [{ type: "input_text", text: "fix the parser" }] }),
    // A subagent still starting is not the call that patches.
    entry("response_item", { type: "function_call", call_id: "spawn", name: "spawn_agent", arguments: JSON.stringify({ task_name: "docs" }) }),
    exec("patch", "text(await tools.apply_patch(`*** Begin Patch\n*** Update File: a.ts\n@@\n+x\n*** End Patch`))"),
    change({
      "/ws/a.ts": { type: "update", unified_diff: "@@ -12,2 +12,4 @@\n \n+x\n+y\n z\n", move_path: null },
      "/ws/b.ts": { type: "add", content: "export {};\n" },
      "/ws/c.ts": { type: "delete", content: "gone\n" },
      "/ws/old.ts": { type: "update", unified_diff: "@@ -1,3 +1,3 @@\n-a\n+b\n c\n", move_path: "/ws/new.ts" },
    }),
    // A second patch in the same script: a file it already named keeps its first change.
    change({ "/ws/a.ts": { type: "update", unified_diff: "@@ -40,1 +42,1 @@\n-p\n+q\n", move_path: null } }),
    done("patch"),
    exec("refused", "text(await tools.apply_patch(`...`))"),
    change({ "/ws/d.ts": { type: "update", unified_diff: "@@ -1 +1 @@\n-a\n+b\n" } }, "declined"),
    done("refused"),
    // No call is open: nothing to put it on.
    change({ "/ws/e.ts": { type: "add", content: "" } }),
  ]));
  const tools = toolsOf(readTranscript(repo, "run_x", { home }));
  assert.deepEqual(tools.patch.files, [
    { path: "/ws/a.ts", line: 13, end: 14 },
    { path: "/ws/b.ts", line: 0, end: 0 },
    { path: "/ws/new.ts", line: 1, end: 0 },
  ], "an update opens at its first change, an added file at the top, a moved one where it went; a deleted one is gone");
  assert.equal(tools.spawn.files, undefined);
  assert.equal(tools.refused.files, undefined, "a patch that did not apply changed nothing");
});

test("OpenCode: read, edit, write and apply_patch name their files, grep and glob the paths they list", (t) => {
  const SESSION = "ses_0a1b2c3d4e5fToolFilesTest";
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-tool-files-oc-"));
  const saved = { data: process.env.XDG_DATA_HOME, cache: process.env.XDG_CACHE_HOME };
  delete process.env.XDG_DATA_HOME;
  delete process.env.XDG_CACHE_HOME;
  t.after(() => {
    fs.rmSync(root, { recursive: true, force: true });
    for (const [key, value] of [["XDG_DATA_HOME", saved.data], ["XDG_CACHE_HOME", saved.cache]]) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  });
  const home = path.join(root, "home");
  const repo = path.join(root, "repo");
  fs.mkdirSync(path.join(repo, ".rudder", "runs", "run_o"), { recursive: true });
  fs.writeFileSync(path.join(repo, ".rudder", "runs", "run_o", "run.json"), JSON.stringify({ id: "run_o", backend: "opencode", status: "done", task: "fix the parser", session: { nativeSessionId: SESSION } }));
  const dir = path.join(home, ".local", "share", "opencode");
  fs.mkdirSync(dir, { recursive: true });
  const db = new DatabaseSync(path.join(dir, "opencode.db"));
  db.exec(`
    CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, title TEXT, time_updated INTEGER);
    CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
    CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
  `);
  db.prepare("INSERT INTO session VALUES (?, NULL, 'Fix the parser', 9000)").run(SESSION);
  const part = (tool, state) => ({ type: "tool", tool, callID: `call_${tool}_${state.input?.filePath?.split("/").pop() ?? state.status}`, state: { time: { start: 1500, end: 1600 }, ...state } });
  const pieces = [
    part("read", { status: "completed", input: { filePath: "/repo/src/parse.ts", offset: 590, limit: 80 }, output: "<path>/repo/src/parse.ts</path>\n<type>file</type>\n<content>\n590: x\n</content>", metadata: { preview: "x", truncated: false, display: { type: "file", path: "/repo/src/parse.ts", lineStart: 590 } } }),
    part("read", { status: "completed", input: { filePath: "/repo/src" }, output: "<path>/repo/src</path>\n<type>directory</type>", metadata: { display: { type: "directory", path: "/repo/src", entries: ["parse.ts"] } } }),
    part("edit", { status: "completed", input: { filePath: "/repo/src/lex.ts", oldString: "a", newString: "b" }, output: "Edit applied successfully.", metadata: { diff: "Index: /repo/src/lex.ts\n===================================================================\n--- /repo/src/lex.ts\n+++ /repo/src/lex.ts\n@@ -6,8 +6,9 @@\n one\n two\n three\n \n+four\n five\n six\n seven\n eight\n", filediff: { file: "/repo/src/lex.ts", additions: 1, deletions: 0 } } }),
    part("edit", { status: "error", input: { filePath: "/repo/src/bad.ts", oldString: "a", newString: "b" }, error: "Could not find oldString in the file." }),
    part("write", { status: "completed", input: { filePath: "/repo/PLAN.md", content: "# Plan" }, output: "Wrote file successfully.", metadata: { filepath: "/repo/PLAN.md", exists: false } }),
    part("apply_patch", { status: "completed", input: { patchText: "*** Begin Patch\n*** End Patch" }, output: "Success. Updated the following files:\nM src/a.rs\nA src/b.rs\nD src/c.rs\nM src/e.rs", metadata: { files: [
      { filePath: "/repo/src/a.rs", relativePath: "src/a.rs", type: "update", patch: "@@ -3,3 +3,3 @@\n x\n-y\n+z\n", additions: 1, deletions: 1 },
      { filePath: "/repo/src/b.rs", relativePath: "src/b.rs", type: "add", patch: "@@ -0,0 +1,1 @@\n+fn b() {}\n", additions: 1, deletions: 0 },
      { filePath: "/repo/src/c.rs", relativePath: "src/c.rs", type: "delete", patch: "", additions: 0, deletions: 4 },
      { filePath: "/repo/src/d.rs", relativePath: "src/e.rs", type: "move", movePath: "/repo/src/e.rs", patch: "@@ -1,2 +1,2 @@\n-old\n+new\n keep\n", additions: 1, deletions: 1 },
    ] } }),
    part("grep", { status: "completed", input: { pattern: "parse", path: "/repo/src" }, output: "Found 3 matches\n/repo/src/parse.ts:\n  Line 6: export function parse() {\n\n  Line 26:   return parse(rest);\n\n\n/repo/src/lex.ts:\n  Line 3: import { parse } from \"./parse\";\n\n\n(Results truncated. Consider using a more specific path or pattern.)", metadata: { matches: 3, truncated: true } }),
    part("glob", { status: "completed", input: { pattern: "**/*.ts" }, output: "/repo/src/parse.ts\n/repo/src/lex.ts\n\n(Results are truncated: showing first 100 results. Consider using a more specific path or pattern.)", metadata: { count: 2, truncated: true } }),
  ];
  db.prepare("INSERT INTO message VALUES (?, ?, ?, ?)").run("msg_01", SESSION, 1000, JSON.stringify({ role: "user", time: { created: 1000 } }));
  db.prepare("INSERT INTO part VALUES (?, ?, ?, ?, ?, ?)").run("prt_000", "msg_01", SESSION, 1000, 1000, JSON.stringify({ type: "text", text: "fix the parser" }));
  db.prepare("INSERT INTO message VALUES (?, ?, ?, ?)").run("msg_02", SESSION, 1100, JSON.stringify({ role: "assistant", time: { created: 1100, completed: 3000 }, finish: "stop" }));
  pieces.forEach((piece, i) => db.prepare("INSERT INTO part VALUES (?, ?, ?, ?, ?, ?)").run(`prt_${String(i + 1).padStart(3, "0")}`, "msg_02", SESSION, 1100, 1100, JSON.stringify(piece)));
  db.close();

  const tools = toolsOf(readTranscript(repo, "run_o", { home, limit: 300 }));
  const file = (p, line = 0, end = 0) => ({ path: p, line, end });
  assert.deepEqual(tools["call_read_parse.ts"].files, [file("/repo/src/parse.ts", 590)]);
  assert.equal(tools.call_read_src.files, undefined, "a folder read has no file to open");
  assert.deepEqual(tools["call_edit_lex.ts"].files, [file("/repo/src/lex.ts", 10)]);
  assert.equal(tools["call_edit_bad.ts"].files, undefined, "a failed edit opens nothing");
  assert.deepEqual(tools["call_write_PLAN.md"].files, [file("/repo/PLAN.md")]);
  assert.deepEqual(tools.call_apply_patch_completed.files, [file("/repo/src/a.rs", 4), file("/repo/src/b.rs"), file("/repo/src/e.rs", 1)]);
  assert.deepEqual(tools.call_grep_completed.found, [
    { row: 1, text: "/repo/src/parse.ts", path: "/repo/src/parse.ts", line: 6 },
    { row: 2, text: "Line 6", path: "/repo/src/parse.ts", line: 6 },
    { row: 4, text: "Line 26", path: "/repo/src/parse.ts", line: 26 },
    { row: 7, text: "/repo/src/lex.ts", path: "/repo/src/lex.ts", line: 3 },
    { row: 8, text: "Line 3", path: "/repo/src/lex.ts", line: 3 },
  ], "a file opens at its first match, each match at its own line");
  assert.deepEqual(tools.call_glob_completed.found.map((f) => [f.row, f.path]), [[0, "/repo/src/parse.ts"], [1, "/repo/src/lex.ts"]]);
});
