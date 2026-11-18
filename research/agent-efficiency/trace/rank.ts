// Ranks waste across a survey's session rows (survey.ts) and prints Markdown.
// Rows hold private numbers about real work: keep the output outside the repo.
//
// node research/agent-efficiency/trace/rank.ts DIR/sessions.jsonl [--after ISO]
//
// --after keeps only sessions that started at or after that instant, to check
// a change against traces from after it shipped.
import { readFileSync } from "node:fs";
import type { Row } from "./survey.ts";

// API list prices in $ per million tokens: input, cache read, cache write (one
// hour, the TTL Claude Code uses), output. An estimate of spend, not a bill:
// subscription use is metered against limits, not dollars. Edit when prices move.
const PRICES: [RegExp, [number, number, number, number]][] = [
  [/opus-5-5/, [4, 0.2, 8, 20]],
  [/fable-5-2/, [8, 0.2, 16, 40]],
  [/haiku-5-5/, [0.1, 0.01, 0.2, 0.5]],
  [/sonnet-5/, [2, 0.1, 4, 10]],
  [/opus/, [5, 0.5, 10, 25]],
  [/fable/, [10, 1, 20, 50]],
  [/haiku/, [1, 0.1, 2, 5]],
];

function price(model: string) {
  return PRICES.find(([pattern]) => pattern.test(model))?.[1];
}

const args = process.argv.slice(2);
const file = args.find((arg) => !arg.startsWith("--"));
const afterAt = args.indexOf("--after");
const after = afterAt === -1 ? "" : args[afterAt + 1];
if (!file) {
  console.error("usage: rank.ts sessions.jsonl [--after ISO]");
  process.exit(2);
}
const rows: Row[] = readFileSync(file, "utf8").split("\n").filter(Boolean).map((line) => JSON.parse(line)).filter((row: Row) => row.start >= after);

const M = (value: number) => (value / 1e6).toFixed(1);
const usd = (value: number) => `$${Math.round(value)}`;
const out: string[] = [];
const starts = rows.map((row) => row.start).sort();
out.push(`# Agent waste ranking`, "", `${rows.length} sessions, ${starts[0]?.slice(0, 10)} to ${starts.at(-1)?.slice(0, 10)}.`, "");

// Spend by backend and role.
const groups = new Map<string, { sessions: number; requests: number; input: number; cached: number; write: number; output: number; dollars: number; active: number; model: number; tool: number; compactions: number; compactSeconds: number }>();
let claudeDollars = 0;
for (const row of rows) {
  const key = `${row.backend}/${row.role}`;
  const group = groups.get(key) ?? { sessions: 0, requests: 0, input: 0, cached: 0, write: 0, output: 0, dollars: 0, active: 0, model: 0, tool: 0, compactions: 0, compactSeconds: 0 };
  const t = row.tokens;
  const p = row.backend === "claude" ? price(row.model) : undefined;
  const dollars = p ? ((t.input - t.cached - t.cacheWrite) * p[0] + t.cached * p[1] + t.cacheWrite * p[2] + t.output * p[3]) / 1e6 : 0;
  claudeDollars += dollars;
  Object.assign(group, {
    sessions: group.sessions + 1, requests: group.requests + row.requests, input: group.input + t.input, cached: group.cached + t.cached,
    write: group.write + t.cacheWrite, output: group.output + t.output, dollars: group.dollars + dollars, active: group.active + row.hours.active,
    model: group.model + row.hours.model, tool: group.tool + row.hours.tool, compactions: group.compactions + row.compactions, compactSeconds: group.compactSeconds + (row.compactSeconds ?? 0),
  });
  groups.set(key, group);
}
out.push("## Spend by backend and role", "", "| Backend/role | Sessions | Requests | Input M | Cached M | Cache write M | Output M | Claude $ | Active h | Model h | Tool h | Compactions | Compacting h |", "|---|---|---|---|---|---|---|---|---|---|---|---|---|");
for (const [key, g] of [...groups].sort((a, b) => b[1].input - a[1].input)) {
  out.push(`| ${key} | ${g.sessions} | ${g.requests} | ${M(g.input)} | ${M(g.cached)} | ${M(g.write)} | ${M(g.output)} | ${g.dollars ? usd(g.dollars) : "-"} | ${g.active.toFixed(0)} | ${g.model.toFixed(0)} | ${g.tool.toFixed(0)} | ${g.compactions} | ${(g.compactSeconds / 3600).toFixed(1)} |`);
}

// Patterns. Claude in dollars at cache-read or cache-write price; Codex in
// carried tokens, since its usage is not priced here.
const patterns = new Map<string, { claude: number; codex: number; sessions: number }>();
const note = (name: string, row: Row, claude: number, codex: number) => {
  if (!claude && !codex) return;
  const entry = patterns.get(name) ?? { claude: 0, codex: 0, sessions: 0 };
  entry.claude += claude;
  entry.codex += codex;
  entry.sessions++;
  patterns.set(name, entry);
};
for (const row of rows) {
  const p = row.backend === "claude" ? price(row.model) : undefined;
  const read = (tokens: number) => (p ? (tokens * p[1]) / 1e6 : 0);
  const write = (tokens: number) => (p ? (tokens * p[2]) / 1e6 : 0);
  const codex = (tokens: number) => (row.backend === "codex" ? tokens : 0);
  const i = row.instructions;
  note("Context above 150K tokens per request", row, read(row.tokens.above150k), codex(row.tokens.above150k));
  note("Cache rewrite after compaction", row, write(row.writes?.afterCompaction ?? 0), 0);
  note("Cache rewrite after an idle hour", row, write(row.writes?.idle ?? 0), 0);
  note("Instruction files carried", row, read(i.carried), codex(i.carried));
  note("Parent checkout's instruction copy carried", row, read(i.outerCarried), codex(i.outerCarried));
  note("Tool outputs carried", row, read(row.outputs.carried), codex(row.outputs.carried));
  note("Tool outputs over 8K tokens carried", row, read(row.outputs.bigCarried), codex(row.outputs.bigCarried));
  note("Exact re-reads carried", row, read(row.rereads.carried), codex(row.rereads.carried));
  if (row.role === "probe") {
    const t = row.tokens;
    note("Credential probe calls", row, p ? ((t.input - t.cached - t.cacheWrite) * p[0] + t.cached * p[1] + t.cacheWrite * p[2] + t.output * p[3]) / 1e6 : 0, codex(t.input));
  }
}
out.push("", "## Waste patterns", "", `Claude total ${usd(claudeDollars)} at API list prices. Patterns overlap: the parent checkout's copy is part of the instructions, and both are part of the context.`, "", "| Pattern | Claude $ | Share of Claude $ | Codex tokens M | Sessions |", "|---|---|---|---|---|");
for (const [name, entry] of [...patterns].sort((a, b) => b[1].claude - a[1].claude)) {
  out.push(`| ${name} | ${usd(entry.claude)} | ${((100 * entry.claude) / Math.max(1, claudeDollars)).toFixed(1)}% | ${M(entry.codex)} | ${entry.sessions} |`);
}

// Time sinks that do not show up as tokens.
const sum = (pick: (row: Row) => number, backend: string) => rows.filter((row) => row.backend === backend).reduce((total, row) => total + pick(row), 0);
out.push("", "## Time and repetition", "", "| Measure | Claude | Codex |", "|---|---|---|");
const line = (name: string, pick: (row: Row) => number, digits = 0) => out.push(`| ${name} | ${sum(pick, "claude").toFixed(digits)} | ${sum(pick, "codex").toFixed(digits)} |`);
line("Compacting, hours", (row) => (row.compactSeconds ?? 0) / 3600, 1);
line("sleep calls", (row) => row.sleeps.calls);
line("Hours asked to sleep", (row) => row.sleeps.seconds / 3600, 1);
line("Same command twice in a row", (row) => row.repeats.calls);
line("Failed command failing again", (row) => row.failedRepeats);
line("Full test suite runs", (row) => row.fullSuites.calls);
line("Tool outputs over 8K tokens", (row) => row.outputs.big);

// Checks for the shipped changes.
const workers = rows.filter((row) => row.backend === "claude" && (row.role === "worker" || row.role === "subagent") && row.instructions.loads);
const codexRows = rows.filter((row) => row.backend === "codex" && row.role !== "probe");
out.push("", "## Checks", "",
  `- Claude workers and subagents loading the parent checkout's instructions: ${workers.filter((row) => row.instructions.outerBytesPerLoad > 0).length} of ${workers.length} (want 0).`,
  `- Codex requests' tokens above 150K: ${M(codexRows.reduce((total, row) => total + row.tokens.above150k, 0))}M (want 0 with the 150K compaction limit).`,
  `- Codex effort by session: ${JSON.stringify(Object.fromEntries(Object.entries(codexRows.reduce((counts: Record<string, number>, row) => ({ ...counts, [row.effort || "unset"]: (counts[row.effort || "unset"] ?? 0) + 1 }), {}))))}.`,
);
console.log(out.join("\n"));
