// The morning digest: once a day (9:01 in the account's own time zone) the
// control plane asks the home machine what every agent did since the last
// digest (cloud/worker/digest.mjs reads the run records), groups it by project
// and keeps it. The phone shows the newest one first on the home screen.
//
// Everything here is pure: the scheduling arithmetic and the grouping. The
// server owns storage, the sweep and the RPC.

export type DigestEvent = { at: number; kind: string; text?: string | null };

export type DigestRun = {
  id: string;
  title: string;
  task?: string;
  status: string;
  mode?: string | null;
  backend?: string | null;
  model?: string | null;
  nodeId?: string | null;
  summary?: string | null;
  merged?: boolean;
  pushed?: boolean;
  pr?: { url: string; number: number | null } | null;
  delivery?: { target: string | null; url: string | null; revision: string | null } | null;
  tokens?: number;
  firstAt: number;
  lastAt: number;
  events: DigestEvent[];
};

export type DigestRawProject = { workspaceId: string; name?: string; repo?: string | null; runs?: DigestRun[] };

export type DigestTotals = {
  agents: number;
  started: number;
  finished: number;
  merged: number;
  deployed: number;
  prs: number;
  needsYou: number;
  working: number;
  failed: number;
  tokens: number;
};

export type DigestProject = {
  workspaceId: string;
  name: string;
  repo: string | null;
  totals: DigestTotals;
  runs: DigestRun[];
  timeline: Array<DigestEvent & { runId: string; title: string }>;
};

export type Digest = {
  day: string;
  timeZone: string;
  since: number;
  until: number;
  generatedAt: number;
  headline: string;
  lines: string[];
  totals: DigestTotals;
  projects: DigestProject[];
};

export const DIGEST_HOUR = 9;
export const DIGEST_MINUTE = 1;
export const DEFAULT_DIGEST_TIME_ZONE = "America/Los_Angeles";
const MAX_WINDOW_MS = 7 * 24 * 3600_000;

/** A time zone name Intl accepts, or null. */
export function validTimeZone(value: unknown): string | null {
  if (typeof value !== "string" || !value || value.length > 64) {
    return null;
  }
  try {
    new Intl.DateTimeFormat("en-US", { timeZone: value });
    return value;
  } catch {
    return null;
  }
}

/** The wall clock in `timeZone` at `ms`: the local day (YYYY-MM-DD), hour and minute. */
export function zonedClock(ms: number, timeZone: string): { day: string; hour: number; minute: number } {
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
  }).formatToParts(new Date(ms));
  const get = (type: string) => parts.find((part) => part.type === type)?.value ?? "00";
  return { day: `${get("year")}-${get("month")}-${get("day")}`, hour: Number(get("hour")) % 24, minute: Number(get("minute")) };
}

/**
 * The local day whose digest is due at `now`, or null. Due once the account's
 * clock has passed 9:01 and no digest exists yet for that day.
 */
export function digestDueDay(now: number, timeZone: string, lastDay: string | null, at = { hour: DIGEST_HOUR, minute: DIGEST_MINUTE }): string | null {
  const clock = zonedClock(now, timeZone);
  if (clock.hour * 60 + clock.minute < at.hour * 60 + at.minute) {
    return null;
  }
  return lastDay && lastDay >= clock.day ? null : clock.day;
}

/** The window a new digest covers: from where the previous one ended, at most a week. */
export function digestWindow(now: number, previousUntil: number | null): { since: number; until: number } {
  const floor = now - MAX_WINDOW_MS;
  const since = previousUntil && previousUntil > floor && previousUntil < now ? previousUntil : now - 24 * 3600_000;
  return { since, until: now };
}

function emptyTotals(): DigestTotals {
  return { agents: 0, started: 0, finished: 0, merged: 0, deployed: 0, prs: 0, needsYou: 0, working: 0, failed: 0, tokens: 0 };
}

function addRun(totals: DigestTotals, run: DigestRun): void {
  totals.agents += 1;
  const kinds = new Set(run.events.map((event) => event.kind));
  if (kinds.has("started")) totals.started += 1;
  if (kinds.has("finished") || kinds.has("merged") || kinds.has("deployed")) totals.finished += 1;
  if (run.merged) totals.merged += 1;
  if (run.delivery) totals.deployed += 1;
  if (run.pr) totals.prs += 1;
  if (run.status === "waiting" || run.status === "review") totals.needsYou += 1;
  if (run.status === "working") totals.working += 1;
  if (run.status === "failed") totals.failed += 1;
  totals.tokens += Number(run.tokens) || 0;
}

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

export function composeDigest(
  raw: DigestRawProject[],
  options: { day: string; timeZone: string; since: number; until: number; generatedAt?: number },
): Digest {
  const totals = emptyTotals();
  const projects: DigestProject[] = [];
  for (const project of raw) {
    const runs = (project.runs ?? []).filter((run) => run && Array.isArray(run.events) && run.events.length > 0);
    if (runs.length === 0) {
      continue;
    }
    const projectTotals = emptyTotals();
    for (const run of runs) {
      addRun(projectTotals, run);
      addRun(totals, run);
    }
    const timeline = runs
      .flatMap((run) => run.events.map((event) => ({ ...event, runId: run.id, title: run.title })))
      .sort((a, b) => a.at - b.at);
    projects.push({
      workspaceId: project.workspaceId,
      name: project.name || project.repo || "Project",
      repo: project.repo ?? null,
      totals: projectTotals,
      runs: runs.slice().sort((a, b) => b.lastAt - a.lastAt),
      timeline,
    });
  }
  projects.sort((a, b) => b.totals.agents - a.totals.agents || lastAt(b) - lastAt(a));
  const { headline, lines } = describe(totals, projects);
  return {
    day: options.day,
    timeZone: options.timeZone,
    since: options.since,
    until: options.until,
    generatedAt: options.generatedAt ?? Date.now(),
    headline,
    lines,
    totals,
    projects,
  };
}

function lastAt(project: DigestProject): number {
  return project.timeline[project.timeline.length - 1]?.at ?? 0;
}

function describe(totals: DigestTotals, projects: DigestProject[]): { headline: string; lines: string[] } {
  if (totals.agents === 0) {
    return { headline: "A quiet stretch: no agent ran since the last digest.", lines: [] };
  }
  const where = projects.length === 1 ? `in ${projects[0].name}` : `across ${plural(projects.length, "project")}`;
  const done: string[] = [];
  if (totals.finished) done.push(`${totals.finished} finished`);
  if (totals.merged) done.push(`${totals.merged} merged`);
  if (totals.deployed) done.push(`${totals.deployed} deployed`);
  if (totals.prs) done.push(plural(totals.prs, "PR"));
  const headline = `${plural(totals.agents, "agent")} worked ${where}${done.length ? `: ${done.join(", ")}` : ""}.`;
  const lines: string[] = [];
  if (totals.needsYou) lines.push(`${plural(totals.needsYou, "agent")} ${totals.needsYou === 1 ? "needs" : "need"} you.`);
  if (totals.working) lines.push(`${plural(totals.working, "agent")} still working.`);
  if (totals.failed) lines.push(`${plural(totals.failed, "agent")} failed.`);
  return { headline, lines };
}

/** The short form the home screen card and the app state carry. */
export function digestPreview(digest: Digest): { day: string; headline: string; lines: string[]; totals: DigestTotals; projects: string[] } {
  return {
    day: digest.day,
    headline: digest.headline,
    lines: digest.lines,
    totals: digest.totals,
    projects: digest.projects.map((project) => project.name).slice(0, 6),
  };
}
