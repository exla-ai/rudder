// Which tools agents call, how long they block, and how much output they put
// into context. Feeds decisions about which tools to make faster or quieter.
import { dominant, programOf } from "./classify.ts";
import type { Command, Segment, Trace } from "./model.ts";

export interface ToolStat {
  key: string;
  // Segments run with this tool, including inside compound commands.
  uses: number;
  // Commands that ran this tool alone; latency percentiles use only these.
  solo: number;
  p50: number;
  p95: number;
  max: number;
  // Wall clock of commands where this tool was the main step.
  seconds: number;
  tokens: number;
  carried: number;
  p95Tokens: number;
  failures: number;
  polls: number;
}

export interface TimeSplit {
  agent: string;
  activeHours: number;
  toolHours: number;
  modelHours: number;
}

export interface GitVariant {
  variant: string;
  uses: number;
  p50: number;
  p95: number;
  avgTokens: number;
  p95Tokens: number;
  carried: number;
}

export interface ToolReport {
  tools: ToolStat[];
  time: TimeSplit[];
  git: GitVariant[];
  modelSecondsPerRequest: { p50: number; p95: number };
  // Median model seconds per step, by how much context the request carried.
  modelByContext: { bucket: string; requests: number; p50: number; p95: number }[];
  commandsPerRequest: number;
}

const MAX_GAP_SECONDS = 20 * 60;
// Shell plumbing and separators, not tools.
const NOISE = new Set(["", "cd", "export", "set", "source", ".", "unset", "trap", "true", "false", "exit", "return", "local", "fi", "done", "esac", "for", "while", "case", "umask", "echo", "printf", "{", "}"]);
const SUBCOMMAND = new Set(["git", "jj", "cargo", "npm", "pnpm", "yarn", "bun", "npx", "gh", "flyctl", "fly", "docker", "tailscale", "rudder"]);

export function toolKey(segment: Segment): string {
  const { prog, args } = programOf(segment.text);
  if (!prog) return "(none)";
  if (prog === "git" || prog === "jj") {
    const sub = args.filter((arg, n) => !arg.startsWith("-") && args[n - 1] !== "-C" && args[n - 1] !== "-R")[0] ?? "";
    return `${prog} ${sub}`.trim();
  }
  if (prog === "npm" && args[0] === "run") return `npm run ${args[1] ?? ""}`.trim();
  if (SUBCOMMAND.has(prog)) return `${prog} ${args.find((arg) => !arg.startsWith("-") && !arg.startsWith("+")) ?? ""}`.trim();
  if (prog === "node" || prog === "python3" || prog === "python") {
    if (args.includes("--test")) return `${prog} --test`;
    if (args.includes("--check")) return `${prog} --check`;
    if (segment.text.includes("<<") || args.includes("-c") || args.includes("-e") || args[0] === "-") return `${prog} inline`;
    return `${prog} script`;
  }
  if (prog === "sed" && args.includes("-n")) return "sed -n";
  return prog;
}

export function gitVariant(segment: Segment): string | null {
  const { prog, args } = programOf(segment.text);
  if (prog !== "git") return null;
  const sub = args.filter((arg) => !arg.startsWith("-"))[0] ?? "";
  if (sub === "diff") {
    if (args.includes("--check")) return "git diff --check";
    if (args.some((arg) => /^--(stat|shortstat|numstat|name-only|name-status)$/.test(arg))) return "git diff (summary: --stat/--name-only)";
    if (args.includes("--")) return "git diff -- <paths>";
    return "git diff (whole tree)";
  }
  if (sub === "status") return "git status";
  if (sub === "show") return "git show";
  if (sub === "log") return "git log";
  return null;
}

export function analyzeTools(traces: Trace[]): ToolReport {
  const stats = new Map<string, { uses: number; latencies: number[]; seconds: number; tokens: number[]; carried: number; failures: number; polls: number }>();
  const git = new Map<string, { latencies: number[]; tokens: number[]; carried: number }>();
  const stat = (key: string) => {
    let found = stats.get(key);
    if (!found) stats.set(key, (found = { uses: 0, latencies: [], seconds: 0, tokens: [], carried: 0, failures: 0, polls: 0 }));
    return found;
  };
  const time: TimeSplit[] = [];
  const modelSeconds: number[] = [];
  const byContext = new Map<number, number[]>();
  let commands = 0;
  let requestsWithCalls = 0;

  for (const trace of traces) {
    const nextCompaction = (index: number) => trace.compactions.find((at) => at > index) ?? trace.requests.length;
    let active = 0;
    let tool = 0;
    trace.requests.forEach((request, index) => {
      const next = trace.requests[index + 1];
      const doneAt = Math.max(request.at, ...request.calls.map((call) => call.doneAt ?? request.at));
      if (next && next.turnId === request.turnId) {
        const gap = Math.min(MAX_GAP_SECONDS, Math.max(0, next.at - request.at));
        const blocked = Math.min(gap, Math.max(0, doneAt - request.at));
        active += gap;
        tool += blocked;
        if (request.calls.length) {
          modelSeconds.push(gap - blocked);
          // The next request's own generation time is what its context slows down.
          const bucket = Math.min(4, Math.floor(next.input / 100_000));
          byContext.set(bucket, [...(byContext.get(bucket) ?? []), gap - blocked]);
        }
      }
      if (request.calls.length) requestsWithCalls++;
      const span = Math.max(0, nextCompaction(index) - index - 1);
      for (const call of request.calls) {
        for (const command of call.commands) {
          commands++;
          noteCommand(command, span, stat, git);
        }
      }
    });
    time.push({ agent: trace.agentPath, activeHours: active / 3600, toolHours: tool / 3600, modelHours: (active - tool) / 3600 });
  }

  const tools = [...stats].map(([key, entry]): ToolStat => ({
    key,
    uses: entry.uses,
    solo: entry.latencies.length,
    p50: quantile(entry.latencies, 0.5),
    p95: quantile(entry.latencies, 0.95),
    max: Math.max(0, ...entry.latencies),
    seconds: entry.seconds,
    tokens: entry.tokens.reduce((sum, value) => sum + value, 0),
    carried: entry.carried,
    p95Tokens: quantile(entry.tokens, 0.95),
    failures: entry.failures,
    polls: entry.polls,
  })).sort((a, b) => b.uses - a.uses);
  const variants = [...git].map(([variant, entry]): GitVariant => ({
    variant,
    uses: entry.tokens.length,
    p50: quantile(entry.latencies, 0.5),
    p95: quantile(entry.latencies, 0.95),
    avgTokens: entry.tokens.reduce((sum, value) => sum + value, 0) / Math.max(1, entry.tokens.length),
    p95Tokens: quantile(entry.tokens, 0.95),
    carried: entry.carried,
  })).sort((a, b) => b.uses - a.uses);
  return {
    tools,
    time,
    git: variants,
    modelSecondsPerRequest: { p50: quantile(modelSeconds, 0.5), p95: quantile(modelSeconds, 0.95) },
    modelByContext: [...byContext].sort((a, b) => a[0] - b[0]).map(([bucket, values]) => ({
      bucket: bucket === 4 ? "400K+" : `${bucket * 100}-${bucket * 100 + 100}K`,
      requests: values.length,
      p50: quantile(values, 0.5),
      p95: quantile(values, 0.95),
    })),
    commandsPerRequest: commands / Math.max(1, requestsWithCalls),
  };
}

function noteCommand(
  command: Command,
  span: number,
  stat: (key: string) => { uses: number; latencies: number[]; seconds: number; tokens: number[]; carried: number; failures: number; polls: number },
  git: Map<string, { latencies: number[]; tokens: number[]; carried: number }>,
) {
  const segments = command.segments.filter((segment) => !NOISE.has(programOf(segment.text).prog));
  if (!segments.length) return;
  const share = command.outputTokens / segments.length;
  const solo = segments.length === 1;
  // Time goes to the step most likely to dominate it: a build over a read.
  const main = segments.find((segment) => segment.category === dominant(segments.map((each) => each.category))) ?? segments[0];
  for (const segment of segments) {
    const entry = stat(toolKey(segment));
    entry.uses++;
    entry.tokens.push(share);
    entry.carried += share * span;
    if (solo && command.wallSeconds !== undefined) entry.latencies.push(command.wallSeconds);
    if (segment === main) {
      entry.seconds += command.wallSeconds ?? 0;
      entry.polls += command.polls;
      if (command.exitCode && command.exitCode !== 0) entry.failures++;
    }
    const variant = gitVariant(segment);
    if (variant) {
      let found = git.get(variant);
      if (!found) git.set(variant, (found = { latencies: [], tokens: [], carried: 0 }));
      found.tokens.push(share);
      found.carried += share * span;
      if (solo && command.wallSeconds !== undefined) found.latencies.push(command.wallSeconds);
    }
  }
}

function quantile(values: number[], q: number): number {
  if (!values.length) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))];
}
