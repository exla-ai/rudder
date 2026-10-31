// Turns normalized traces into cost centers and ranked waste findings.
//
// Token attribution: each model request resends the whole context, so a
// command output of T tokens costs roughly T on every later request until the
// next compaction ("carried" tokens). Request-level numbers use the actual
// input tokens Codex billed for that request.
import { dominant, redact, signature, toolCategory } from "./classify.ts";
import type { Category, Command, Request, Trace } from "./model.ts";

export interface Finding {
  title: string;
  tokens: number;
  share: number;
  evidence: string[];
  fix: string;
}

export interface AgentSummary {
  id: string;
  agentPath: string;
  model?: string;
  requests: number;
  input: number;
  cached: number;
  output: number;
  reasoning: number;
  hours: number;
  compactions: number;
  avgContext: number;
  peakContext: number;
  // Smallest request: roughly the fixed system prompt, tools and instructions.
  floorContext: number;
  commands: number;
  filesEdited: number;
  linesAdded: number;
}

interface Bucket {
  category: Category;
  requests: number;
  input: number;
  commands: number;
  outputTokens: number;
  carried: number;
  // Wall clock from this request to the agent's next response in the same turn.
  seconds: number;
}

export interface Report {
  agents: AgentSummary[];
  totals: { requests: number; input: number; cached: number; output: number; reasoning: number };
  credits?: { used: number; windowFirst: number; windowLast: number };
  categories: Bucket[];
  waits: { requests: number; input: number; emptyPolls: number; clockCalls: number; logChecks: number };
  rereads: { fileRepeats: number; exact: number; carried: number; files: { file: string; reads: number; exact: number }[] };
  outputs: { agent: string; command: string; tokens: number; carried: number }[];
  suites: { signature: string; category: Category; runs: number; failures: number }[];
  failures: { total: number; input: number; repeated: { signature: string; failures: number }[] };
  releases: { at: number; agent: string; text: string }[];
  edits: { area: string; files: number; added: number; removed: number }[];
  ceremony: { requests: number; input: number };
  coordination: { calls: number; requests: number; input: number; subagentInput: number };
  context: { target: number; excess: number; floorTokens: number; compactAt: number; average: number };
  carriedRatio: number;
  findings: Finding[];
}

const BIG_OUTPUT = 2_000;
// Longer gaps inside a turn are treated as idle, not work.
const MAX_GAP_SECONDS = 20 * 60;
// A context size agents can usually work within; tokens above it are the
// most a smaller auto-compaction limit could save.
const CONTEXT_TARGET = 150_000;

export function analyze(traces: Trace[]): Report {
  const buckets = new Map<Category, Bucket>();
  const bucket = (category: Category) => {
    let found = buckets.get(category);
    if (!found) buckets.set(category, (found = { category, requests: 0, input: 0, commands: 0, outputTokens: 0, carried: 0, seconds: 0 }));
    return found;
  };
  const report: Report = {
    agents: [],
    totals: { requests: 0, input: 0, cached: 0, output: 0, reasoning: 0 },
    categories: [],
    waits: { requests: 0, input: 0, emptyPolls: 0, clockCalls: 0, logChecks: 0 },
    rereads: { fileRepeats: 0, exact: 0, carried: 0, files: [] },
    outputs: [],
    suites: [],
    failures: { total: 0, input: 0, repeated: [] },
    releases: [],
    edits: [],
    ceremony: { requests: 0, input: 0 },
    coordination: { calls: 0, requests: 0, input: 0, subagentInput: 0 },
    context: { target: CONTEXT_TARGET, excess: 0, floorTokens: 0, compactAt: 0, average: 0 },
    carriedRatio: 0,
    findings: [],
  };
  const suites = new Map<string, Report["suites"][number]>();
  const failing = new Map<string, number>();
  const edits = new Map<string, Report["edits"][number]>();
  const fileReads = new Map<string, { file: string; reads: number; exact: number }>();
  let carriedTotal = 0;
  let bigOutputs = 0;
  let bigCarried = 0;
  let creditsUsed = 0;
  let windowFirst = Infinity;
  let windowLast = 0;
  const compactAt: number[] = [];

  for (const trace of traces) {
    const summary = summarize(trace);
    report.agents.push(summary);
    report.context.floorTokens += summary.floorContext * summary.requests;
    for (const at of trace.compactions) if (at > 0 && trace.requests[at - 1]) compactAt.push(trace.requests[at - 1].input);
    for (const request of trace.requests) report.context.excess += Math.max(0, request.input - CONTEXT_TARGET);
    if (trace.credits) {
      creditsUsed = Math.max(creditsUsed, trace.credits.first - trace.credits.last);
      windowFirst = Math.min(windowFirst, trace.credits.windowFirst);
      windowLast = Math.max(windowLast, trace.credits.windowLast);
    }
    if (trace.parentId) report.coordination.subagentInput += sum(trace.requests, (request) => request.input);
    const rel = (file: string) => relative(trace.cwd, file);
    const lastEdit = new Map<string, number>();
    const lastRange = new Map<string, number>();
    const compactionBefore = (index: number) => Math.max(-1, ...trace.compactions.filter((at) => at <= index));
    const nextCompaction = (index: number) => trace.compactions.find((at) => at > index) ?? trace.requests.length;

    trace.requests.forEach((request, index) => {
      const span = Math.max(0, nextCompaction(index) - index - 1);
      const category = requestCategory(request);
      const target = bucket(category);
      const next = trace.requests[index + 1];
      target.requests++;
      target.input += request.input;
      if (next && next.turnId === request.turnId) target.seconds += Math.min(MAX_GAP_SECONDS, Math.max(0, next.at - request.at));
      addTotals(report.totals, request);
      if (category === "wait") {
        report.waits.requests++;
        report.waits.input += request.input;
      }
      if (category === "coordinate") {
        report.coordination.requests++;
        report.coordination.input += request.input;
      }
      let failed = false;
      const areas = new Set<string>();

      for (const call of request.calls) {
        report.waits.emptyPolls += call.emptyPolls;
        for (const tool of call.tools) {
          if (tool === "clock__curr_time") report.waits.clockCalls++;
          if (toolCategory(tool) === "coordinate") report.coordination.calls++;
        }
        for (const patch of call.patches) {
          const file = rel(patch.path);
          const area = areaOf(file);
          areas.add(area);
          const entry = edits.get(area) ?? { area, files: 0, added: 0, removed: 0 };
          entry.files++;
          entry.added += patch.added;
          entry.removed += patch.removed;
          edits.set(area, entry);
          lastEdit.set(file, index);
        }
        const fallback = call.commands.length === 1 ? call.outputTokens : 0;
        for (const command of call.commands) {
          const tokens = command.outputTokens || fallback;
          const commandCategory = dominant(command.segments.map((segment) => segment.category));
          const commandBucket = bucket(commandCategory);
          commandBucket.commands++;
          commandBucket.outputTokens += tokens;
          commandBucket.carried += tokens * span;
          carriedTotal += tokens * span;
          if (tokens >= BIG_OUTPUT) {
            bigOutputs++;
            bigCarried += tokens * span;
          }
          report.outputs.push({ agent: trace.agentPath, command: signature(command.text), tokens, carried: tokens * span });
          if (command.exitCode && command.exitCode !== 0) {
            failed = true;
            report.failures.total++;
            const key = signature(command.text);
            failing.set(key, (failing.get(key) ?? 0) + 1);
          }
          if (commandCategory === "test" || commandCategory === "build") noteSuite(suites, command, commandCategory);
          for (const segment of command.segments) {
            if (segment.category === "release") report.releases.push({ at: request.at, agent: trace.agentPath, text: redact(segment.text).slice(0, 160) });
            if (segment.category === "wait" && (/\btail\b/.test(segment.text) || segment.files.some((file) => /\.log$/.test(file)))) report.waits.logChecks++;
            if (segment.writes) segment.files.forEach((file) => lastEdit.set(rel(file), index));
            if (segment.category !== "read") continue;
            const share = tokens / Math.max(1, command.segments.length);
            for (const raw of segment.files) {
              const file = rel(raw);
              const stats = fileReads.get(file) ?? { file, reads: 0, exact: 0 };
              stats.reads++;
              if (stats.reads > 1) report.rereads.fileRepeats++;
              const key = file + "#" + rangeOf(segment.text);
              const previous = lastRange.get(key);
              // Same range again, with no edit since the last read and no compaction
              // in between that would have dropped it from context.
              const unchanged = previous !== undefined && previous < index && (lastEdit.get(file) ?? -1) < previous && compactionBefore(index) <= previous;
              if (unchanged) {
                stats.exact++;
                report.rereads.exact++;
                report.rereads.carried += Math.round(share * (span + 1));
              }
              lastRange.set(key, index);
              fileReads.set(file, stats);
            }
          }
        }
      }
      if (failed) report.failures.input += request.input;
      if (areas.size && [...areas].every((area) => area === "docs" || area === "evidence")) {
        report.ceremony.requests++;
        report.ceremony.input += request.input;
      }
    });
  }

  report.credits = creditsUsed || windowLast ? { used: creditsUsed, windowFirst: Number.isFinite(windowFirst) ? windowFirst : 0, windowLast } : undefined;
  report.categories = [...buckets.values()].sort((a, b) => b.input - a.input);
  report.outputs = report.outputs.sort((a, b) => b.carried - a.carried).slice(0, 15);
  report.suites = [...suites.values()].sort((a, b) => b.runs - a.runs).slice(0, 12);
  report.failures.repeated = [...failing].map(([signature, failures]) => ({ signature, failures })).filter((entry) => entry.failures > 1).sort((a, b) => b.failures - a.failures).slice(0, 8);
  report.edits = [...edits.values()].sort((a, b) => b.added - a.added);
  report.rereads.files = [...fileReads.values()].filter((entry) => entry.reads > 2).sort((a, b) => b.reads - a.reads).slice(0, 12);
  report.carriedRatio = report.totals.input ? carriedTotal / report.totals.input : 0;
  report.context.compactAt = compactAt.sort((a, b) => a - b)[Math.floor(compactAt.length / 2)] ?? 0;
  report.context.average = report.totals.requests ? report.totals.input / report.totals.requests : 0;
  report.findings = findings(report, bigOutputs, bigCarried);
  return report;
}

function findings(report: Report, bigOutputs: number, bigCarried: number): Finding[] {
  const total = Math.max(1, report.totals.input);
  const of = (category: Category) => report.categories.find((entry) => entry.category === category);
  const make = (title: string, tokens: number, evidence: string[], fix: string): Finding => ({ title, tokens, share: tokens / total, evidence, fix });
  const top = report.outputs.slice(0, 3).map((entry) => `${fmt(entry.tokens)} tokens carried ${fmt(entry.carried)}: \`${entry.command}\``);
  const docs = report.edits.filter((entry) => entry.area === "docs" || entry.area === "evidence");
  const code = report.edits.filter((entry) => entry.area === "code" || entry.area === "tests");
  const releaseBuckets = (of("release")?.input ?? 0) + (of("ci")?.input ?? 0);
  const verify = (of("test")?.input ?? 0) + (of("build")?.input ?? 0);
  const repeatedSuites = report.suites.filter((suite) => suite.runs > 2);
  const subagents = report.agents.filter((agent) => agent.agentPath !== "/root").sort((a, b) => b.input - a.input);
  const compactions = sum(report.agents, (agent) => agent.compactions);
  const floors = report.agents.filter((agent) => agent.requests > 10).map((agent) => agent.floorContext);
  const list = [
    make(`Context fills the window before compacting`, report.context.excess,
      [`average request carries ${fmt(report.context.average)} tokens; the fixed floor is only ${fmt(Math.min(...floors))}-${fmt(Math.max(...floors))}`, `median compaction happens at ${fmt(report.context.compactAt)} tokens (${compactions} compactions)`, `tokens above ${fmt(CONTEXT_TARGET)} per request: ${fmt(report.context.excess)}`],
      `Have Rudder launch agents with a lower auto-compaction limit (Codex model_auto_compact_token_limit, about ${fmt(CONTEXT_TARGET)}) and re-inject the task brief after compaction; check that re-reads do not rise.`),
    make("Fixed prompt resent on every request", report.context.floorTokens,
      [`each request starts from ${fmt(Math.min(...floors))}-${fmt(Math.max(...floors))} tokens of system prompt, tools, skills and instructions`, `${fmt(report.totals.requests)} requests`],
      "Trim the injected Rudder prompt, AGENTS.md and the always-listed skills and tools; load the rest on demand."),
    make("Large command outputs stay in context", bigCarried,
      [`${bigOutputs} outputs over ${fmt(BIG_OUTPUT)} tokens, each resent on every later request until compaction`, ...top],
      "Cap shell output by default; have test, build and deploy wrappers print a status line plus failures only."),
    make("Requests that only wait or poll", report.waits.input,
      [`${report.waits.requests} requests did nothing but wait`, `${report.waits.emptyPolls} empty polls of running commands`, `${report.waits.clockCalls} clock checks`, `${report.waits.logChecks} log checks (tail / log reads)`],
      "Give agents one blocking wait (for a job, CI run or deploy) that returns a single result instead of polling."),
    make("Re-reading unchanged file ranges", report.rereads.carried,
      [`${report.rereads.exact} exact re-reads with no edit or compaction in between`, `${report.rereads.fileRepeats} repeat reads of an already-read file overall`, ...report.rereads.files.slice(0, 3).map((entry) => `${entry.file}: ${entry.reads} reads, ${entry.exact} exact repeats`)],
      "Keep a per-session read cache in Rudder and tell the agent when a range is unchanged since it last read it."),
    make("Requests that only edit docs or evidence", report.ceremony.input,
      [`${report.ceremony.requests} requests whose edits touched only docs or evidence files`, `docs/evidence lines added: ${fmt(sum(docs, (entry) => entry.added))} vs code/tests: ${fmt(sum(code, (entry) => entry.added))}`],
      "Generate delivery evidence mechanically from CI and deploy results instead of having the agent write it."),
    make("Release and CI loop", releaseBuckets,
      [`${report.releases.length} release commands (npm version, tags, pushes of tags)`, `${of("ci")?.requests ?? 0} requests spent checking CI or the registry`],
      "Run the full platform matrix before tagging, behind one ship command that tags only when every gate is green."),
    make("Repeated build and test suites", verify,
      [`${of("test")?.requests ?? 0} test requests, ${of("build")?.requests ?? 0} build requests`, ...repeatedSuites.slice(0, 3).map((suite) => `${suite.runs}x \`${suite.signature}\``)],
      "Cache verification results by input hash so unchanged suites are skipped, and run only affected tests between milestones."),
    make("Sub-agent sessions (a cost center, not waste by itself)", report.coordination.subagentInput,
      [`${subagents.length} sub-agents used ${fmt(report.coordination.subagentInput)} input tokens (${(report.coordination.subagentInput / total * 100).toFixed(0)}% of the tree) and made ${fmt(sum(subagents, (agent) => agent.linesAdded))} of ${fmt(sum(report.agents, (agent) => agent.linesAdded))} edited lines`, ...subagents.slice(0, 4).map((agent) => `${agent.agentPath}: ${agent.hours.toFixed(1)} h, ${fmt(agent.input)} tokens, ${fmt(agent.linesAdded)} lines, first request ${fmt(agent.floorContext)}+`)],
      "Each sub-agent pays the same large-context cost as the main agent; apply the same compaction limit and start them without the parent's history."),
    make("Coordination-only requests", report.coordination.input,
      [`${report.coordination.requests} requests did nothing but message, spawn or follow up with agents`, `${report.coordination.calls} coordination calls in total`],
      "Brief sub-agents once with a scoped file list; have them return a structured result rather than chatting."),
    make("Failed commands", report.failures.input,
      [`${report.failures.total} commands exited non-zero`, ...report.failures.repeated.slice(0, 3).map((entry) => `${entry.failures}x failed: \`${entry.signature}\``)],
      "Record known failure fixes in Rudder memory so the next attempt starts from the fix."),
  ];
  return list.sort((a, b) => b.tokens - a.tokens);
}

function requestCategory(request: Request): Category {
  if (!request.calls.length) return "message";
  const categories: Category[] = [];
  for (const call of request.calls) {
    for (const tool of call.tools) {
      const category = toolCategory(tool);
      if (category) categories.push(category);
    }
    if (call.emptyPolls) categories.push("wait");
    for (const command of call.commands) categories.push(...command.segments.map((segment) => segment.category));
  }
  return dominant(categories);
}

function noteSuite(suites: Map<string, Report["suites"][number]>, command: Command, category: Category) {
  const main = command.segments.find((segment) => segment.category === category) ?? command.segments[0];
  const key = signature(main?.text ?? command.text).replace(/\s*[>|].*$/, "").slice(0, 90);
  const entry = suites.get(key) ?? { signature: key, category, runs: 0, failures: 0 };
  entry.runs++;
  if (command.exitCode && command.exitCode !== 0) entry.failures++;
  suites.set(key, entry);
}

function summarize(trace: Trace): AgentSummary {
  const inputs = trace.requests.map((request) => request.input);
  const calls = trace.requests.flatMap((request) => request.calls);
  const positive = inputs.filter((value) => value > 0);
  return {
    id: trace.id,
    agentPath: trace.agentPath,
    model: trace.model,
    requests: trace.requests.length,
    input: sum(inputs, (value) => value),
    cached: sum(trace.requests, (request) => request.cached),
    output: sum(trace.requests, (request) => request.output),
    reasoning: sum(trace.requests, (request) => request.reasoning),
    hours: (trace.endedAt - trace.startedAt) / 3600,
    compactions: trace.compactions.length,
    avgContext: inputs.length ? sum(inputs, (value) => value) / inputs.length : 0,
    peakContext: Math.max(0, ...inputs),
    floorContext: positive.length ? Math.min(...positive) : 0,
    commands: sum(calls, (call) => call.commands.length),
    filesEdited: new Set(calls.flatMap((call) => call.patches.map((patch) => patch.path))).size,
    linesAdded: sum(calls, (call) => sum(call.patches, (patch) => patch.added)),
  };
}

function addTotals(totals: Report["totals"], request: Request) {
  totals.requests++;
  totals.input += request.input;
  totals.cached += request.cached;
  totals.output += request.output;
  totals.reasoning += request.reasoning;
}

function areaOf(file: string): string {
  if (/(^|\/)evidence\/|(^|\/)delivery-[^/]*\.json$|SHA256SUMS$/.test(file)) return "evidence";
  if (/(^|\/)tests?\/|\.test\.|_tests?\.rs$|(^|\/)fixtures?\//.test(file)) return "tests";
  if (/\.md$|(^|\/)docs\//.test(file)) return "docs";
  return "code";
}

function rangeOf(text: string): string {
  const sed = /sed -n\s+['"]?([\d,$]+)p/.exec(text);
  if (sed) return sed[1];
  const head = /\b(head|tail)\s+-n?\s*(\d+)/.exec(text);
  if (head) return head[1] + head[2];
  return /\b(rg|grep)\b/.test(text) ? "search:" + signature(text) : "all";
}

function relative(cwd: string | undefined, file: string): string {
  const clean = file.replace(/^\.\//, "");
  return cwd && clean.startsWith(cwd + "/") ? clean.slice(cwd.length + 1) : clean;
}

function sum<T>(items: T[], value: (item: T) => number): number {
  return items.reduce((total, item) => total + value(item), 0);
}

export function fmt(value: number): string {
  if (value >= 1e9) return (value / 1e9).toFixed(1) + "B";
  if (value >= 1e6) return (value / 1e6).toFixed(1) + "M";
  if (value >= 1e3) return (value / 1e3).toFixed(1) + "K";
  return String(Math.round(value));
}
