// Rudder tracks your work in Linear: each new agent in a project linked to
// Linear is decided once, by one model call, as attach (it moves an open issue
// forward), create (new work a person would track) or skip (questions, chores,
// small tweaks, follow-ups). integrations.ts persists the decision per run and
// syncs progress quietly; this file holds the prompt, the strict schema, the
// validation and the text that lands in Linear.

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
};

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
  "You keep a person's Linear project in step with the coding agents they start in Rudder. Linear is where they see progress on work they care about, so it holds pieces of work a person would track: a feature, a bug fix, an integration, a customer onboarding, an infrastructure change. One issue per piece of work, never one per agent.",
  "You get one agent's task, the open issues of the linked Linear project, and the agents already tracked in this project. Decide in this order:",
  "1. skip when the task is not progress a person would track: a question or explanation, a review or investigation that changes nothing, tests or chores on their own, a small tweak, a rename, \"summarize X\", a typo fix, or a follow-up to work an already tracked agent did (a fix, a retry or a small change to it).",
  "2. attach when the task moves one of the open issues forward. Prefer attaching over creating whenever an open issue is related. Set issue_identifier to that issue's identifier exactly as listed.",
  "3. create only for new work a person would track that no open issue covers. Set title.",
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
    "<agent_task>",
    clip(params.task, TASK_LIMIT),
    "</agent_task>",
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

/** A created issue's description: what the agent is doing, and where to watch it. */
export function trackedIssueDescription(task: string, projectName: string, agentUrl: string): string {
  const first = task.trim().split(/\n\s*\n/)[0] ?? "";
  const summary = clip(first.replace(/\s+/g, " "), SUMMARY_LIMIT);
  return [summary, `Tracked from a Rudder agent in ${projectName}.`, agentUrl].filter(Boolean).join("\n\n");
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
