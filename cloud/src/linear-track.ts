// Rudder tracks your work in Linear: each piece of new work in a project linked
// to Linear is decided once, by one model call, as attach (an open issue covers
// it), create (any other real work, small fixes included) or skip (not work:
// questions, chat, one-word or read-only prompts). A piece of work is an agent,
// a whole Rudder plan (judged by its goal), or one user turn of a long session
// (main row). Branches follow their parent's decision without a model call.
// integrations.ts persists the decisions and syncs progress quietly; this file
// holds the prompt, the strict schema, the validation, the plan and branch
// helpers and the text that lands in Linear.

import { structuredJson, type CallWording } from "./linear-tasks.js";
import type { LinearIssue } from "./linear.js";

export type TrackAgent = {
  id?: string | null;
  title?: string | null;
  task?: string | null;
  mode?: string | null;
  swarmExperiment?: boolean | null;
  summary?: string | null;
  lastMessage?: string | null;
  createdAt?: number | null;
  planId?: string | null;
  nodeId?: string | null;
};

/** What a plan is, for the classifier: its goal and the pieces its workers took. */
export type PlanContext = { goal: string; pieces: string[] };

export type TrackDecision =
  | { action: "attach"; issue: LinearIssue; reason: string }
  | { action: "create"; title: string; reason: string }
  | { action: "skip"; reason: string };

const TASK_LIMIT = 4000;
const ISSUE_TITLE_LIMIT = 160;
const TITLE_LIMIT = 120;
const REASON_LIMIT = 300;
const SUMMARY_LIMIT = 500;
const MERGE_LINE_LIMIT = 200;

// Rows that are not a piece of work: the main agent, a plan's orchestrator, a
// review-all pass, a one-off question and a /gam reviewer. Everything else
// ("execute", "plan", or no mode from an older home) is a candidate.
const UNTRACKED_MODES = new Set(["main", "rudder-plan", "review-all", "one-off", "gam-adversarial"]);
// An agent started from a Linear issue carries "Linear ENG-7: <title>" (linearTaskText).
const LINEAR_STARTED = /\bLinear [A-Za-z][A-Za-z0-9_]*-\d+:/;

// What the user asked, without the framing Rudder wraps a task in. The same
// rule as userTask in cloud/worker/transcripts.mjs (homes already apply it;
// this covers older ones).
export function userTask(raw: unknown): string {
  let text = String(raw ?? "").trim();
  text = text.replace(/^Objective: [^\n]*\nDone when: [^\n]*\n+/, "");
  const context = text.indexOf("Rudder-specific context injected by Rudder");
  if (context >= 0) {
    const after = text.slice(context).split(/\n\s*\n/).slice(1).join("\n\n").trim();
    text = after || text.slice(0, context).trim();
  }
  return text.trim();
}

// A branch (`b` in the dashboard) is started with "Branch of: <the parent's task>".
const BRANCH_PREFIX = /^\s*Branch of: /;
const COMPARE_CHARS = 300;
const PIECE_LIMIT = 15;

/** The parent's task a branch was made from, or null when the run is not a branch. */
export function branchSourceTask(task: unknown): string | null {
  const text = String(task ?? "");
  return BRANCH_PREFIX.test(text) ? userTask(text.replace(BRANCH_PREFIX, "")) : null;
}

const compareKey = (text: string) => text.replace(/\s+/g, " ").trim().slice(0, COMPARE_CHARS).toLowerCase();

/**
 * The run a branch was made from: the newest other run in the same project,
 * created before the branch, whose task is the one the branch names. Both
 * sides are clipped by the home, so the first few hundred characters decide.
 */
export function findBranchParent<T extends TrackAgent>(branch: T, agents: T[]): T | null {
  const source = branchSourceTask(branch.task);
  if (!source) {
    return null;
  }
  const wanted = compareKey(source);
  if (!wanted) {
    return null;
  }
  const createdAt = typeof branch.createdAt === "number" ? branch.createdAt : Infinity;
  let best: T | null = null;
  for (const candidate of agents) {
    if (!candidate?.id || candidate.id === branch.id) {
      continue;
    }
    const at = typeof candidate.createdAt === "number" ? candidate.createdAt : null;
    if (at !== null && at > createdAt) {
      continue;
    }
    if (compareKey(userTask(candidate.task)) !== wanted) {
      continue;
    }
    if (!best || (at ?? 0) > (typeof best.createdAt === "number" ? best.createdAt : 0)) {
      best = candidate;
    }
  }
  return best;
}

// A plan worker's prompt (native rudder_plan_worker_prompt): "Worker node: <id>
// Original request: <goal> Worker task: <title> ...", sometimes after a line
// saying it was spawned from a /rudder-plan coordinator.
const ORIGINAL_REQUEST = /(?:^|\n)Original request:\s*\n([\s\S]*?)\n\s*\nWorker task:/;
const WORKER_TASK = /(?:^|\n)Worker task:[ \t]*([^\n]+)/;

/** The one-line piece of a plan this worker took. */
export function planPiece(agent: TrackAgent, nodes: Array<{ runId?: string | null; title?: string | null }> = []): string {
  const node = nodes.find((candidate) => candidate?.runId && candidate.runId === agent.id);
  const fromTask = WORKER_TASK.exec(String(agent.task ?? ""))?.[1]?.trim();
  return clip(String(node?.title || fromTask || agent.title || "").replace(/\s+/g, " "), ISSUE_TITLE_LIMIT);
}

/**
 * A plan's goal, from the best source the home publishes: the plan's request
 * in its queue entry, else the orchestrator row's task, else the "Original
 * request" quoted in a worker's prompt, else that worker's own task.
 */
export function planContext(params: {
  planId: string;
  agents: TrackAgent[];
  plans?: Array<{ id?: string | null; request?: string | null; summary?: string | null }> | null;
  nodes?: Array<{ runId?: string | null; title?: string | null }> | null;
}): PlanContext {
  const workers = params.agents.filter((agent) => agent?.planId === params.planId && agent.mode !== "rudder-plan");
  const entry = (params.plans ?? []).find((plan) => plan?.id === params.planId);
  const orchestrator = params.agents.find((agent) => agent?.planId === params.planId && agent.mode === "rudder-plan");
  const quoted = workers.map((agent) => ORIGINAL_REQUEST.exec(String(agent.task ?? ""))?.[1]?.trim() ?? "").find(Boolean);
  const request = [entry?.request, userTask(orchestrator?.task), quoted, userTask(workers[0]?.task)]
    .map((value) => String(value ?? "").trim())
    .find(Boolean) ?? "";
  const summary = String(entry?.summary ?? "").trim();
  const goal = summary && !request.includes(summary) ? `${request}\n\nPlan summary: ${summary}` : request;
  const pieces = [...new Set(workers.map((agent) => planPiece(agent, params.nodes ?? [])).filter(Boolean))].slice(0, PIECE_LIMIT);
  return { goal, pieces };
}

/** Empty or one word ("help", "continue", "thanks"): not work, decided without a model call. */
export function trivialTask(task: unknown): boolean {
  return String(task ?? "").trim().split(/\s+/).filter(Boolean).length < 2;
}

/** Why a run is never tracked, before any model call, or null for a candidate. */
export function untrackedReason(agent: TrackAgent): string | null {
  const id = String(agent.id ?? "");
  const mode = String(agent.mode ?? "");
  if (!id || id.startsWith("__") || mode === "main") {
    return "main";
  }
  if (UNTRACKED_MODES.has(mode)) {
    return "mode";
  }
  if (agent.swarmExperiment === true) {
    return "swarm";
  }
  if (LINEAR_STARTED.test(String(agent.task ?? "")) || LINEAR_STARTED.test(String(agent.title ?? ""))) {
    return "linear";
  }
  return null;
}

export const TRACK_SCHEMA = {
  type: "object",
  properties: {
    action: { type: "string", enum: ["attach", "create", "skip"] },
    issue_identifier: { type: ["string", "null"] },
    title: { type: ["string", "null"] },
    reason: { type: "string" },
  },
  required: ["action", "issue_identifier", "title", "reason"],
  additionalProperties: false,
} as const;

export const TRACK_SYSTEM = [
  "You keep a person's Linear project in step with the coding agents they run in Rudder. Linear is where they see what is being worked on and how far it got. One issue per piece of work, never one per agent: related work shares an issue.",
  "You get one task (an agent's task, a plan's goal, or the newest prompt in a long session), the open issues of the linked Linear project, and the work already tracked in this project. Decide in this order:",
  "1. skip only when the task is not work: a question, an explanation, chat or a request for help, an empty or one-word prompt, or a read-only request such as \"summarize the README\" or \"what does X do\".",
  "2. attach when an open issue covers the task: it continues, fixes, finishes or extends that work, including a follow-up to work already tracked here. Prefer attaching over creating whenever an open issue is related. Set issue_identifier to that issue's identifier exactly as listed.",
  "3. create for any other real work: changing code, config, docs or infrastructure, investigating a bug in order to fix it, running an onboarding or outreach task, and so on. Small fixes count: a small fix gets its own small issue when no open issue covers it. Set title.",
  "A Rudder plan is one goal several agents split into pieces: judge the goal as a whole, never the plan's framing.",
  "title (create only): the outcome in a few words, the way the person would name it in Linear, such as \"Onboard Bild\", \"Move Rudder to bare metal\" or \"Fix login on Safari\". Describe the outcome, never the prompt or what the agent was asked. No trailing period.",
  "reason: one short sentence.",
  "issue_identifier and title are null when they do not apply.",
].join("\n");

const TRACK_WORDING: CallWording = {
  act: "classify the agent",
  declined: "classify the agent.",
  unfinished: "answering",
  noRoom: "answering",
  hint: "",
};

function clip(text: string, limit: number): string {
  const value = text.trim();
  if (value.length <= limit) {
    return value;
  }
  const cut = value.slice(0, limit - 1);
  const space = cut.lastIndexOf(" ");
  return `${(space > limit * 0.6 ? cut.slice(0, space) : cut).trimEnd()}…`;
}

export function trackUserPrompt(params: {
  task: string;
  project: { name: string; repo?: string | null };
  linearName?: string | null;
  issues: LinearIssue[];
  tracked: Array<{ title: string; identifier: string }>;
  plan?: { pieces: string[] } | null;
}): string {
  const issues = params.issues.length
    ? params.issues.map((issue) => `- ${issue.identifier} [${issue.state ?? issue.stateType ?? "open"}] ${clip(issue.title, ISSUE_TITLE_LIMIT)}`).join("\n")
    : "(none)";
  const tracked = params.tracked.length
    ? params.tracked.map((row) => `- ${JSON.stringify(clip(row.title, ISSUE_TITLE_LIMIT))} -> ${row.identifier}`).join("\n")
    : "(none)";
  return [
    `Rudder project: ${params.project.name}${params.project.repo ? ` (${params.project.repo})` : ""}`,
    ...(params.linearName ? [`Linear project: ${params.linearName}`] : []),
    "",
    "Open issues:",
    issues,
    "",
    "Agents already tracked in this project:",
    tracked,
    "",
    ...(params.plan
      ? [
        "This is a Rudder plan: several agents split the goal below into pieces and work on them together.",
        ...(params.plan.pieces.length ? ["Pieces:", ...params.plan.pieces.map((piece) => `- ${piece}`), ""] : []),
        "<plan_goal>",
        clip(params.task, TASK_LIMIT),
        "</plan_goal>",
      ]
      : ["<agent_task>", clip(params.task, TASK_LIMIT), "</agent_task>"]),
  ].join("\n");
}

type RawDecision = { action: "attach" | "create" | "skip"; issueIdentifier: string; title: string; reason: string };

/** Strict: exactly the four schema keys, a known action, and the field that action needs. */
export function parseTrackDecision(text: string): RawDecision {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw new Error("the model did not return JSON");
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("the model did not return a decision");
  }
  const raw = parsed as Record<string, unknown>;
  if (Object.keys(raw).some((key) => !["action", "issue_identifier", "title", "reason"].includes(key))) {
    throw new Error("the decision has unexpected fields");
  }
  const action = raw.action;
  if (action !== "attach" && action !== "create" && action !== "skip") {
    throw new Error("the decision has no valid action");
  }
  const textOf = (value: unknown) => (typeof value === "string" ? value.trim() : "");
  const issueIdentifier = textOf(raw.issue_identifier);
  const title = clip(textOf(raw.title).replace(/^["'“”]+|["'“”]+$/g, "").replace(/\.+$/, ""), TITLE_LIMIT);
  if (action === "attach" && !issueIdentifier) {
    throw new Error("attach without an issue_identifier");
  }
  if (action === "create" && !title) {
    throw new Error("create without a title");
  }
  return { action, issueIdentifier, title, reason: clip(textOf(raw.reason), REASON_LIMIT) };
}

/** One model call for one agent. An attach to an issue that is not open becomes a skip. */
export async function classifyWork(params: {
  env: NodeJS.ProcessEnv;
  fetch: typeof fetch;
  task: string;
  project: { name: string; repo?: string | null };
  linearName?: string | null;
  issues: LinearIssue[];
  tracked: Array<{ title: string; identifier: string }>;
  plan?: { pieces: string[] } | null;
  log?: (message: string) => void;
}): Promise<TrackDecision> {
  const answer = await structuredJson({
    env: params.env,
    fetch: params.fetch,
    log: params.log,
    parse: parseTrackDecision,
    request: {
      system: TRACK_SYSTEM,
      prompt: trackUserPrompt(params),
      schema: TRACK_SCHEMA as unknown as Record<string, unknown>,
      name: "linear_track",
      wording: TRACK_WORDING,
      maxTokens: 1024,
      effort: "low",
      timeoutMs: 30_000,
    },
  });
  const decision = answer.value;
  if (decision.action === "attach") {
    const wanted = decision.issueIdentifier.toLowerCase();
    const issue = params.issues.find((candidate) => candidate.identifier.toLowerCase() === wanted);
    return issue
      ? { action: "attach", issue, reason: decision.reason }
      : { action: "skip", reason: `named ${decision.issueIdentifier}, which is not an open issue here` };
  }
  if (decision.action === "create") {
    return { action: "create", title: decision.title, reason: decision.reason };
  }
  return { action: "skip", reason: decision.reason };
}

const SOURCE_WORDS = { agent: "a Rudder agent", plan: "a Rudder plan", session: "a Rudder session" } as const;

/** A created issue's description: what the work is, and where to watch it. */
export function trackedIssueDescription(task: string, projectName: string, agentUrl: string, source: keyof typeof SOURCE_WORDS = "agent"): string {
  const first = task.trim().split(/\n\s*\n/)[0] ?? "";
  const summary = clip(first.replace(/\s+/g, " "), SUMMARY_LIMIT);
  return [summary, `Tracked from ${SOURCE_WORDS[source]} in ${projectName}.`, agentUrl].filter(Boolean).join("\n\n");
}

/**
 * One line for the merge comment: the first sentence-like line of the agent's
 * summary, else its last message, else its title. Headings ("## Summary") and
 * labels ("What changed:") are passed over.
 */
export function mergeLine(agent: TrackAgent): string {
  for (const source of [agent.summary, agent.lastMessage, agent.title]) {
    let fallback = "";
    for (const line of String(source ?? "").split("\n")) {
      const heading = /^\s*#{1,6}\s/.test(line);
      const plain = line
        .replace(/^[\s#>*+-]+/, "")
        .replace(/\*\*|__|`/g, "")
        .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
        .replace(/\s+/g, " ")
        .trim();
      if (!plain) {
        continue;
      }
      if (heading || plain.endsWith(":")) {
        fallback ||= plain.replace(/:$/, "");
        continue;
      }
      return clip(plain, MERGE_LINE_LIMIT);
    }
    if (fallback) {
      return clip(fallback, MERGE_LINE_LIMIT);
    }
  }
  return "the agent's work";
}
