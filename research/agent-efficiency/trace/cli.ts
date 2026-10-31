// node research/agent-efficiency/trace/cli.ts <codex-session-id | rollout.jsonl>
//   [--sessions DIR] [--md FILE] [--json FILE] [--root-only]
import { writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { analyze } from "./analyze.ts";
import { findSessionTree, readCodexRollout } from "./codex.ts";
import { renderMarkdown, renderTools } from "./report.ts";
import { analyzeTools } from "./tools.ts";

const args = process.argv.slice(2);
const flag = (name: string) => {
  const at = args.indexOf(name);
  return at === -1 ? undefined : args.splice(at, 2)[1];
};
const sessions = flag("--sessions") ?? join(homedir(), ".codex", "sessions");
const md = flag("--md");
const json = flag("--json");
const rootOnly = args.includes("--root-only");
const target = args.find((arg) => !arg.startsWith("--"));
if (!target) {
  console.error("usage: cli.ts <codex-session-id | rollout.jsonl> [--sessions DIR] [--md FILE] [--json FILE] [--root-only]");
  process.exit(2);
}

let files = findSessionTree(sessions, target);
if (!files.length && target.endsWith(".jsonl")) files = [target];
if (!files.length) {
  console.error(`no Codex rollouts found for ${target} under ${sessions}`);
  process.exit(1);
}
const traces = [];
for (const file of files) traces.push(await readCodexRollout(file));
const roots = traces.filter((trace) => !trace.parentId);
const selected = rootOnly ? roots : traces;
selected.sort((a, b) => Number(Boolean(a.parentId)) - Number(Boolean(b.parentId)) || a.startedAt - b.startedAt);
const report = analyze(selected);
const tools = analyzeTools(selected);
const markdown = renderMarkdown(report, roots[0]?.id || target) + "\n" + renderTools(tools);
if (json) writeFileSync(json, JSON.stringify({ ...report, tools }, null, 2) + "\n");
if (md) writeFileSync(md, markdown);
if (!md) process.stdout.write(markdown);
else console.error(`wrote ${md}${json ? ` and ${json}` : ""} from ${files.length} rollout(s)`);
