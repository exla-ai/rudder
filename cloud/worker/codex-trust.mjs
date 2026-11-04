// Codex stops at "Trust this folder?" for a project with no trust entry, and a
// resumed agent sits at that menu until someone presses Enter. The home's
// ~/.codex/config.toml is the laptop's copy from the vault, rewritten at every
// boot and vault refresh, so trust given on the home was lost each time and
// every restart left each Codex agent waiting. After each write, add entries
// for the home's own repos (the agents there already run with approvals and
// sandbox bypassed, a stronger grant than folder trust).

import fs from "node:fs";
import path from "node:path";

function tomlString(value) {
  return `"${value.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

/** The config with a trusted entry for each dir that has none; unchanged otherwise. */
export function withCodexTrust(config, dirs) {
  let next = config;
  for (const dir of dirs) {
    const header = `[projects.${tomlString(dir)}]`;
    // A table the user already wrote, trusted or not, is theirs.
    if (next.split("\n").some((line) => line.trim() === header)) continue;
    next = `${next}${next === "" || next.endsWith("\n") ? "" : "\n"}\n${header}\ntrust_level = "trusted"\n`;
  }
  return next;
}

/** Repo roots under the code dir, as Codex resolves them (real paths). */
export function homeRepoDirs(codeDir) {
  let entries = [];
  try {
    entries = fs.readdirSync(codeDir, { withFileTypes: true });
  } catch {
    return [];
  }
  const dirs = [];
  for (const entry of entries) {
    if (!entry.isDirectory() || entry.name.startsWith(".")) continue;
    try {
      dirs.push(fs.realpathSync(path.join(codeDir, entry.name)));
    } catch {
      // vanished
    }
  }
  return dirs.sort();
}

export function ensureCodexTrust({ configPath, codeDir, log = () => {} }) {
  const dirs = homeRepoDirs(codeDir);
  if (!dirs.length) return false;
  let current = "";
  try {
    current = fs.readFileSync(configPath, "utf8");
  } catch {
    // no config yet
  }
  const next = withCodexTrust(current, dirs);
  if (next === current) return false;
  try {
    fs.mkdirSync(path.dirname(configPath), { recursive: true, mode: 0o700 });
    const tmp = `${configPath}.${process.pid}.tmp`;
    fs.writeFileSync(tmp, next, { mode: 0o600 });
    fs.renameSync(tmp, configPath);
    return true;
  } catch (error) {
    log(`codex trust: ${error?.message || error}`);
    return false;
  }
}
