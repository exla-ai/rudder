// Hook configs synced from a laptop name the laptop's home: a Claude
// settings.json carries `"command": "/Users/me/.claude/hooks/eta.py pre"`, and
// on the home machine ($HOME=/home/rudder) that path does not exist, so every
// tool call in every pane printed a hook error. The vault records files as
// ~/paths and not the laptop's home, so the prefix is detected: any
// /Users/<name> or /home/<name> that is not this machine's home is rewritten to
// it, and a hook whose command still starts with an absolute path that is not
// on this machine is dropped (it can only ever fail).

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
export function sanitizeHookConfig(config, { home, exists = fs.existsSync } = {}) {
  const result = { config, changed: false, rewritten: [], dropped: [] };
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
        nextHooks.push(command === hook.command ? hook : { ...hook, command });
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
