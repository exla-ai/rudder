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
//   - OpenCode subagents: a session whose parent_id names the agent's session.
//   - Antigravity subagents: brain/<conversation>/.system_generated/subagents/<id>.json,
//     one per invoke_subagent; each is a conversation of its own.
// Each log is followed (transcripts.mjs followFile), so a poll reads only what
// was appended since the last one, and each line is kept by what it was made
// from (trayLine): a log that has not changed is not read at all.
//
// The list has one line per subagent:
//   { id, backend, model, type, kind, name, description, status, startedAt,
//     endedAt, toolCalls, step }
// model is the one its log names last, null before its first reply; type is "subagent" or "teammate"; status is running, done, stopped or failed; step is
// what a running one does now, or what a finished one came back with. `open`
// names one subagent whose activity comes back too, in the shape of the chat's
// cards (cloud/app/subagents.js): the line plus prompt, result, messages and
// earlier.

import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import {
  SUBAGENT_PROMPT_LIMIT,
  antigravityChildReader,
  attachAntigravitySubagents,
  attachClaudeSubagents,
  attachCodexSubagents,
  claudeInterrupted,
  claudeReader,
  claudeSessionHead,
  claudeSubagentMetas,
  claudeSubagentsDir,
  clip,
  codexChildFile,
  codexReader,
  followFile,
  oneLine,
  outsideAgents,
  outsideCodexStatus,
  opencodeSubagentActivity,
  opencodeSubagents,
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
  const live = subagentsLive(record);
  if (record.backend === "opencode") {
    const subagents = opencodeSubagents(home, record.session?.nativeSessionId, live).sort(newestFirst);
    const opened = subagents.find((entry) => entry.id === open);
    return { runId: record.id, subagents, open: opened ? opencodeSubagentActivity(opened, { home, live }) : null };
  }
  const store = trayStore(home);
  const file = transcriptFile(record, home);
  const found = !file
    ? []
    : record.backend === "codex"
      ? codexSubagents(store, home, record.session.nativeSessionId, live)
      : record.backend === "antigravity"
        ? antigravitySubagents(home, record.session.nativeSessionId, live)
        : [...claudeAgentSubagents(home, file, live), ...claudeTeammates(home, file, live)];
  const lines = found.map((item) => trayLine(store, item)).filter(Boolean);
  // Agents of another harness those ran from a shell are listed beside them.
  if (record.backend === "claude") {
    const outside = claudeOutsideAgents(home, lines, live);
    found.push(...outside);
    lines.push(...outside.map((item) => trayLine(store, item)).filter(Boolean));
  }
  const subagents = lines.sort(newestFirst);
  const item = open === null ? null : found.find((entry) => entry.id === open);
  const opened = item ? item.make() : null;
  return { runId: record.id, subagents, open: opened && activity(opened) };
}

// ---------------------------------------------------------------------------
// Tray lines kept by what they are made from. Each subagent is an item
// { id, file, key, make }: `key` holds its log's size and mtime and every
// other fact its line reads (its meta, whether the agent lives, whether it is
// still a team member), and `make()` follows the log into the full entry. A
// line whose key is unchanged is not made again. Following every log from the
// start took 451 ms for libra hi-lsup5 (29 subagents, 270 MB of logs) on a
// home's first poll; a finished subagent's log never changes. The lines, and
// the Codex rollout heads, are kept on disk too, so a restarted home does not
// read them all again, and are dropped when this code or the readers change.
// ---------------------------------------------------------------------------

const LINES_MAX = 4096;
const SAVE_MS = 2_000;
const CODE = createHash("sha256")
  .update(fs.readFileSync(new URL(import.meta.url)))
  .update(fs.readFileSync(new URL("./transcripts.mjs", import.meta.url)))
  .digest("hex")
  .slice(0, 16);
const stores = new Map(); // home -> { file, lines, heads, timer }

function trayStore(home) {
  let store = stores.get(home);
  if (store) {
    return store;
  }
  store = { file: path.join(home, ".rudder", "subagent-tray.json"), lines: new Map(), heads: new Map(), timer: null };
  let saved = null;
  try {
    saved = JSON.parse(fs.readFileSync(store.file, "utf8"));
  } catch (error) {
    // None yet, or one this code cannot read: the lines are made again.
    if (error?.code !== "ENOENT" && !(error instanceof SyntaxError)) {
      throw error;
    }
  }
  if (saved?.code === CODE) {
    for (const [file, key, value] of saved.lines) store.lines.set(file, { key, line: value });
    for (const [file, meta] of saved.heads) store.heads.set(file, meta);
  }
  stores.set(home, store);
  return store;
}

function trayLine(store, item) {
  const hit = store.lines.get(item.file);
  if (hit?.key === item.key) {
    return hit.line;
  }
  const entry = item.make();
  if (!entry) {
    return null; // removed since the listing
  }
  const value = line(entry);
  store.lines.delete(item.file);
  store.lines.set(item.file, { key: item.key, line: value });
  if (store.lines.size > LINES_MAX) {
    store.lines.delete(store.lines.keys().next().value);
  }
  saveSoon(store);
  return value;
}

function saveSoon(store) {
  if (store.timer) {
    return;
  }
  store.timer = setTimeout(() => {
    store.timer = null;
    // Into the home's own ~/.rudder, never made here (a test's home may be
    // gone by the time this runs).
    if (!fs.existsSync(path.dirname(store.file))) {
      return;
    }
    const body = JSON.stringify({
      code: CODE,
      lines: [...store.lines].map(([file, { key, line: value }]) => [file, key, value]),
      heads: [...store.heads],
    });
    const tmp = `${store.file}.${process.pid}.tmp`;
    fs.writeFileSync(tmp, body);
    fs.renameSync(tmp, store.file);
  }, SAVE_MS);
  store.timer.unref?.();
}

function statOrNull(file) {
  try {
    return fs.statSync(file);
  } catch (error) {
    if (error?.code === "ENOENT" || error?.code === "ENOTDIR") {
      return null;
    }
    throw error;
  }
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
  const outside = outsideAgents(entry.reader).map((run) => run.threadId);
  // An interrupted one says what it was doing, not Claude's interrupt note.
  const last = messages[messages.length - (claudeInterrupted(entry.reader) ? 2 : 1)];
  return {
    id: entry.id,
    backend: entry.backend,
    model: entry.reader.model ?? null,
    type: entry.type,
    kind: entry.kind,
    name: entry.name,
    description: entry.description,
    status: entry.status,
    startedAt: entry.startedAt,
    endedAt: entry.endedAt,
    toolCalls: entry.reader.toolCalls,
    step: entry.status !== "running" && entry.result ? oneLine(entry.result) : step(last),
    // Codex threads it ran from its shell (claudeOutsideAgents).
    ...(outside.length ? { outside } : {}),
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

function claudeAgentSubagents(home, session, live) {
  const dir = claudeSubagentsDir(session);
  const list = [];
  for (const [toolUseId, { agentId, meta }] of claudeSubagentMetas(dir)) {
    if (typeof meta.parentAgentId === "string") {
      continue; // shows inside its parent
    }
    const file = path.join(dir, `agent-${agentId}.jsonl`);
    const stat = statOrNull(file);
    if (!stat) {
      continue; // its log is not written yet
    }
    list.push({ id: agentId, file, key: [stat.size, stat.mtimeMs, live, JSON.stringify(meta)].join("|"), make: () => {
      const reader = followFile(file, () => claudeReader({ sub: true, self: toolUseId }));
      if (!reader) {
        return null;
      }
      const status = meta.finished ? "done" : live && !claudeInterrupted(reader) ? "running" : "stopped";
      return {
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
        nest: () => attachClaudeSubagents(reader, { home, dir, projectDir: path.dirname(session), live, shown: null, depth: 1 }),
      };
    } });
  }
  return list;
}

// A Codex thread a Claude subagent or teammate ran from its shell, for a
// review or a second opinion (transcripts.mjs outsideAgents): a line of its
// own, read from the thread's rollout and titled as the one that ran it.
function claudeOutsideAgents(home, lines, live) {
  const list = [];
  for (const parent of lines) {
    for (const id of parent.outside ?? []) {
      const file = codexChildFile(home, id);
      const stat = file ? statOrNull(file) : null;
      if (!stat) {
        continue; // its rollout is not written yet
      }
      list.push({ id, file, key: [stat.size, stat.mtimeMs, live, parent.description].join("|"), make: () => {
        const reader = followFile(file, () => codexReader({ child: true }));
        if (!reader) {
          return null;
        }
        const status = outsideCodexStatus(reader, live);
        return {
          id,
          backend: "codex",
          type: "subagent",
          kind: "Codex",
          name: null,
          description: parent.description,
          status,
          startedAt: uuid7Time(id),
          endedAt: status === "running" ? null : reader.endedAt,
          result: status === "done" ? reader.result : null,
          reader,
          nest: () => attachCodexSubagents(reader, { home, live, shown: null, depth: 1 }),
        };
      } });
    }
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
    const member = members.some((m) => m.name === head.agent && m.prompt === head.prompt);
    const id = path.basename(name, ".jsonl");
    list.push({ id, file, key: [stat.size, stat.mtimeMs, live, member].join("|"), make: () => {
      const reader = followFile(file, () => claudeReader({ sub: true }));
      if (!reader) {
        return null;
      }
      const status = reader.idle || reader.shutDown ? "done" : live && member && !reader.exited && !claudeInterrupted(reader) ? "running" : "stopped";
      return {
        id,
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
        nest: () => attachClaudeSubagents(reader, { home, dir: claudeSubagentsDir(file), projectDir, live, shown: null, depth: 1 }),
      };
    } });
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
// Antigravity
// ---------------------------------------------------------------------------

// agy lists each subagent a conversation started in
// brain/<conversation>/.system_generated/subagents/<id>.json:
//   {"conversationId":"…","subagentDescriptor":{"typeName":"research","role":"Examples Auditor"},
//    "state":"SUBAGENT_STATE_ALIVE","spawnStepIndex":80,"workspaceUris":[…]}
// and keeps the subagent's steps in its own brain folder, like any
// conversation. agy 1.5.14 leaves `state` at SUBAGENT_STATE_ALIVE after the
// subagent finishes, so its own transcript tells: it is done once its last
// step is a reply with no tool calls (its turn ended there; a message from
// its parent starts it again).
function antigravitySubagents(home, conversationId, live) {
  const dir = path.join(home, ".gemini", "antigravity-cli", "brain", conversationId, ".system_generated", "subagents");
  let names;
  try {
    names = fs.readdirSync(dir);
  } catch (error) {
    if (error?.code === "ENOENT") {
      return []; // it started none
    }
    throw error;
  }
  const list = [];
  for (const name of names.filter((entry) => entry.endsWith(".json"))) {
    const listingPath = path.join(dir, name);
    const listingStat = statOrNull(listingPath);
    if (!listingStat) {
      continue;
    }
    let listing;
    try {
      listing = JSON.parse(fs.readFileSync(listingPath, "utf8"));
    } catch (error) {
      if (error instanceof SyntaxError) {
        continue; // agy is still writing it
      }
      throw error;
    }
    const id = text(listing?.conversationId);
    if (!id) {
      continue;
    }
    const descriptor = listing.subagentDescriptor;
    const file = transcriptFile({ backend: "antigravity", session: { nativeSessionId: id } }, home);
    const stat = file ? statOrNull(file) : null;
    if (!stat) {
      // Newly spawned subagent: list it immediately so the tray button left of
      // the message bar appears before agy flushes its first transcript line.
      list.push({
        id,
        file: listingPath,
        key: ["starting", listingStat.size, listingStat.mtimeMs, live, JSON.stringify(descriptor ?? null)].join("|"),
        make: () => ({
          id,
          backend: "antigravity",
          type: "subagent",
          kind: text(descriptor?.typeName),
          name: text(descriptor?.role),
          description: null,
          status: live ? "running" : "stopped",
          startedAt: listingStat.mtimeMs,
          endedAt: live ? null : listingStat.mtimeMs,
          result: null,
          reader: { messages: [], toolCalls: 0, dropped: 0, prompt: null },
          nest: () => {},
        }),
      });
      continue;
    }
    list.push({ id, file, key: [stat.size, stat.mtimeMs, live, JSON.stringify(descriptor ?? null)].join("|"), make: () => {
      const reader = followFile(file, antigravityChildReader);
      if (!reader) {
        return null;
      }
      const status = reader.finished ? "done" : live ? "running" : "stopped";
      const replies = reader.messages.filter((message) => message.role === "assistant");
      return {
        id,
        backend: "antigravity",
        type: "subagent",
        kind: text(descriptor?.typeName),
        name: text(descriptor?.role),
        description: null,
        status,
        startedAt: reader.firstAt ?? listingStat.mtimeMs,
        endedAt: status === "running" ? null : reader.lastAt,
        result: status === "done" ? reader.report ?? replies[replies.length - 1]?.text ?? null : null,
        reader,
        nest: () => attachAntigravitySubagents(reader, { home, conversationId: id, live, shown: null, depth: 1 }),
      };
    } });
  }
  return list;
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

function codexSubagents(store, home, threadId, live) {
  const since = uuid7Time(threadId);
  if (since === null) {
    throw new Error(`not a Codex thread id: ${threadId}`);
  }
  const list = [];
  for (const file of codexRollouts(home, since)) {
    const meta = codexChildMeta(store, file);
    if (meta?.parent !== threadId) {
      continue;
    }
    const stat = statOrNull(file);
    if (!stat) {
      continue; // removed since the listing
    }
    list.push({ id: meta.id, file, key: [stat.size, stat.mtimeMs, live].join("|"), make: () => {
      const reader = followFile(file, () => codexReader({ child: true }));
      if (!reader) {
        return null;
      }
      const status = reader.turn === "done" ? "done" : reader.turn === "aborted" ? "stopped" : live ? "running" : "stopped";
      return {
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
      };
    } });
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
// day after today cover every zone. A day's folder is listed again only when
// it changed: an old thread's tray looked through weeks of them every poll.
const DAY_MS = 86_400_000;
const dayListings = new Map(); // folder -> { mtimeMs, names }
const ROLLOUT = /^rollout-.*-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$/i;

function codexRollouts(home, since) {
  const files = [];
  const end = Date.now() + DAY_MS;
  for (let at = Math.floor(since / DAY_MS - 1) * DAY_MS; at <= end; at += DAY_MS) {
    const day = new Date(at);
    const dir = path.join(home, ".codex", "sessions", String(day.getUTCFullYear()), String(day.getUTCMonth() + 1).padStart(2, "0"), String(day.getUTCDate()).padStart(2, "0"));
    const stat = statOrNull(dir);
    if (!stat) {
      continue; // no rollout that day
    }
    let listing = dayListings.get(dir);
    if (listing?.mtimeMs !== stat.mtimeMs) {
      listing = { mtimeMs: stat.mtimeMs, names: fs.readdirSync(dir) };
      dayListings.set(dir, listing);
    }
    for (const name of listing.names) {
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
// null for a thread nobody spawned. It never changes once written, so it is
// kept with the tray's lines (store.heads).
const HEAD_BYTES = 64 * 1024;

function codexChildMeta(store, file) {
  if (store.heads.has(file)) {
    return store.heads.get(file);
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
  store.heads.set(file, meta);
  saveSoon(store);
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
