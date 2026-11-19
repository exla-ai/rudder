// Every subagent an agent started, for the chat's subagent tray
// (cloud/app/subagent-tray.js). The chat's own cards (transcripts.mjs) hang off
// the spawn calls in the part of the conversation it shows. The tray lists all
// of them, found from the subagents' own records, so the spawns of a long
// conversation that scrolled out of the chat still show:
//   - Claude Code Agent-tool subagents: <session>/subagents/agent-<id>.meta.json,
//     one per Agent call (subagents of subagents name a parentAgentId and show
//     inside their parent).
//   - Claude Code teammates: a team's config (~/.claude/teams/<team>/config.json)
//     names its lead session, and each teammate is a session of its own in the
//     lead's project folder whose first entry names the team (teamName) and
//     the teammate (agentName). The config lists only the members still alive;
//     the sessions are every teammate the team ever had.
//   - Codex subagents: a rollout whose session_meta names the agent's thread as
//     its parent (source.subagent.thread_spawn.parent_thread_id).
// Each log is followed (transcripts.mjs followFile), so a poll reads only what
// was appended since the last one.
//
// The list has one line per subagent:
//   { id, backend, type, kind, name, description, status, startedAt, endedAt,
//     toolCalls, step }
// type is "subagent" or "teammate"; status is running, done or stopped; step is
// what a running one does now, or what a finished one came back with. `open`
// names one subagent whose activity comes back too, in the shape of the chat's
// cards (cloud/app/subagents.js): the line plus prompt, result, messages and
// earlier.

import fs from "node:fs";
import path from "node:path";
import {
  SUBAGENT_PROMPT_LIMIT,
  attachClaudeSubagents,
  attachCodexSubagents,
  claudeInterrupted,
  claudeReader,
  claudeSessionHead,
  claudeSubagentMetas,
  claudeSubagentsDir,
  clip,
  codexReader,
  followFile,
  oneLine,
  readRunRecord,
  shownMessages,
  subagentsLive,
  transcriptFile,
} from "./transcripts.mjs";

export function listSubagents(repoDir, runId, { home, open = null }) {
  const record = readRunRecord(repoDir, runId);
  if (!record) {
    throw new Error("agent not found");
  }
  const file = transcriptFile(record, home);
  const live = subagentsLive(record);
  const found = !file
    ? []
    : record.backend === "codex"
      ? codexSubagents(home, record.session.nativeSessionId, live)
      : [...claudeAgentSubagents(file, live), ...claudeTeammates(home, file, live)];
  found.sort(newestFirst);
  const opened = open === null ? null : found.find((entry) => entry.id === open) ?? null;
  return { runId: record.id, subagents: found.map(line), open: opened && activity(opened) };
}

// Running ones first, newest first; then the rest by when they ended.
function newestFirst(a, b) {
  const running = Number(b.status === "running") - Number(a.status === "running");
  if (running) {
    return running;
  }
  const at = (entry) => (entry.status === "running" ? entry.startedAt : entry.endedAt ?? entry.startedAt) ?? 0;
  return at(b) - at(a);
}

function line(entry) {
  const messages = entry.reader.messages;
  // An interrupted one says what it was doing, not Claude's interrupt note.
  const last = messages[messages.length - (claudeInterrupted(entry.reader) ? 2 : 1)];
  return {
    id: entry.id,
    backend: entry.backend,
    type: entry.type,
    kind: entry.kind,
    name: entry.name,
    description: entry.description,
    status: entry.status,
    startedAt: entry.startedAt,
    endedAt: entry.endedAt,
    toolCalls: entry.reader.toolCalls,
    step: entry.status !== "running" && entry.result ? oneLine(entry.result) : step(last),
  };
}

function activity(entry) {
  // Subagents this one started show inside it, as in the chat.
  entry.nest();
  const prompt = entry.reader.prompt;
  return {
    ...line(entry),
    prompt: prompt ? clip(prompt, SUBAGENT_PROMPT_LIMIT) : null,
    result: entry.result,
    ...shownMessages(entry.reader),
  };
}

// A message as one line: a tool call's name and input, or what was said.
function step(message) {
  if (!message) {
    return null;
  }
  if (message.role === "tool") {
    return oneLine(`${message.tool?.name || "Tool"} ${message.tool?.input || ""}`);
  }
  return oneLine(message.text) || null;
}

// What a finished Claude subagent or teammate said last.
function claudeResult(reader, status) {
  const last = reader.messages[reader.messages.length - 1];
  return status !== "running" && last?.role === "assistant" ? last.text : null;
}

const text = (value) => (typeof value === "string" && value ? value : null);

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

function claudeAgentSubagents(session, live) {
  const dir = claudeSubagentsDir(session);
  const list = [];
  for (const [toolUseId, { agentId, meta }] of claudeSubagentMetas(dir)) {
    if (typeof meta.parentAgentId === "string") {
      continue; // shows inside its parent
    }
    const reader = followFile(path.join(dir, `agent-${agentId}.jsonl`), () => claudeReader({ sub: true, self: toolUseId }));
    if (!reader) {
      continue; // its log is not written yet
    }
    const status = meta.finished ? "done" : live && !claudeInterrupted(reader) ? "running" : "stopped";
    list.push({
      id: agentId,
      backend: "claude",
      type: "subagent",
      kind: text(meta.agentType),
      name: text(meta.name),
      description: text(meta.description),
      status,
      startedAt: reader.firstAt,
      endedAt: status === "running" ? null : reader.lastAt,
      result: claudeResult(reader, status),
      reader,
      nest: () => attachClaudeSubagents(reader, { dir, projectDir: path.dirname(session), live, shown: null, depth: 1 }),
    });
  }
  return list;
}

// A teammate is done once a turn of its ends (it waits for its next message)
// or it agrees to shut down, as in the chat's cards. Mid-turn, it runs while
// it is still a member of the
// team (its config drops a teammate that shut down; a team reuses names, so
// the task has to match too) and has neither exited nor been interrupted.
function claudeTeammates(home, session, live) {
  const teams = teamsLedBy(home, path.basename(session, ".jsonl"));
  if (!teams.size) {
    return [];
  }
  const projectDir = path.dirname(session);
  const list = [];
  for (const name of fs.readdirSync(projectDir)) {
    if (!name.endsWith(".jsonl")) {
      continue;
    }
    const file = path.join(projectDir, name);
    let stat;
    try {
      stat = fs.statSync(file);
    } catch (error) {
      if (error?.code === "ENOENT") {
        continue; // removed since the listing
      }
      throw error;
    }
    const head = claudeSessionHead(file, stat.size);
    const members = head ? teams.get(head.team) : null;
    if (!members) {
      continue;
    }
    const reader = followFile(file, () => claudeReader({ sub: true }));
    if (!reader) {
      continue; // removed since the listing
    }
    const member = members.some((m) => m.name === head.agent && m.prompt === head.prompt);
    const status = reader.idle || reader.shutDown ? "done" : live && member && !reader.exited && !claudeInterrupted(reader) ? "running" : "stopped";
    list.push({
      id: path.basename(name, ".jsonl"),
      backend: "claude",
      type: "teammate",
      kind: null,
      name: head.agent,
      description: null,
      status,
      startedAt: head.ts,
      endedAt: status === "running" ? null : reader.lastAt,
      result: claudeResult(reader, status),
      reader,
      nest: () => attachClaudeSubagents(reader, { dir: claudeSubagentsDir(file), projectDir, live, shown: null, depth: 1 }),
    });
  }
  return list;
}

// The teams a session leads: team name -> its members ({ name, prompt }).
// Team config -> { team, lead, members, mtimeMs, size }. A team's lead never
// changes, so another session's team is read once; the session's own teams
// are read again when their config changes (members come and go). A config
// not written yet, or caught mid-write, is read on the next poll.
const teamConfigs = new Map();

function teamsLedBy(home, sessionId) {
  const root = path.join(home, ".claude", "teams");
  let names;
  try {
    names = fs.readdirSync(root);
  } catch (error) {
    if (error?.code === "ENOENT") {
      return new Map(); // no agent team on this machine yet
    }
    throw error;
  }
  const teams = new Map();
  for (const name of names) {
    const file = path.join(root, name, "config.json");
    let config = teamConfigs.get(file);
    if (config && config.lead !== sessionId) {
      continue;
    }
    let stat;
    try {
      stat = fs.statSync(file);
    } catch (error) {
      if (error?.code === "ENOENT" || error?.code === "ENOTDIR") {
        continue;
      }
      throw error;
    }
    if (!config || config.mtimeMs !== stat.mtimeMs || config.size !== stat.size) {
      config = readTeamConfig(file, stat);
      if (!config) {
        continue;
      }
      teamConfigs.set(file, config);
    }
    if (config.lead === sessionId && config.team) {
      teams.set(config.team, config.members);
    }
  }
  return teams;
}

function readTeamConfig(file, stat) {
  let config;
  try {
    config = JSON.parse(fs.readFileSync(file, "utf8"));
  } catch (error) {
    if (error instanceof SyntaxError || error?.code === "ENOENT") {
      return null;
    }
    throw error;
  }
  const members = Array.isArray(config?.members) ? config.members : [];
  return {
    team: text(config?.name),
    lead: text(config?.leadSessionId),
    members: members.filter((m) => typeof m?.name === "string").map((m) => ({ name: m.name, prompt: text(m.prompt) })),
    mtimeMs: stat.mtimeMs,
    size: stat.size,
  };
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

function codexSubagents(home, threadId, live) {
  const since = uuid7Time(threadId);
  if (since === null) {
    throw new Error(`not a Codex thread id: ${threadId}`);
  }
  const list = [];
  for (const file of codexRollouts(home, since)) {
    const meta = codexChildMeta(file);
    if (meta?.parent !== threadId) {
      continue;
    }
    const reader = followFile(file, () => codexReader({ child: true }));
    if (!reader) {
      continue; // removed since the listing
    }
    const status = reader.turn === "done" ? "done" : reader.turn === "aborted" ? "stopped" : live ? "running" : "stopped";
    list.push({
      id: meta.id,
      backend: "codex",
      type: "subagent",
      kind: meta.role,
      name: meta.nickname,
      description: meta.path ? path.basename(meta.path).replace(/[_-]+/g, " ").trim() : null,
      status,
      startedAt: uuid7Time(meta.id),
      endedAt: status === "running" ? null : reader.endedAt,
      result: status === "done" ? reader.result : null,
      reader,
      nest: () => attachCodexSubagents(reader, { home, live, shown: null, depth: 1 }),
    });
  }
  return list;
}

// A UUIDv7 starts with its creation time in milliseconds; Codex thread ids
// are UUIDv7.
const UUID7 = /^([0-9a-f]{8})-([0-9a-f]{4})-7[0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

function uuid7Time(id) {
  const match = UUID7.exec(String(id));
  return match ? parseInt(match[1] + match[2], 16) : null;
}

// The rollouts of threads started since `since`. Codex files a rollout under
// the local day it started, so the UTC days from the day before through the
// day after today cover every zone.
const DAY_MS = 86_400_000;
const ROLLOUT = /^rollout-.*-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$/i;

function codexRollouts(home, since) {
  const files = [];
  const end = Date.now() + DAY_MS;
  for (let at = Math.floor(since / DAY_MS - 1) * DAY_MS; at <= end; at += DAY_MS) {
    const day = new Date(at);
    const dir = path.join(home, ".codex", "sessions", String(day.getUTCFullYear()), String(day.getUTCMonth() + 1).padStart(2, "0"), String(day.getUTCDate()).padStart(2, "0"));
    let names;
    try {
      names = fs.readdirSync(dir);
    } catch (error) {
      if (error?.code === "ENOENT") {
        continue; // no rollout that day
      }
      throw error;
    }
    for (const name of names) {
      const match = ROLLOUT.exec(name);
      const started = match ? uuid7Time(match[1]) : null;
      if (started !== null && started >= since) {
        files.push(path.join(dir, name));
      }
    }
  }
  return files;
}

// Rollout -> who spawned it, from its first line (its own session_meta), or
// null for a thread nobody spawned. It never changes once written.
const HEAD_BYTES = 64 * 1024;
const codexHeads = new Map();

function codexChildMeta(file) {
  if (codexHeads.has(file)) {
    return codexHeads.get(file);
  }
  const first = firstLine(file);
  if (first === null) {
    return null; // not written yet
  }
  let entry;
  try {
    entry = JSON.parse(first);
  } catch (error) {
    if (!(error instanceof SyntaxError)) {
      throw error;
    }
    entry = null;
  }
  const payload = entry?.type === "session_meta" ? entry.payload : null;
  const spawn = payload?.source?.subagent?.thread_spawn;
  const meta = spawn && typeof payload.id === "string" && typeof spawn.parent_thread_id === "string"
    ? {
        id: payload.id,
        parent: spawn.parent_thread_id,
        path: text(payload.agent_path),
        nickname: text(payload.agent_nickname),
        role: text(payload.agent_role),
      }
    : null;
  codexHeads.set(file, meta);
  return meta;
}

function firstLine(file) {
  let fd;
  try {
    fd = fs.openSync(file, "r");
  } catch (error) {
    if (error?.code === "ENOENT") {
      return null; // removed since the listing
    }
    throw error;
  }
  try {
    const size = fs.fstatSync(fd).size;
    for (let length = Math.min(size, HEAD_BYTES); length > 0; length = Math.min(size, length * 4)) {
      const buffer = Buffer.alloc(length);
      const read = fs.readSync(fd, buffer, 0, length, 0);
      const end = buffer.subarray(0, read).indexOf(10);
      if (end !== -1) {
        return buffer.toString("utf8", 0, end);
      }
      if (length === size) {
        return null; // the first line is still being written
      }
    }
    return null;
  } finally {
    fs.closeSync(fd);
  }
}
