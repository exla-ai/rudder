import { fmt } from "./analyze.ts";
import type { Report } from "./analyze.ts";
import type { ToolReport } from "./tools.ts";

const pct = (value: number) => (value * 100).toFixed(value < 0.1 ? 1 : 0) + "%";
const cell = (text: string) => text.replace(/\|/g, "\\|").replace(/\n/g, " ");

export function renderMarkdown(report: Report, title: string): string {
  const out: string[] = [];
  const t = report.totals;
  out.push(`# Agent trace report: ${title}`, "");
  out.push(`${report.agents.length} agent session(s), ${fmt(t.requests)} model requests, ${fmt(t.input)} input tokens (${pct(t.cached / Math.max(1, t.input))} cached), ${fmt(t.output)} output tokens of which ${fmt(t.reasoning)} reasoning.`);
  if (report.credits) out.push(`Weekly window ${report.credits.windowFirst}% to ${report.credits.windowLast}%; credits used since the window filled: ${fmt(report.credits.used)}.`);
  out.push("", "## Ranked findings", "", "Shares are of all input tokens and overlap: one request can appear in several findings.", "");
  out.push("| # | Finding | Est. tokens | Share | Evidence | Fix to try |", "|---|---|---|---|---|---|");
  report.findings.forEach((finding, n) => {
    out.push(`| ${n + 1} | ${finding.title} | ${fmt(finding.tokens)} | ${pct(finding.share)} | ${cell(finding.evidence.join("; "))} | ${cell(finding.fix)} |`);
  });

  out.push("", "## Agents", "", "| Agent | Model | Hours | Requests | Input | Floor | Avg context | Peak | Compactions | Commands | Files edited | Lines added |", "|---|---|---|---|---|---|---|---|---|---|---|---|");
  for (const agent of report.agents) {
    out.push(`| ${agent.agentPath} | ${agent.model ?? "?"} | ${agent.hours.toFixed(1)} | ${fmt(agent.requests)} | ${fmt(agent.input)} | ${fmt(agent.floorContext)} | ${fmt(agent.avgContext)} | ${fmt(agent.peakContext)} | ${agent.compactions} | ${fmt(agent.commands)} | ${agent.filesEdited} | ${fmt(agent.linesAdded)} |`);
  }

  out.push("", "## Where input tokens go", "", "Each request is labeled by the most consequential thing it did (edit > release > test > build > cloud > ci > vcs > coordinate > web > read > wait). A request with no tool call is a message. Time is wall clock until the agent's next response in the same turn, summed across agents running in parallel.", "");
  out.push("| Request kind | Requests | Input tokens | Share | Commands | Output tokens | Carried | Time |", "|---|---|---|---|---|---|---|---|");
  for (const entry of report.categories) {
    out.push(`| ${entry.category} | ${fmt(entry.requests)} | ${fmt(entry.input)} | ${pct(entry.input / Math.max(1, t.input))} | ${fmt(entry.commands)} | ${fmt(entry.outputTokens)} | ${fmt(entry.carried)} | ${(entry.seconds / 3600).toFixed(1)} h |`);
  }
  out.push("", `Carried output estimate covers ${pct(report.carriedRatio)} of all input tokens; the rest is the system prompt, instructions, messages and reasoning.`);

  out.push("", "## Heaviest outputs (by tokens carried in context)", "", "| Agent | Output tokens | Carried | Command |", "|---|---|---|---|");
  for (const entry of report.outputs) out.push(`| ${entry.agent} | ${fmt(entry.tokens)} | ${fmt(entry.carried)} | \`${cell(entry.command)}\` |`);

  out.push("", "## Most-read files", "", "| File | Reads | Exact unchanged re-reads |", "|---|---|---|");
  for (const entry of report.rereads.files) out.push(`| ${entry.file} | ${entry.reads} | ${entry.exact} |`);

  out.push("", "## Most-run build and test commands", "", "| Kind | Runs | Failures | Command |", "|---|---|---|---|");
  for (const suite of report.suites) out.push(`| ${suite.category} | ${suite.runs} | ${suite.failures} | \`${cell(suite.signature)}\` |`);

  out.push("", "## Release commands", "");
  for (const release of report.releases) out.push(`- ${new Date(release.at * 1000).toISOString().slice(5, 16).replace("T", " ")} ${release.agent}: \`${cell(release.text)}\``);

  out.push("", "## Edits by area", "", "| Area | File edits | Lines added | Lines removed |", "|---|---|---|---|");
  for (const entry of report.edits) out.push(`| ${entry.area} | ${entry.files} | ${fmt(entry.added)} | ${fmt(entry.removed)} |`);
  out.push(`\nRequests whose edits touched only docs or evidence: ${report.ceremony.requests} (${fmt(report.ceremony.input)} input tokens).`);

  out.push("", "## Method and limits", "");
  out.push("- Reads the clear-text parts of the rollout: tool calls, outputs, token usage, prompts and updates. Reasoning and inter-agent messages are encrypted and not read.");
  out.push("- Commands are classified from their text, not their effect. Edits made by scripts are detected only from obvious write calls.");
  out.push("- Carried tokens assume an output stays in context until the next compaction; this is an estimate, not a billing record.");
  out.push("- Token counts are raw. Cached input is far cheaper per token than uncached input, but both count toward usage.");
  return out.join("\n") + "\n";
}

export function renderTools(tools: ToolReport): string {
  const out: string[] = [];
  const secs = (value: number) => (value >= 60 ? (value / 60).toFixed(1) + " min" : value >= 1 ? value.toFixed(1) + " s" : Math.round(value * 1000) + " ms");
  out.push("## Time: model vs tools", "", "Wall clock inside turns, per agent. Tool time is how long a request's tool calls blocked before returning (a background command returns early and is polled).", "");
  out.push("| Agent | Active | Model | Tools | Tool share |", "|---|---|---|---|---|");
  for (const entry of tools.time) out.push(`| ${entry.agent} | ${entry.activeHours.toFixed(1)} h | ${entry.modelHours.toFixed(1)} h | ${entry.toolHours.toFixed(1)} h | ${((entry.toolHours / Math.max(1e-9, entry.activeHours)) * 100).toFixed(0)}% |`);
  out.push("", `Model time between tool calls: median ${secs(tools.modelSecondsPerRequest.p50)}, p95 ${secs(tools.modelSecondsPerRequest.p95)}. Commands per request: ${tools.commandsPerRequest.toFixed(1)}.`);
  out.push("", "Model time per step by the context that step carried:", "", "| Context | Steps | Median | p95 |", "|---|---|---|---|");
  for (const entry of tools.modelByContext) out.push(`| ${entry.bucket} | ${fmt(entry.requests)} | ${secs(entry.p50)} | ${secs(entry.p95)} |`);
  out.push("", "## Most-called tools", "", "Latency percentiles use commands that ran the tool alone; background commands are timed start to finish. Output tokens are split evenly across the steps of a compound command.", "");
  out.push("| Tool | Uses | Solo | p50 | p95 | Max | Main-step time | Output tokens | p95 output | Carried | Polls | Failures |", "|---|---|---|---|---|---|---|---|---|---|---|---|");
  for (const entry of tools.tools.slice(0, 40)) {
    out.push(`| \`${entry.key}\` | ${fmt(entry.uses)} | ${fmt(entry.solo)} | ${secs(entry.p50)} | ${secs(entry.p95)} | ${secs(entry.max)} | ${secs(entry.seconds)} | ${fmt(entry.tokens)} | ${fmt(entry.p95Tokens)} | ${fmt(entry.carried)} | ${entry.polls} | ${entry.failures} |`);
  }
  out.push("", "## Git commands", "", "| Variant | Uses | p50 | p95 | Avg output | p95 output | Carried |", "|---|---|---|---|---|---|---|");
  for (const entry of tools.git) out.push(`| \`${entry.variant}\` | ${entry.uses} | ${secs(entry.p50)} | ${secs(entry.p95)} | ${fmt(entry.avgTokens)} | ${fmt(entry.p95Tokens)} | ${fmt(entry.carried)} |`);
  return out.join("\n") + "\n";
}
