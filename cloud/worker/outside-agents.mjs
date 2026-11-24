// Agents of another harness that an agent launched from its shell: `codex
// exec`, `agy -p` or `claude -p` for a review or a second opinion. No harness
// records these as subagents, so they are found where they run: in the process
// tree under the agent's own process, whichever harness the agent is.
//
// watchOutsideAgents reads the process table, notes each such command under a
// live agent (when it started, where, its command line) and finds the session
// its harness wrote for it:
//   Codex        the rollout of a thread started then, in that directory
//   Antigravity  the conversation started then whose prompt is in the command
//   Claude       the session started then in that directory's project folder
// No CLI keeps its session file open, so there is no file descriptor to read
// it from. What was found is kept in ~/.rudder/outside-agents.json, so a
// review that has ended stays in the subagent tray (subagents.mjs).
//
// The Codex plugin is not found this way: its Codex runs under a broker that
// detached from the agent. Its shell output names the thread instead
// (transcripts.mjs outsideAgents).

import fs from "node:fs";
import path from "node:path";

const CLIS = [
  { harness: "codex", comm: "codex", mode: /\s(?:exec|review)(?:\s|$)/ },
  { harness: "antigravity", comm: "agy", mode: /\s(?:-p|--print)(?:\s|$)/ },
  { harness: "claude", comm: "claude", mode: /\s(?:-p|--print)(?:\s|$)/ },
];
// A session starts after its process, within this long. The little before is
// the clock's slack only: a Claude agent's own session, begun 1.8 s before its
// first `claude -p`, was once taken for that command's.
const EARLY_MS = 250;
const LATE_MS = 120_000;
// One process is one entry: the same pid seen again with the same start.
const SAME_START_MS = 5_000;
const KEEP_PER_RUN = 40;
const KEEP_RUNS = 400;
const HEAD_BYTES = 256 * 1024;
const COMMAND_KEEP = 4_000;
const PROMPT_HEAD = 48;

const stores = new Map(); // home -> { file, runs: Map(key -> entries) }

function storeOf(home) {
  let store = stores.get(home);
  if (!store) {
    store = { file: path.join(home, ".rudder", "outside-agents.json"), runs: new Map() };
    let saved = null;
    try {
      saved = JSON.parse(fs.readFileSync(store.file, "utf8"));
    } catch (error) {
      if (error?.code !== "ENOENT" && !(error instanceof SyntaxError)) {
        throw error;
      }
    }
    for (const [key, entries] of saved?.runs ?? []) store.runs.set(key, entries);
    stores.set(home, store);
  }
  return store;
}

function save(store) {
  if (!fs.existsSync(path.dirname(store.file))) {
    return;
  }
  const tmp = `${store.file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({ runs: [...store.runs] }));
  fs.renameSync(tmp, store.file);
}

const runKey = (repoDir, runId) => `${repoDir}|${runId}`;

/** The outside agents found under run `runId`, oldest first: [{ harness, id, file, startedAt, endedAt, command }]. */
export function outsideAgentsOf(home, repoDir, runId) {
  return (storeOf(home).runs.get(runKey(repoDir, runId)) ?? []).filter((entry) => entry.id);
}

/**
 * Note the agent CLIs running under each live agent and find their sessions.
 * `agents`: [{ repoDir, runId, pid }]; `processes`: cleanup.mjs readProcesses.
 */
export function watchOutsideAgents({ home, agents, processes, now = Date.now() }) {
  const store = storeOf(home);
  let changed = false;
  const children = new Map();
  for (const proc of processes.values()) {
    if (!children.has(proc.ppid)) children.set(proc.ppid, []);
    children.get(proc.ppid).push(proc);
  }
  for (const agent of agents) {
    const key = runKey(agent.repoDir, agent.runId);
    const entries = store.runs.get(key) ?? [];
    const running = new Set();
    const stack = [...(children.get(agent.pid) ?? [])];
    while (stack.length) {
      const proc = stack.pop();
      stack.push(...(children.get(proc.pid) ?? []));
      const cli = CLIS.find((c) => proc.comm === c.comm && c.mode.test(proc.cmdline));
      if (!cli) {
        continue;
      }
      const startedAt = Math.round(now - proc.ageMs);
      let entry = entries.find((e) => e.pid === proc.pid && Math.abs(e.startedAt - startedAt) < SAME_START_MS);
      if (!entry) {
        entry = { pid: proc.pid, harness: cli.harness, startedAt, endedAt: null, cwd: proc.cwd, command: proc.cmdline.slice(0, COMMAND_KEEP), id: null, file: null };
        entries.push(entry);
        changed = true;
      }
      running.add(entry);
    }
    for (const entry of entries) {
      if (!entry.id && (running.has(entry) || entry.endedAt === null)) {
        // Once more as it ends: a short run can finish between two looks.
        const found = locate(entry, home);
        if (found && !entries.some((e) => e.id === found.id)) {
          Object.assign(entry, found);
          changed = true;
        }
      }
      if (entry.endedAt === null && !running.has(entry)) {
        entry.endedAt = now;
        changed = true;
      }
    }
    if (entries.length) {
      // A wrapper and what it starts can be the same command twice; an entry
      // that never found a session says nothing.
      const kept = entries.filter((e) => e.id || e.endedAt === null).slice(-KEEP_PER_RUN);
      if (kept.length !== entries.length) changed = true;
      store.runs.delete(key);
      if (kept.length) store.runs.set(key, kept);
    }
  }
  while (store.runs.size > KEEP_RUNS) {
    store.runs.delete(store.runs.keys().next().value);
    changed = true;
  }
  if (changed) {
    save(store);
  }
}

function locate(entry, home) {
  const found = entry.harness === "codex" ? codexSession(entry, home)
    : entry.harness === "antigravity" ? antigravitySession(entry, home)
    : claudeSession(entry, home);
  return found;
}

const inWindow = (at, entry) => at >= entry.startedAt - EARLY_MS && at <= entry.startedAt + LATE_MS;

function names(dir) {
  try {
    return fs.readdirSync(dir);
  } catch (error) {
    if (error?.code === "ENOENT" || error?.code === "ENOTDIR") {
      return [];
    }
    throw error;
  }
}

function head(file) {
  let fd;
  try {
    fd = fs.openSync(file, "r");
  } catch (error) {
    if (error?.code === "ENOENT") {
      return "";
    }
    throw error;
  }
  try {
    const buffer = Buffer.alloc(HEAD_BYTES);
    return buffer.toString("utf8", 0, fs.readSync(fd, buffer, 0, buffer.length, 0));
  } finally {
    fs.closeSync(fd);
  }
}

function firstJson(text, pick) {
  for (const line of text.split("\n")) {
    let value;
    try {
      value = JSON.parse(line);
    } catch {
      continue; // cut by the head, or still being written
    }
    const hit = pick(value);
    if (hit) {
      return hit;
    }
  }
  return null;
}

const closest = (entry, hits) => hits.sort((a, b) => Math.abs(a.at - entry.startedAt) - Math.abs(b.at - entry.startedAt))[0] ?? null;

// Codex files a rollout under the local day its thread started; its id is a
// UUIDv7, which opens with that time, and its session_meta has the directory.
const ROLLOUT = /^rollout-.*-(([0-9a-f]{8})-([0-9a-f]{4})-7[0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$/i;
const DAY_MS = 86_400_000;

function codexSession(entry, home) {
  const hits = [];
  for (let at = entry.startedAt - DAY_MS; at <= entry.startedAt + DAY_MS; at += DAY_MS) {
    const day = new Date(at);
    const dir = path.join(home, ".codex", "sessions", String(day.getUTCFullYear()), String(day.getUTCMonth() + 1).padStart(2, "0"), String(day.getUTCDate()).padStart(2, "0"));
    for (const name of names(dir)) {
      const match = ROLLOUT.exec(name);
      const startedAt = match ? parseInt(match[2] + match[3], 16) : null;
      if (startedAt === null || !inWindow(startedAt, entry)) {
        continue;
      }
      const file = path.join(dir, name);
      const meta = firstJson(head(file), (line) => (line.type === "session_meta" ? line.payload : null));
      // A thread another thread spawned is that one's subagent, not this command's.
      if (meta && meta.cwd === entry.cwd && !meta.source?.subagent) {
        hits.push({ id: match[1], file, at: startedAt });
      }
    }
  }
  const hit = closest(entry, hits);
  return hit && { id: hit.id, file: hit.file };
}

// agy has no working directory in its conversation; the prompt it was given
// on its command line is the conversation's first request.
function antigravitySession(entry, home) {
  const brain = path.join(home, ".gemini", "antigravity-cli", "brain");
  const hits = [];
  for (const id of names(brain)) {
    const file = path.join(brain, id, ".system_generated", "logs", "transcript_full.jsonl");
    let stat;
    try {
      stat = fs.statSync(file);
    } catch (error) {
      if (error?.code === "ENOENT" || error?.code === "ENOTDIR") {
        continue;
      }
      throw error;
    }
    if (stat.mtimeMs < entry.startedAt - EARLY_MS) {
      continue;
    }
    const first = firstJson(head(file), (step) => (step.type === "USER_INPUT" ? step : null));
    const at = Date.parse(first?.created_at);
    // created_at is in whole seconds.
    if (!Number.isFinite(at) || !inWindow(at + 999, entry)) {
      continue;
    }
    const request = /<USER_REQUEST>([\s\S]*?)(?:<\/USER_REQUEST>|$)/.exec(first.content ?? "")?.[1] ?? "";
    const start = request.trim().split("\n")[0].trim().slice(0, PROMPT_HEAD);
    if (start.length >= 8 && entry.command.includes(start)) {
      hits.push({ id, file, at });
    }
  }
  const hit = closest(entry, hits);
  return hit && { id: hit.id, file: hit.file };
}

// Claude Code files a session under its config folder's projects/, in a
// folder named after the working directory. Rudder gives each account a
// config folder of its own.
function claudeSession(entry, home) {
  if (!entry.cwd) {
    return null;
  }
  const slug = entry.cwd.replace(/[^A-Za-z0-9]/g, "-");
  const accounts = path.join(home, ".rudder", "accounts", "claude");
  const roots = [path.join(home, ".claude"), ...names(accounts).map((name) => path.join(accounts, name))];
  const hits = [];
  for (const root of roots) {
    const dir = path.join(root, "projects", slug);
    for (const name of names(dir)) {
      if (!name.endsWith(".jsonl")) {
        continue;
      }
      const file = path.join(dir, name);
      const at = fs.statSync(file).birthtimeMs;
      if (at > 0 && inWindow(at, entry)) {
        hits.push({ id: path.basename(name, ".jsonl"), file, at });
      }
    }
  }
  const hit = closest(entry, hits);
  return hit && { id: hit.id, file: hit.file };
}

/** Every run under `codeDir` that names a process: [{ repoDir, runId, pid }]. */
export function agentProcesses(codeDir) {
  const agents = [];
  for (const project of names(codeDir)) {
    const repoDir = path.join(codeDir, project);
    const runsDir = path.join(repoDir, ".rudder", "runs");
    for (const runId of names(runsDir)) {
      let record;
      try {
        record = JSON.parse(fs.readFileSync(path.join(runsDir, runId, "run.json"), "utf8"));
      } catch (error) {
        if (error?.code === "ENOENT" || error?.code === "ENOTDIR" || error instanceof SyntaxError) {
          continue; // not a run, or its record is being written
        }
        throw error;
      }
      const pid = record?.process?.pid;
      if (record?.id === runId && Number.isInteger(pid) && pid > 1) {
        agents.push({ repoDir, runId, pid });
      }
    }
  }
  return agents;
}
