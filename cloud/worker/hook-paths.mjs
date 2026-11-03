// Hook configs synced from a laptop name the laptop's home: a Claude
// settings.json carries `"command": "/Users/me/.claude/hooks/eta.py pre"`, and
// on the home machine ($HOME=/home/rudder) that path does not exist, so every
// tool call in every pane printed a hook error. The vault records files as
// ~/paths and not the laptop's home, so the prefix is detected: any
// /Users/<name> or /home/<name> that is not this machine's home is rewritten to
// it, and a hook whose command still starts with an absolute path that is not
// on this machine is dropped (it can only ever fail).

import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const FOREIGN_HOME = /(^|[\s"'=:(])(\/(?:Users|home)\/[^/\s"'`$;|&<>()]+)(?=\/|[\s"'`;|&<>()]|$)/g;

// Rewrite every laptop home prefix in `command` to `home`.
export function rewriteHomePrefix(command, home) {
  if (typeof command !== "string" || !home) {
    return command;
  }
  const target = home.replace(/\/+$/, "");
  return command.replace(FOREIGN_HOME, (match, lead, prefix) => (prefix === target ? match : `${lead}${target}`));
}

// The program a command runs, when it is an absolute path; null otherwise
// (`python3 …`, `$HOME/…`, `[ -x … ]`, `cd … &&`, `FOO=1 cmd` are left alone).
export function absoluteProgram(command) {
  if (typeof command !== "string") {
    return null;
  }
  const text = command.trimStart();
  let token;
  if (text.startsWith('"') || text.startsWith("'")) {
    const end = text.indexOf(text[0], 1);
    if (end < 0) {
      return null;
    }
    token = text.slice(1, end);
  } else {
    token = text.split(/[\s;|&<>()]/, 1)[0];
  }
  return token.startsWith("/") ? token : null;
}

// A path under $HOME the command runs or reads that is not on this machine
// (`node "/home/x/.codex/libra-hooks/hook.mjs"`, `FOO=1 "/home/x/notify.sh"`).
// A command that guards the path itself (`[ -f '<path>' ] && …`) is fine.
export function missingHomePath(command, home, exists = fs.existsSync) {
  if (typeof command !== "string" || !home) {
    return null;
  }
  const root = home.replace(/\/+$/, "");
  const paths = command.match(new RegExp(`${root.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}/[^\\s'"\\;|&<>()]+`, "g")) ?? [];
  for (const candidate of paths) {
    const guarded = new RegExp(`-[fxe]\\s+['"]?${candidate.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`).test(command);
    if (!guarded && !exists(candidate)) {
      return candidate;
    }
  }
  return null;
}

// Rewrite and prune one parsed hook config (Claude settings.json or Codex
// hooks.json: `{ hooks: { <Event>: [{ matcher?, hooks: [{ type, command }] }] } }`).
// Returns { config, changed, rewritten, dropped } and never mutates the input.
// `kept` lists every surviving command hook with its new group/handler index
// and the declaration it had before the rewrite (Codex trust carries over).
export function sanitizeHookConfig(config, { home, exists = fs.existsSync } = {}) {
  const result = { config, changed: false, rewritten: [], dropped: [], kept: [] };
  if (!config || typeof config !== "object" || Array.isArray(config)) {
    return result;
  }
  const events = config.hooks;
  if (!events || typeof events !== "object" || Array.isArray(events)) {
    return result;
  }
  const nextEvents = {};
  for (const [event, groups] of Object.entries(events)) {
    if (!Array.isArray(groups)) {
      nextEvents[event] = groups;
      continue;
    }
    const nextGroups = [];
    for (const group of groups) {
      if (!group || typeof group !== "object" || !Array.isArray(group.hooks)) {
        nextGroups.push(group);
        continue;
      }
      const nextHooks = [];
      for (const hook of group.hooks) {
        if (!hook || typeof hook !== "object" || typeof hook.command !== "string") {
          nextHooks.push(hook);
          continue;
        }
        const command = rewriteHomePrefix(hook.command, home);
        if (command !== hook.command) {
          result.rewritten.push({ event, from: hook.command, to: command });
        }
        const program = absoluteProgram(command) ?? missingHomePath(command, home, exists);
        if (program && !exists(program)) {
          result.dropped.push({ event, command, missing: program });
          continue;
        }
        const next = command === hook.command ? hook : { ...hook, command };
        result.kept.push({ event, group: nextGroups.length, handler: nextHooks.length, matcher: group.matcher, from: hook, to: next });
        nextHooks.push(next);
      }
      if (nextHooks.length === 0 && group.hooks.length > 0) {
        continue;
      }
      nextGroups.push({ ...group, hooks: nextHooks });
    }
    if (nextGroups.length === 0 && groups.length > 0) {
      continue;
    }
    nextEvents[event] = nextGroups;
  }
  result.changed = result.rewritten.length > 0 || result.dropped.length > 0;
  if (result.changed) {
    const next = { ...config };
    if (Object.keys(nextEvents).length === 0) {
      delete next.hooks;
    } else {
      next.hooks = nextEvents;
    }
    result.config = next;
  }
  return result;
}

// The hook configs the vault writes into $HOME, fixed in place. Runs after the
// vault files (the hooks directory included) are on disk, so the existence
// check sees the synced scripts. The vault does not keep file modes, so the
// synced hook scripts are made executable first.
export function sanitizeHomeHookConfigs(home, log = () => {}) {
  const hooksDir = path.join(home, ".claude", "hooks");
  markExecutable(hooksDir);
  for (const file of [path.join(home, ".claude", "settings.json"), path.join(home, ".codex", "hooks.json")]) {
    let parsed;
    try {
      parsed = JSON.parse(fs.readFileSync(file, "utf8"));
    } catch {
      continue;
    }
    const outcome = sanitizeHookConfig(parsed, { home });
    if (file.endsWith(path.join(".codex", "hooks.json"))) {
      carryHomeCodexHookTrust(home, file, outcome.kept, log);
    }
    if (!outcome.changed) {
      continue;
    }
    try {
      const tmp = `${file}.rudder-tmp`;
      fs.writeFileSync(tmp, `${JSON.stringify(outcome.config, null, 2)}\n`, { mode: 0o600 });
      fs.renameSync(tmp, file);
    } catch (error) {
      log(`hooks ${file}: ${error?.message || error}`);
      continue;
    }
    const name = file.slice(home.length + 1);
    if (outcome.rewritten.length) {
      log(`hooks ${name}: rewrote ${outcome.rewritten.length} command(s) to ${home}`);
    }
    for (const entry of outcome.dropped) {
      log(`hooks ${name}: dropped ${entry.event} hook \`${entry.command}\` (${entry.missing} is not on this machine)`);
    }
  }
}

// Codex asks "Hooks need review ... Trust all and continue" for any hook whose
// approval it cannot find, and it keys approvals by the hooks.json path, the
// hook's position and a hash of its declaration. All three change when the
// laptop's config lands here (another home, dropped hooks shift positions,
// rewritten commands), so every boot asked again. A hook the user approved on
// the laptop keeps that approval here: the laptop declaration must hash to a
// recorded trusted_hash, and only then is the rewritten declaration recorded
// under its new key. A new or edited hook is never approved by this.
const CODEX_MATCHER_EVENTS = new Set([
  "PreToolUse", "PermissionRequest", "PostToolUse", "SessionStart", "SessionEnd",
  "SubagentStart", "SubagentStop", "PreCompact", "PostCompact",
]);
const CODEX_CONTEXT_LIMIT_EVENTS = new Set(["PreToolUse", "PostToolUse", "SessionStart", "UserPromptSubmit", "SubagentStart"]);
const CODEX_DEFAULT_CONTEXT_LIMIT = 2500;

export function codexEventLabel(event) {
  return event.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase();
}

// Codex's declaration hash (hooks discovery `hook_hash` + config
// `version_for_toml`): sha256 of the key-sorted compact JSON of the
// normalized { event_name, matcher?, hooks: [handler] }. Null for handlers
// Codex would not run as a command hook.
export function codexHookHash(event, matcher, handler) {
  if (!handler || handler.type !== "command" || typeof handler.command !== "string" || !handler.command.trim()) {
    return null;
  }
  const raw = Number.isInteger(handler.timeout) && handler.timeout >= 0 ? handler.timeout : null;
  const timeout = event === "SessionEnd" || event === "Interrupt"
    ? Math.min(Math.max(raw ?? 1, 1), 3)
    : Math.max(raw ?? 600, 1);
  const normalized = { type: "command", command: handler.command, timeout, async: handler.async === true };
  if (typeof handler.statusMessage === "string") {
    normalized.statusMessage = handler.statusMessage;
  }
  const limit = handler.additionalContextLimit;
  if (CODEX_CONTEXT_LIMIT_EVENTS.has(event) && Number.isInteger(limit) && limit !== CODEX_DEFAULT_CONTEXT_LIMIT) {
    normalized.additionalContextLimit = limit;
  }
  const identity = { event_name: codexEventLabel(event), hooks: [normalized] };
  if (CODEX_MATCHER_EVENTS.has(event) && typeof matcher === "string") {
    identity.matcher = matcher;
  }
  return `sha256:${createHash("sha256").update(JSON.stringify(sortKeys(identity))).digest("hex")}`;
}

function sortKeys(value) {
  if (Array.isArray(value)) {
    return value.map(sortKeys);
  }
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.keys(value).sort().map((key) => [key, sortKeys(value[key])]));
  }
  return value;
}

function tomlString(value) {
  return `"${value.replace(/\\/g, "\\\\").replace(/"/g, '\\"').replace(/[\u0000-\u001f\u007f]/g, (ch) => `\\u${ch.charCodeAt(0).toString(16).padStart(4, "0").toUpperCase()}`)}"`;
}

// The edited config.toml text, or null when nothing changes. `hooksPaths` are
// the spellings Codex may key this machine's hooks.json by.
export function carryCodexHookTrust(text, { hooksPaths, kept }) {
  const trusted = new Set();
  const existing = new Set();
  const header = /^\s*\[hooks\.state\.("(?:[^"\\]|\\.)*")\]\s*$/;
  const lines = text.split("\n");
  for (let index = 0; index < lines.length; index += 1) {
    const match = header.exec(lines[index]);
    if (!match) {
      continue;
    }
    let key;
    try {
      key = JSON.parse(match[1]);
    } catch {
      continue;
    }
    existing.add(key);
    for (let next = index + 1; next < lines.length && !/^\s*\[/.test(lines[next]); next += 1) {
      const hash = /^\s*trusted_hash\s*=\s*"(sha256:[0-9a-f]{64})"\s*$/.exec(lines[next]);
      // Only the user's own hooks.json approvals (any home), never a plugin's.
      if (hash && /\/\.codex\/hooks\.json:[a-z_]+:\d+:\d+$/.test(key)) {
        trusted.add(hash[1]);
      }
    }
  }
  let additions = "";
  for (const hook of kept) {
    const before = codexHookHash(hook.event, hook.matcher, hook.from);
    const after = codexHookHash(hook.event, hook.matcher, hook.to);
    if (!before || !after || !trusted.has(before)) {
      continue;
    }
    for (const source of hooksPaths) {
      const key = `${source}:${codexEventLabel(hook.event)}:${hook.group}:${hook.handler}`;
      if (existing.has(key)) {
        continue;
      }
      existing.add(key);
      additions += `\n[hooks.state.${tomlString(key)}]\ntrusted_hash = "${after}"\n`;
    }
  }
  if (!additions) {
    return null;
  }
  return `${text}${text === "" || text.endsWith("\n") ? "" : "\n"}${additions}`;
}

function carryHomeCodexHookTrust(home, hooksFile, kept, log) {
  const config = path.join(home, ".codex", "config.toml");
  let text;
  try {
    text = fs.readFileSync(config, "utf8");
  } catch {
    return;
  }
  const hooksPaths = [hooksFile];
  try {
    const real = fs.realpathSync(hooksFile);
    if (real !== hooksFile) {
      hooksPaths.push(real);
    }
  } catch {
    // keep the plain path
  }
  const next = carryCodexHookTrust(text, { hooksPaths, kept });
  if (next === null) {
    return;
  }
  try {
    const target = fs.realpathSync(config);
    const mode = fs.statSync(target).mode & 0o7777;
    const tmp = `${target}.rudder-tmp`;
    fs.writeFileSync(tmp, next, { mode });
    fs.renameSync(tmp, target);
    const carried = (next.slice(text.length).match(/^\[hooks\.state\./gm) ?? []).length;
    log(`hooks .codex/config.toml: kept the laptop's approval for ${carried} hook entr${carried === 1 ? "y" : "ies"}`);
  } catch (error) {
    log(`hooks .codex/config.toml: ${error?.message || error}`);
  }
}

function markExecutable(dir) {
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      markExecutable(full);
    } else if (entry.isFile()) {
      try {
        fs.chmodSync(full, fs.statSync(full).mode | 0o500);
      } catch {
        // leave it
      }
    }
  }
}
