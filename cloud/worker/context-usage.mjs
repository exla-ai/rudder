// How full an agent's context is and where its backend compacts it, for the
// chat's context meter (cloud/app/context-meter.js):
//
//   { used, window, autoCompactAt, source, at }
//
//   used           tokens in context after the newest response
//   window         the context window the backend measures against
//   autoCompactAt  context size at which the backend compacts on its own
//   source         where autoCompactAt comes from, or why there is none
//   at             when `used` was measured (ms)
//
// Each is null when it is not known. Nothing here is assumed: a value the
// backend decides somewhere this machine cannot read stays null.
//
// Claude Code (2.1.x). `used` is the newest main-chain response's input +
// cache write + cache read + output tokens, the count Claude Code itself
// compares, or a compaction's postTokens until the next response. The
// model's window is 1M for a `[1m]` model, else the models.dev catalog
// `rudder __models` keeps in ~/.rudder. The auto-compact window is
// CLAUDE_CODE_AUTO_COMPACT_WINDOW, else autoCompactWindow from the settings
// (`/autocompact` writes modelSettings.<model>.autoCompactWindow), capped at
// the model's window; `/context` counts against it, and so does `window`.
// Claude Code compacts at that window less min(max output, 20K) kept for the
// response and 13K for the summary; with CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=p,
// at p% of (window - reserve) when that is lower. With neither set, Claude
// Code takes the window from server-side defaults this machine cannot read:
// `window` is the model's and there is no threshold. It is off with
// autoCompactEnabled false or DISABLE_AUTO_COMPACT / DISABLE_COMPACT set.
// Measured on a home: sessions with autoCompactWindow 400000 compacted at
// 367,149 to 369,616 tokens (threshold 367,000), with 500000 at 467,481 to
// 468,979 (467,000). Claude Code also compacts a chat idle for about an
// hour at whatever size it is.
//
// Codex. `used` and the window are the newest `token_count` event's
// last_token_usage.total_tokens and model_context_window (Codex's own usable
// window). After each response Codex compares that total to its auto-compact
// limit. Rudder launches Codex with model_auto_compact_token_limit=150000
// (src/codex-config.ts), but Codex resolves the limit itself, so the one
// shown is the limit Codex logged for this session: the `post sampling token
// usage` trace in ~/.codex/logs_2.sqlite carries auto_compact_scope_limit.
// Codex keeps about a day of logs, so an older session shows no threshold.

import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";

const CHUNK_BYTES = 256 * 1024;
// Claude Code's reserve: min(max output tokens, OUTPUT_RESERVE) + SUMMARY_RESERVE.
const OUTPUT_RESERVE = 20_000;
const SUMMARY_RESERVE = 13_000;
const CODEX_LOGS = "logs_2.sqlite";
const MEMO_MAX = 200;

/**
 * The context numbers of the run `record` from its session log `file`
 * (cloud/worker/transcripts.mjs transcriptFile). Null until the session
 * reports a usage.
 */
export function contextUsage(record, file, home) {
  if (record.backend === "antigravity") {
    // agy names no context window and compacts on its own terms.
    return null;
  }
  return record.backend === "codex" ? codexUsage(record, file, home) : claudeUsage(record, file, home);
}

// ---------------------------------------------------------------------------
// Claude Code

function claudeUsage(record, file, home) {
  const last = memoByFile(claudeMemo, file, () => claudeLast(file));
  if (!last) {
    return null;
  }
  const entry = claudeCatalog(home)?.get(last.model) ?? null;
  const modelWindow = /\[1m\]$/i.test(String(record.model || "")) ? 1_000_000 : entry?.context ?? null;
  const compact = claudeAutoCompact({ model: last.model, modelWindow, maxOutput: entry?.output ?? null, cwd: record.workspace?.path, home });
  return { used: last.used, window: compact.window, autoCompactAt: compact.autoCompactAt, source: compact.source, at: last.at };
}

// The newest main-chain response, or a compaction after it.
function claudeLast(file) {
  let boundary = null;
  for (const line of linesFromEnd(file)) {
    if (!line.includes('"assistant"') && !line.includes('"compact_boundary"')) {
      continue;
    }
    const entry = JSON.parse(line);
    if (entry.isSidechain) {
      continue;
    }
    if (entry.type === "system" && entry.subtype === "compact_boundary") {
      boundary ??= entry;
      continue;
    }
    const message = entry.type === "assistant" ? entry.message : null;
    // `<synthetic>`: Claude Code's own error and interrupt notes.
    if (!message?.usage || typeof message.model !== "string" || message.model.startsWith("<")) {
      continue;
    }
    if (boundary) {
      const post = Number(boundary.compactMetadata?.postTokens);
      return { model: message.model, used: Number.isInteger(post) && post > 0 ? post : null, at: toMillis(boundary.timestamp) };
    }
    const u = message.usage;
    const used = count(u.input_tokens) + count(u.cache_creation_input_tokens) + count(u.cache_read_input_tokens) + count(u.output_tokens);
    return { model: message.model, used, at: toMillis(entry.timestamp) };
  }
  return null;
}

const SETTINGS_SOURCES = [
  { label: "user settings", file: (home) => path.join(home, ".claude", "settings.json") },
  { label: "project settings", file: (home, cwd) => path.join(cwd, ".claude", "settings.json") },
  { label: "local project settings", file: (home, cwd) => path.join(cwd, ".claude", "settings.local.json") },
];

// The window Claude Code measures against, where it compacts, and why.
function claudeAutoCompact({ model, modelWindow, maxOutput, cwd, home }) {
  const unknown = { window: modelWindow, autoCompactAt: null, source: null };
  if (typeof cwd !== "string" || !cwd) {
    return unknown; // project settings could decide it
  }
  const sources = SETTINGS_SOURCES.map((s) => ({ label: s.label, settings: readJson(s.file(home, cwd)) }));
  // Settings `env` blocks apply over the environment the agent inherits from
  // this home, which the worker shares.
  const env = { ...process.env };
  for (const { settings } of sources) {
    for (const [key, value] of Object.entries(settings?.env ?? {})) {
      env[key] = String(value);
    }
  }
  const off = (why) => ({ window: modelWindow, autoCompactAt: null, source: `Auto-compact is off (${why})` });
  for (const name of ["DISABLE_COMPACT", "DISABLE_AUTO_COMPACT"]) {
    if (truthy(env[name])) {
      return off(name);
    }
  }
  // The newest source that sets it wins; ~/.claude.json is the older home of the setting.
  let enabled = { value: readJson(path.join(home, ".claude.json"))?.autoCompactEnabled, label: "~/.claude.json" };
  for (const { label, settings } of sources) {
    if (settings?.autoCompactEnabled !== undefined) {
      enabled = { value: settings.autoCompactEnabled, label };
    }
  }
  if (enabled.value === false) {
    return off(`autoCompactEnabled in ${enabled.label}`);
  }
  if (!modelWindow) {
    return unknown;
  }

  let configured = null;
  const fromEnv = positiveInt(env.CLAUDE_CODE_AUTO_COMPACT_WINDOW);
  if (fromEnv) {
    configured = { tokens: fromEnv, from: "CLAUDE_CODE_AUTO_COMPACT_WINDOW" };
  } else {
    const key = settingsKey(model);
    // Claude Code's merge: a source's top-level autoCompactWindow replaces
    // every per-model value before it; its per-model values add to them.
    let merged = { fallback: undefined, byModel: {} };
    for (const { label, settings } of sources) {
      const own = {};
      for (const [name, value] of Object.entries(settings?.modelSettings ?? {})) {
        const setting = autoCompactWindow(value?.autoCompactWindow);
        const canonical = settingsKey(name);
        if (setting !== undefined && (name === canonical || !Object.hasOwn(own, canonical))) {
          own[canonical] = { value: setting, label };
        }
      }
      const top = autoCompactWindow(settings?.autoCompactWindow);
      merged = top === undefined
        ? { fallback: merged.fallback, byModel: { ...merged.byModel, ...own } }
        : { fallback: { value: top, label }, byModel: own };
    }
    const chosen = Object.hasOwn(merged.byModel, key) ? merged.byModel[key] : merged.fallback;
    if (chosen && chosen.value !== "auto") {
      configured = { tokens: chosen.value, from: `autoCompactWindow in ${chosen.label}` };
    }
  }
  if (!configured) {
    return unknown; // Claude Code's own default for the model
  }
  const window = Math.min(configured.tokens, modelWindow);
  const outputCap = positiveInt(env.CLAUDE_CODE_MAX_OUTPUT_TOKENS) ?? maxOutput;
  if (!outputCap) {
    return { window, autoCompactAt: null, source: null };
  }
  const reserve = Math.min(outputCap, OUTPUT_RESERVE);
  const effective = window - reserve;
  let at = effective - SUMMARY_RESERVE;
  const pct = Number.parseFloat(env.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE ?? "");
  const scaled = pct > 0 && pct <= 100 ? Math.floor(effective * (pct / 100)) : null;
  if (scaled !== null && scaled < at) {
    at = scaled;
  }
  const size = configured.tokens > modelWindow
    ? `${tokens(configured.tokens)} (${configured.from}), capped at the model's ${tokens(modelWindow)}`
    : `${tokens(window)} (${configured.from}) of the model's ${tokens(modelWindow)}`;
  const how = scaled === at
    ? `Compacts at ${pct}% of it less ${tokens(reserve)} for output (CLAUDE_AUTOCOMPACT_PCT_OVERRIDE).`
    : `Claude Code compacts ${tokens(window - at)} before it.`;
  return { window, autoCompactAt: at, source: `Auto-compact window ${size}. ${how}` };
}

// A valid autoCompactWindow: a token count or "auto". Claude Code ignores
// anything else, as if it were not set.
function autoCompactWindow(value) {
  if (value === "auto" || (Number.isInteger(value) && value > 0)) {
    return value;
  }
  return undefined;
}

// Per-model settings are keyed by the canonical id (no date, no [1m]).
function settingsKey(model) {
  return String(model).toLowerCase().replace(/\[1m\]$/, "").replace(/-\d{8}$/, "");
}

// Context window and max output per Anthropic model, from the models.dev
// catalog `rudder __models` keeps in ~/.rudder. Null without the catalog.
function claudeCatalog(home) {
  return readJson(path.join(home, ".rudder", "models-dev.json"), (data) => {
    const models = new Map();
    for (const [id, entry] of Object.entries(data?.anthropic?.models ?? {})) {
      const context = positiveInt(entry?.limit?.context);
      if (context) {
        models.set(id, { context, output: positiveInt(entry?.limit?.output) });
      }
    }
    return models;
  });
}

// ---------------------------------------------------------------------------
// Codex

function codexUsage(record, file, home) {
  const last = memoByFile(codexMemo, file, () => codexLast(file));
  if (!last) {
    return null;
  }
  const threadId = record.session?.nativeSessionId;
  const limit = typeof threadId === "string" && threadId ? codexLimit(home, threadId, fs.statSync(file).size) : null;
  return {
    used: last.used,
    window: last.window,
    autoCompactAt: limit,
    source: limit === null ? null : "Codex's auto-compact limit for this session, from its log",
    at: last.at,
  };
}

function codexLast(file) {
  for (const line of linesFromEnd(file)) {
    if (!line.includes('"token_count"')) {
      continue;
    }
    const entry = JSON.parse(line);
    const info = entry.type === "event_msg" && entry.payload?.type === "token_count" ? entry.payload.info : null;
    if (info) {
      return { used: positiveInt(info.last_token_usage?.total_tokens), window: positiveInt(info.model_context_window), at: toMillis(entry.timestamp) };
    }
  }
  return null;
}

const codexLimits = new Map(); // threadId -> { size, value }
let sqlite = null;

// The auto-compact limit Codex logged after the session's newest response. A
// new response also grows the rollout, so it is looked up again only then.
function codexLimit(home, threadId, rolloutSize) {
  const hit = codexLimits.get(threadId);
  if (hit?.size === rolloutSize) {
    return hit.value;
  }
  const db = path.join(home, ".codex", CODEX_LOGS);
  let value = null;
  if (fs.existsSync(db)) {
    // Loaded on first use: Node 22 warns once that node:sqlite is experimental.
    sqlite ??= createRequire(import.meta.url)("node:sqlite");
    const conn = new sqlite.DatabaseSync(db, { readOnly: true });
    let row;
    try {
      row = conn
        .prepare("SELECT feedback_log_body AS body FROM logs WHERE thread_id = ? AND feedback_log_body LIKE '%post sampling token usage%' ORDER BY ts DESC, ts_nanos DESC, id DESC LIMIT 1")
        .get(threadId);
    } finally {
      conn.close();
    }
    if (row) {
      const limit = /auto_compact_scope_limit=(?:Some\((\d+)\)|None)/.exec(row.body);
      const scope = /auto_compact_limit_scope=(\w+)/.exec(row.body);
      if (!limit || !scope) {
        throw new Error(`Codex's log no longer reads "auto_compact_scope_limit=... auto_compact_limit_scope=..." (${db})`);
      }
      // "Total" compares the response's total tokens, which is `used`.
      value = limit[1] && scope[1] === "Total" ? Number(limit[1]) : null;
    }
  }
  remember(codexLimits, threadId, { size: rolloutSize, value });
  return value;
}

// ---------------------------------------------------------------------------

/** Complete lines of `file`, newest first. A last line still being written is skipped. */
function* linesFromEnd(file) {
  const fd = fs.openSync(file, "r");
  try {
    let position = fs.fstatSync(fd).size;
    let carry = null; // the start of a line whose beginning is not read yet
    let unfinished = true; // the bytes after the file's last newline
    while (position > 0) {
      const start = Math.max(0, position - CHUNK_BYTES);
      const chunk = Buffer.alloc(position - start);
      fs.readSync(fd, chunk, 0, chunk.length, start);
      position = start;
      const data = carry ? Buffer.concat([chunk, carry]) : chunk;
      let end = data.length;
      while (end > 0) {
        const newline = data.lastIndexOf(10, end - 1);
        if (newline === -1) {
          break;
        }
        if (unfinished) {
          unfinished = false;
        } else if (end > newline + 1) {
          yield data.toString("utf8", newline + 1, end);
        }
        end = newline;
      }
      carry = data.subarray(0, end);
    }
    if (!unfinished && carry?.length) {
      yield carry.toString("utf8");
    }
  } finally {
    fs.closeSync(fd);
  }
}

const claudeMemo = new Map(); // file -> { mtimeMs, size, value }
const codexMemo = new Map();

/** `read()` for `file`, again only once the file changes. */
function memoByFile(memo, file, read) {
  const stat = fs.statSync(file);
  const hit = memo.get(file);
  if (hit && hit.mtimeMs === stat.mtimeMs && hit.size === stat.size) {
    return hit.value;
  }
  const value = read();
  remember(memo, file, { mtimeMs: stat.mtimeMs, size: stat.size, value });
  return value;
}

function remember(memo, key, value) {
  memo.delete(key);
  memo.set(key, value);
  if (memo.size > MEMO_MAX) {
    memo.delete(memo.keys().next().value);
  }
}

const jsonFiles = new Map(); // file -> { mtimeMs, size, value }

/** A parsed JSON file (through `shape`), re-read only when it changes. Null when it does not exist. */
function readJson(file, shape = (data) => data) {
  let stat;
  try {
    stat = fs.statSync(file);
  } catch (error) {
    if (error?.code === "ENOENT") {
      jsonFiles.delete(file);
      return null;
    }
    throw error;
  }
  const hit = jsonFiles.get(file);
  if (hit && hit.mtimeMs === stat.mtimeMs && hit.size === stat.size) {
    return hit.value;
  }
  let value;
  try {
    value = shape(JSON.parse(fs.readFileSync(file, "utf8")));
  } catch (error) {
    throw new Error(`could not read ${file}: ${error instanceof Error ? error.message : String(error)}`);
  }
  jsonFiles.set(file, { mtimeMs: stat.mtimeMs, size: stat.size, value });
  return value;
}

const count = (value) => (Number.isFinite(Number(value)) ? Number(value) : 0);
const truthy = (value) => ["1", "true", "yes", "on"].includes(String(value ?? "").trim().toLowerCase());

function positiveInt(value) {
  const number = typeof value === "string" ? Number(value.trim()) : value;
  return Number.isInteger(number) && number > 0 ? number : null;
}

function tokens(n) {
  return n >= 1_000_000 ? `${Math.round(n / 100_000) / 10}M` : `${Math.round(n / 1000)}K`;
}

function toMillis(value) {
  const parsed = Date.parse(String(value ?? ""));
  return Number.isFinite(parsed) ? parsed : null;
}
