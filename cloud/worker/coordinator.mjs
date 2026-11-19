// "All agents": one chat in the app that talks to every agent on this home.
//
// It is an ordinary Rudder agent (a Main agent in the hidden project the
// control plane opens on first use, `descriptor.coordinator`), so its chat,
// transcript, model switch, prompts and edits work like any other. What makes
// it a coordinator is the files written here into that project: instructions
// (CLAUDE.md / AGENTS.md) and the `all_agents` MCP server for Claude
// (.mcp.json) and Codex (.codex/config.toml). The server is
// coordinator-mcp.mjs, a stdio proxy to the socket this module serves inside
// the supervisor. Its dashboard runs with RUDDER_ALL_AGENTS=1 (home.mjs), so a
// chat starts with its own short framing (native all_agents_task_prompt)
// instead of a checkout owner's. Every write goes through the supervisor's own RPCs (send,
// keys, task, answer), so each project's dashboard stays the single writer
// of its agents.

import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { readTranscript } from "./transcripts.mjs";

export const COORDINATOR_SERVER = "all_agents";

const LIVE = new Set(["working", "starting"]);
const ID_RE = /^[A-Za-z0-9._-]{1,128}$/;
const WAIT_DEFAULT_S = 300;
const WAIT_MAX_S = 900;
const WAIT_POLL_MS = 1500;
const READ_DEFAULT = 20;
const READ_MAX = 80;
const READ_BUDGET = 14_000;
const LIST_RECENT_MS = 24 * 3600_000;

const agentArg = { type: "string", description: "The agent's id, as list_agents shows it." };
const projectArg = { type: "string", description: "Project name or id. Optional when the agent id is unique." };

export const COORDINATOR_TOOLS = [
  {
    name: "list_agents",
    description: "Every project on this machine and its agents: id, title, status (working, waiting, review, done, failed...), what each needs from the user, backend and model, last activity and last message. Agents idle for a day are counted, not listed, unless include_old is true.",
    inputSchema: {
      type: "object",
      properties: {
        project: { type: "string", description: "Only this project (name or id)." },
        include_old: { type: "boolean", description: "Also list agents idle for more than a day." },
      },
      additionalProperties: false,
    },
  },
  {
    name: "read_agent",
    description: "An agent's task, status, pending prompts and the latest messages of its conversation (user, assistant and one line per tool call).",
    inputSchema: {
      type: "object",
      properties: {
        agent: agentArg,
        project: projectArg,
        messages: { type: "integer", minimum: 1, maximum: READ_MAX, description: `How many recent messages (default ${READ_DEFAULT}).` },
      },
      required: ["agent"],
      additionalProperties: false,
    },
  },
  {
    name: "send_message",
    description: "Type a message into an agent's chat, exactly as if the user sent it from the app. A finished or paused agent resumes with it; a busy one reads it when its current step ends.",
    inputSchema: {
      type: "object",
      properties: { agent: agentArg, project: projectArg, text: { type: "string", description: "The message." } },
      required: ["agent", "text"],
      additionalProperties: false,
    },
  },
  {
    name: "interrupt_agent",
    description: "Stop an agent's current turn (Esc in its terminal). The agent stays open and keeps its conversation.",
    inputSchema: {
      type: "object",
      properties: { agent: agentArg, project: projectArg },
      required: ["agent"],
      additionalProperties: false,
    },
  },
  {
    name: "start_agent",
    description: "Start new work in a project, the same as typing a task in that project in the app. Rudder picks how: usually one new agent, sometimes a plan for the user to approve. Returns the new agent ids it sees.",
    inputSchema: {
      type: "object",
      properties: {
        project: { type: "string", description: "Project name or id." },
        prompt: { type: "string", description: "What the new agent should do. Plain words, not a / command." },
      },
      required: ["project", "prompt"],
      additionalProperties: false,
    },
  },
  {
    name: "answer_prompt",
    description: "Answer an agent's pending prompt (read_agent and list_agents show them with their id). Permission: {\"behavior\":\"allow\"} or {\"behavior\":\"deny\",\"message\":\"why\"}. Question: {\"answers\":{\"<question text>\":\"<option label or your words>\"}}. Plan: {\"approve\":true} or {\"message\":\"what to change\"}.",
    inputSchema: {
      type: "object",
      properties: {
        agent: agentArg,
        project: projectArg,
        prompt_id: { type: "string", description: "The prompt's id." },
        answer: { type: "object", description: "The answer, shaped as described above." },
      },
      required: ["agent", "prompt_id", "answer"],
      additionalProperties: false,
    },
  },
  {
    name: "wait_for_events",
    description: `Wait until an agent finishes, fails, stops, starts or needs an answer, then return what happened. Returns early when the user sends you a message. Default ${WAIT_DEFAULT_S}s, at most ${WAIT_MAX_S}s.`,
    inputSchema: {
      type: "object",
      properties: {
        timeout_seconds: { type: "integer", minimum: 5, maximum: WAIT_MAX_S },
        project: { type: "string", description: "Only events in this project." },
        agents: { type: "array", items: { type: "string" }, description: "Only events from these agent ids." },
      },
      additionalProperties: false,
    },
  },
];

export const COORDINATOR_PROMPT = `# All agents

You are the user's coordinator for every Rudder agent on this machine, across all of their projects. The user talks to you from the "All agents" chat in the Rudder app.

Your tools (the all_agents server):
- list_agents: every project and agent, with status, model and last activity.
- read_agent: an agent's recent conversation and pending prompts.
- send_message: type a message into an agent's chat, as if the user sent it. interrupt_agent stops its current turn.
- start_agent: start new work in a project with a prompt.
- answer_prompt: answer an agent's pending question or permission prompt.
- wait_for_events: wait until agents finish, fail or need an answer.

How to work:
- You coordinate and report. Do not edit files or run commands in the user's projects yourself. Ask the agent that owns the work, or start one.
- Look before you act: list or read first, then send. When you report, name agents by title and project.
- Send, start and interrupt only what the user asked for. Answer an agent's prompt only when the user tells you to or has clearly handed that decision to you.
- When the user asks you to watch, call wait_for_events in a loop and report each event in a sentence or two. Stop when they say so or when nothing is left running.
- Keep replies short: what changed, what needs the user, what you did.
`;

const COORDINATOR_FILES = ["CLAUDE.md", "AGENTS.md", ".mcp.json", ".claude/", ".codex/"];

/**
 * The coordinator's instructions and tool registration, rewritten at every
 * start so a new release's take effect. Kept out of version control.
 */
export function writeCoordinatorFiles(dir, { node, mcpScript, socketPath }) {
  const write = (rel, text) => {
    const file = path.join(dir, rel);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, text);
  };
  const args = [mcpScript, "--socket", socketPath];
  write("CLAUDE.md", COORDINATOR_PROMPT);
  write("AGENTS.md", COORDINATOR_PROMPT);
  write(".mcp.json", `${JSON.stringify({ mcpServers: { [COORDINATOR_SERVER]: { type: "stdio", command: node, args } } }, null, 2)}\n`);
  write(".claude/settings.json", `${JSON.stringify({
    enableAllProjectMcpServers: true,
    enabledMcpjsonServers: [COORDINATOR_SERVER],
    permissions: { allow: [`mcp__${COORDINATOR_SERVER}`] },
    // wait_for_events holds a call for up to WAIT_MAX_S.
    env: { MCP_TOOL_TIMEOUT: String((WAIT_MAX_S + 60) * 1000) },
  }, null, 2)}\n`);
  const toml = (value) => JSON.stringify(value);
  write(".codex/config.toml", [
    `[mcp_servers.${COORDINATOR_SERVER}]`,
    `command = ${toml(node)}`,
    `args = [${args.map(toml).join(", ")}]`,
    // Codex passes MCP servers a short env allowlist: the caller's run id rides here.
    `env_vars = ["RUDDER_SCRATCH"]`,
    `tool_timeout_sec = ${WAIT_MAX_S + 60}`,
    "",
  ].join("\n"));
  const exclude = path.join(dir, ".git", "info", "exclude");
  if (fs.existsSync(path.dirname(exclude))) {
    const current = fs.existsSync(exclude) ? fs.readFileSync(exclude, "utf8") : "";
    const missing = COORDINATOR_FILES.map((rel) => `/${rel}`).filter((line) => !current.split("\n").includes(line));
    if (missing.length) {
      fs.writeFileSync(exclude, `${current}${current && !current.endsWith("\n") ? "\n" : ""}${missing.join("\n")}\n`);
    }
  }
}

// ---------------------------------------------------------------------------
// Tools. `projects()` lists the supervisor's projects ({ workspaceId, name,
// dir, status, error, coordinator, agents() }); `rpc(method, params)` is its
// handleRpc.
// ---------------------------------------------------------------------------

export function createCoordinatorTools({ projects, rpc, home, sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms)), now = Date.now }) {
  const userProjects = () => projects().filter((p) => !p.coordinator);
  // What each chat (by its run id) last saw of each agent: the state just
  // before it sent to the agent, or what its last wait reported. A wait
  // reports what changed since then, so a reply that lands before the wait
  // starts (a one-line reply takes under a second) is still news to it.
  const looked = new Map(); // caller -> Map(`<workspace>/<run>` -> snapshot entry)
  const lookedBy = (caller) => {
    const key = caller ?? "";
    if (!looked.has(key)) looked.set(key, new Map());
    return looked.get(key);
  };
  const remember = (caller, project, agent, change = {}) => {
    lookedBy(caller).set(`${project.workspaceId}/${agent.id}`, { ...snapshotEntry(project, agent), ...change });
  };
  // An agent's newest reply in its transcript ({ text, ts }), or null. The
  // list's preview of it trails the transcript by a few seconds.
  const lastReply = (entry) => {
    const project = userProjects().find((p) => p.workspaceId === entry.workspaceId);
    const { messages = [] } = readTranscript(project.dir, entry.id, { limit: 20, home });
    const last = messages.findLast((m) => m.role === "assistant" && String(m.text || "").trim());
    return last ? { text: last.text, ts: last.ts } : null;
  };

  function findProject(query) {
    const q = String(query || "").trim().toLowerCase();
    const list = userProjects();
    const hit = list.find((p) => p.workspaceId.toLowerCase() === q)
      || list.find((p) => p.name.toLowerCase() === q)
      || list.find((p) => String(p.descriptor?.repo || "").toLowerCase() === q);
    if (!hit) {
      throw new Error(`no project "${query}". Projects: ${list.map((p) => p.name).join(", ") || "none"}`);
    }
    return hit;
  }

  function findAgent(id, projectQuery) {
    const runId = String(id || "").trim();
    if (!ID_RE.test(runId)) {
      throw new Error("agent must be an agent id from list_agents");
    }
    const scope = projectQuery ? [findProject(projectQuery)] : userProjects();
    const hits = [];
    for (const project of scope) {
      const agent = project.status === "cloning" ? null : project.agents().find((a) => a.id === runId);
      if (agent) hits.push({ project, agent });
    }
    if (!hits.length) {
      throw new Error(`no agent ${runId}${projectQuery ? ` in ${scope[0].name}` : ""}. Call list_agents for ids.`);
    }
    if (hits.length > 1) {
      throw new Error(`${runId} is in ${hits.map((h) => h.project.name).join(" and ")}; pass project`);
    }
    return hits[0];
  }

  const delivered = (result, what) => (result?.status === "delivered" ? sentence(what) : `${what} is queued; the dashboard has not confirmed it yet.`);

  const tools = {
    list_agents(args) {
      const scope = args.project ? [findProject(args.project)] : userProjects();
      return formatAgentList(scope, { includeOld: args.include_old === true, now: now() });
    },
    read_agent(args) {
      const { project, agent } = findAgent(args.agent, args.project);
      const limit = clampInt(args.messages, 1, READ_MAX, READ_DEFAULT);
      const transcript = readTranscript(project.dir, agent.id, { limit, home });
      return formatTranscript(project, agent, transcript, { now: now() });
    },
    async send_message(args, { caller }) {
      const { project, agent } = findAgent(args.agent, args.project);
      const text = String(args.text || "").trim();
      if (!text) throw new Error("text is empty");
      remember(caller, project, agent);
      const result = await rpc("send", { workspaceId: project.workspaceId, runId: agent.id, text: text.slice(0, 16_000) });
      return delivered(result, `Sent to ${label(project, agent)}`);
    },
    async interrupt_agent(args, { caller }) {
      const { project, agent } = findAgent(args.agent, args.project);
      remember(caller, project, agent);
      const result = await rpc("keys", { workspaceId: project.workspaceId, runId: agent.id, keys: "esc" });
      return delivered(result, `Interrupted ${label(project, agent)}`);
    },
    async start_agent(args, { caller }) {
      const project = findProject(args.project);
      const prompt = String(args.prompt || "").trim();
      if (!prompt) throw new Error("prompt is empty");
      // A leading / or $ is a dashboard command or a shell, not a task.
      if (/^[/$]/.test(prompt)) throw new Error("start the prompt with words, not / or $");
      const before = new Set(project.agents().map((a) => a.id));
      const result = await rpc("task", { workspaceId: project.workspaceId, text: prompt.slice(0, 16_000) });
      for (let waited = 0; waited < 20_000; waited += 1000) {
        const fresh = project.agents().filter((a) => !before.has(a.id) && !a.pending);
        if (fresh.length) {
          // Still starting as far as this chat knows: a wait reports it finished even if it already has.
          for (const a of fresh) remember(caller, project, a, { status: "starting" });
          return `Started in ${project.name}:\n${fresh.map((a) => `- ${a.id} · ${a.status} · ${clip(a.title, 120)}`).join("\n")}`;
        }
        await sleep(1000);
      }
      return result?.status === "delivered"
        ? `${project.name}'s dashboard took the task but no new agent has appeared yet (it may be planning first). Check list_agents in a moment.`
        : `The task for ${project.name} is queued; its dashboard has not confirmed it yet.`;
    },
    async answer_prompt(args, { caller }) {
      const { project, agent } = findAgent(args.agent, args.project);
      const askId = String(args.prompt_id || "");
      if (!(agent.asks || []).some((q) => q.id === askId)) {
        throw new Error(`${label(project, agent)} has no pending prompt ${askId}`);
      }
      const answer = args.answer && typeof args.answer === "object" && !Array.isArray(args.answer) ? args.answer : null;
      if (!answer) throw new Error("answer must be an object");
      remember(caller, project, agent);
      const result = await rpc("answer", { workspaceId: project.workspaceId, runId: agent.id, askId, answer });
      return result?.status === "queued" ? `Answer for ${label(project, agent)} is queued.` : sentence(`Answered ${label(project, agent)}`);
    },
    async wait_for_events(args, { signal, caller }) {
      const timeoutMs = clampInt(args.timeout_seconds, 5, WAIT_MAX_S, WAIT_DEFAULT_S) * 1000;
      const scope = args.project ? findProject(args.project).workspaceId : null;
      const only = Array.isArray(args.agents) && args.agents.length ? new Set(args.agents.map(String)) : null;
      const inScope = (e) => (!scope || e.workspaceId === scope) && (!only || only.has(e.id));
      const callerInput = () => lastUserInput(projects(), caller);
      const startInput = callerInput();
      // Since this chat last looked; an agent it never looked at, since now.
      const seen = lookedBy(caller);
      for (const [key, entry] of snapshot(userProjects())) {
        if (!seen.has(key)) seen.set(key, entry);
      }
      const deadline = now() + timeoutMs;
      for (;;) {
        if (signal?.aborted) return "Stopped waiting.";
        const after = new Map([...snapshot(userProjects())].filter(([, entry]) => inScope(entry)));
        const events = agentEvents(seen, after, lastReply);
        if (events.length) {
          for (const [key, entry] of after) seen.set(key, entry);
          return formatEvents(events);
        }
        if (startInput !== null && callerInput() !== startInput) {
          return "The user sent you a new message. Stop waiting and read it.";
        }
        if (now() >= deadline) return `Nothing changed in ${Math.round(timeoutMs / 1000)}s.`;
        await sleep(WAIT_POLL_MS);
      }
    },
  };

  return {
    list: () => COORDINATOR_TOOLS,
    async call(name, args = {}, context = {}) {
      const tool = Object.hasOwn(tools, name) ? tools[name] : null;
      if (!tool) throw new Error(`unknown tool ${name}`);
      return await tool(args && typeof args === "object" ? args : {}, context);
    },
  };
}

/** One agent's state for the event diff, keyed `<workspace>/<run>`. */
export function snapshot(projectList) {
  const map = new Map();
  for (const project of projectList) {
    if (project.status === "cloning") continue;
    for (const agent of project.agents()) {
      if (agent.pending) continue;
      map.set(`${project.workspaceId}/${agent.id}`, snapshotEntry(project, agent));
    }
  }
  return map;
}

function snapshotEntry(project, agent) {
  return {
    workspaceId: project.workspaceId,
    project: project.name,
    id: agent.id,
    title: agent.title,
    status: agent.status,
    // When the newest message went into its chat (home.mjs agentSummary).
    inputAt: agent.inputAt ?? null,
    asks: (agent.asks || []).map((q) => ({ id: q.id, text: askLine(q) })),
  };
}

const idle = (status) => !LIVE.has(status) && status !== "waiting";
const endKind = (status) => (status === "failed" ? "failed" : ["done", "review", "merged"].includes(status) ? "finished" : "stopped");

/**
 * What changed between two snapshots that a coordinator reports. `reply(entry)`
 * is the agent's newest reply in its transcript ({ text, ts }) or null.
 */
export function agentEvents(before, after, reply) {
  const events = [];
  for (const [key, now] of after) {
    const was = before.get(key);
    const base = { workspaceId: now.workspaceId, project: now.project, id: now.id, title: now.title, status: now.status };
    if (!was) {
      if (LIVE.has(now.status)) events.push({ ...base, kind: "started" });
    } else if ((LIVE.has(was.status) || was.status === "waiting") && idle(now.status)) {
      events.push({ ...base, kind: endKind(now.status), detail: reply(now)?.text ?? null });
    } else if (idle(now.status) && now.inputAt !== null && now.inputAt !== was.inputAt) {
      // A new message went in and the agent is idle: once it has replied
      // after that message, a whole turn went by between two looks (a
      // one-line reply is over in under a second); before, it has not begun.
      const answer = reply(now);
      if (answer && answer.ts >= now.inputAt) {
        events.push({ ...base, kind: endKind(now.status), detail: answer.text });
      }
    } else if (now.status === "waiting" && was.status !== "waiting" && !now.asks.length) {
      events.push({ ...base, kind: "needs you", detail: "waiting for you" });
    }
    const seen = new Set((was?.asks || []).map((q) => q.id));
    for (const ask of now.asks) {
      if (!seen.has(ask.id)) events.push({ ...base, kind: "needs you", detail: `${ask.text} (prompt ${ask.id})` });
    }
  }
  return events;
}

export function formatEvents(events) {
  return events.map((e) => {
    const head = `${e.kind}: ${e.project} / ${clip(e.title, 100)} (${e.id}) · ${e.status}`;
    return e.detail ? `${head}\n  ${clip(e.detail, 600).replace(/\n+/g, " ")}` : head;
  }).join("\n");
}

export function formatAgentList(projectList, { includeOld = false, now = Date.now() } = {}) {
  const lines = [];
  const quiet = [];
  for (const project of projectList) {
    const agents = project.status === "cloning" ? [] : project.agents();
    const shown = agents.filter((a) => includeOld || LIVE.has(a.status) || a.status === "waiting" || (a.asks || []).length || now - (a.updatedAt ?? 0) < LIST_RECENT_MS);
    const dash = project.status === "running" ? "" : ` · dashboard ${project.status}${project.error ? `: ${project.error}` : ""}`;
    if (!shown.length && !dash) {
      quiet.push(agents.length ? `${project.name} (${agents.length} older)` : project.name);
      continue;
    }
    lines.push(`${project.name} [${project.workspaceId}]${dash}`);
    for (const a of shown) {
      const model = [a.backend, a.model, a.effort].filter(Boolean).join(" ");
      lines.push(`- ${a.id} · ${a.status}${a.phase && a.phase !== a.status ? ` (${clip(a.phase, 40)})` : ""} · ${model} · ${ago(a.updatedAt, now)} · ${clip(a.title, 100)}`);
      for (const q of a.asks || []) lines.push(`  needs you: ${askLine(q)} (prompt ${q.id})`);
      if (a.lastMessage) lines.push(`  last: ${clip(a.lastMessage, 200).replace(/\n+/g, " ")}`);
    }
    const older = agents.length - shown.length;
    if (older > 0) lines.push(`  (${older} older not shown)`);
  }
  if (quiet.length) lines.push(`No recent agents: ${quiet.join(", ")}`);
  return lines.join("\n") || "No projects on this machine yet.";
}

export function formatTranscript(project, agent, transcript, { now = Date.now() } = {}) {
  const head = [
    `${project.name} / ${agent.title} (${agent.id})`,
    `status: ${agent.status}${agent.phase ? ` (${agent.phase})` : ""} · ${[agent.backend, agent.model, agent.effort].filter(Boolean).join(" ")} · updated ${ago(agent.updatedAt, now)}`,
  ];
  if (agent.task && agent.task !== agent.title) head.push(`task: ${clip(agent.task, 600)}`);
  for (const q of agent.asks || []) head.push(`needs you: ${askLine(q)} (prompt ${q.id})`);
  if (agent.summary) head.push(`summary: ${clip(agent.summary, 600)}`);
  const messages = transcript.messages || [];
  const lastAssistant = messages.findLastIndex((m) => m.role === "assistant");
  const body = messages.map((m, i) => messageLine(m, i === lastAssistant ? 4000 : 1500)).filter(Boolean);
  // Oldest messages go first when the whole thing is too long.
  let size = body.reduce((n, line) => n + line.length + 1, 0);
  let dropped = 0;
  while (body.length > 1 && size > READ_BUDGET) {
    size -= body.shift().length + 1;
    dropped += 1;
  }
  if (dropped || transcript.earlier) head.push("(older messages not shown)");
  return [...head, "", ...body].join("\n");
}

function messageLine(m, limit) {
  if (m.role === "tool" && m.tool) {
    const input = typeof m.tool.input === "string" ? m.tool.input : JSON.stringify(m.tool.input ?? "");
    const result = m.tool.output == null ? "running" : m.tool.isError ? `error: ${firstLine(m.tool.output)}` : "ok";
    return `[tool] ${m.tool.name}: ${clip(input, 160).replace(/\n+/g, " ")} -> ${clip(result, 160)}`;
  }
  const text = String(m.text || "").trim();
  // Files the user sent from the app (transcripts.mjs showAttachments).
  const files = m.attachments ? ` (attached: ${m.attachments.join(", ")})` : "";
  if (!text && !files) return null;
  const who = m.role === "user" ? "user" : m.role === "assistant" ? "agent" : m.role;
  return `[${who}] ${clip(text, limit)}${files}`;
}

function askLine(q) {
  const kind = q.kind === "question" ? "question" : q.kind === "plan" ? "plan to approve" : `permission for ${q.tool || "a tool"}`;
  const input = q.input && typeof q.input === "object" ? q.input : {};
  const detail = q.kind === "question"
    ? (Array.isArray(input.questions) ? input.questions.map((x) => `${x?.question}${Array.isArray(x?.options) ? ` [${x.options.map((o) => o?.label).join(" | ")}]` : ""}`).join("; ") : "")
    : q.kind === "plan" ? String(input.plan || "") : String(input.command ?? input.file_path ?? input.description ?? JSON.stringify(input));
  return `${kind}${detail ? `: ${clip(detail, 300).replace(/\n+/g, " ")}` : ""}`;
}

// The caller's own chat: a newer user message there ends a wait.
function lastUserInput(projectList, caller) {
  if (!caller || !ID_RE.test(caller)) return null;
  for (const project of projectList) {
    if (!project.coordinator) continue;
    try {
      const record = JSON.parse(fs.readFileSync(path.join(project.dir, ".rudder", "runs", caller, "run.json"), "utf8"));
      return String(record.lastUserInputAt ?? "");
    } catch {
      // not this project's run
    }
  }
  return null;
}

// A title can end a sentence itself ("Write the README.").
const sentence = (text) => (/[.!?…]$/.test(text) ? text : `${text}.`);

function label(project, agent) {
  return `${project.name} / ${clip(agent.title, 80)}`;
}

function ago(ms, now) {
  if (!Number.isFinite(ms)) return "never";
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86_400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86_400)}d ago`;
}

function clip(text, max) {
  const value = String(text ?? "");
  return value.length > max ? `${value.slice(0, max - 1)}…` : value;
}

function firstLine(text) {
  return String(text || "").split("\n").find((line) => line.trim()) || "";
}

function clampInt(value, min, max, fallback) {
  const n = Number(value);
  return Number.isFinite(n) ? Math.min(max, Math.max(min, Math.round(n))) : fallback;
}

// ---------------------------------------------------------------------------
// The socket coordinator-mcp.mjs calls: one JSON request line per connection,
// one JSON reply line. A connection closed early cancels its call (a wait).
// ---------------------------------------------------------------------------

export function startCoordinatorBroker({ socketPath, tools, log }) {
  fs.rmSync(socketPath, { force: true });
  const server = net.createServer((socket) => {
    const abort = new AbortController();
    let buffer = "";
    let handled = false;
    socket.setEncoding("utf8");
    socket.on("close", () => abort.abort());
    socket.on("error", () => abort.abort());
    socket.on("data", (chunk) => {
      if (handled) return;
      buffer += chunk;
      if (buffer.length > 1_000_000) {
        handled = true;
        socket.destroy();
        return;
      }
      const end = buffer.indexOf("\n");
      if (end < 0) return;
      handled = true;
      const reply = (message) => {
        if (!socket.destroyed) socket.end(`${JSON.stringify(message)}\n`);
      };
      let request;
      try {
        request = JSON.parse(buffer.slice(0, end));
      } catch {
        reply({ ok: false, error: "bad request" });
        return;
      }
      if (request?.op === "tools") {
        reply({ ok: true, tools: tools.list() });
        return;
      }
      if (request?.op !== "call") {
        reply({ ok: false, error: "unknown op" });
        return;
      }
      tools.call(String(request.name || ""), request.arguments ?? {}, { signal: abort.signal, caller: typeof request.caller === "string" ? request.caller : null })
        .then((text) => reply({ ok: true, text: String(text) }))
        .catch((error) => reply({ ok: false, error: error?.message || String(error) }));
    });
  });
  server.on("error", (error) => log(`all agents: ${error.message}`));
  server.listen(socketPath, () => {
    try { fs.chmodSync(socketPath, 0o600); } catch { /* gone */ }
  });
  return server;
}
