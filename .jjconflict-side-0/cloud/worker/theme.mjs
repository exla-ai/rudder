// The attaching laptop terminal's colors (`{type:"theme"}` from `rudder cloud`,
// src/terminal-theme.ts). Every dashboard on this machine re-reads this file
// and answers its agents' OSC 10/11 color queries from it, so opencode, Codex
// and Claude Code come up light in a light terminal and follow a switch.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const COLOR = /^#[0-9a-f]{6}$/;

export function terminalColorsPath() {
  return path.join(process.env.RUDDER_HOME || path.join(os.homedir(), ".rudder"), "terminal-colors.json");
}

/** Save a theme message; true when it was well-formed and the colors changed. */
export function writeTerminalColors(payload, file = terminalColorsPath()) {
  const bg = typeof payload?.bg === "string" ? payload.bg.toLowerCase() : "";
  const fg = typeof payload?.fg === "string" ? payload.fg.toLowerCase() : null;
  if (!COLOR.test(bg) || (fg !== null && !COLOR.test(fg))) {
    return false;
  }
  try {
    const current = JSON.parse(fs.readFileSync(file, "utf8"));
    if (current?.bg === bg && (current?.fg ?? null) === fg) {
      return false;
    }
  } catch {
    // none yet
  }
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, `${JSON.stringify({ fg, bg })}\n`);
  fs.renameSync(tmp, file);
  return true;
}
