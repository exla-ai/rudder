// OpenCode's project and MCP commands come from the same /command endpoint
// the TUI uses. TUI actions are registered separately in OpenCode 1.19.5's
// packages/tui/src/app.tsx, routes/session/index.tsx and component/prompt/index.tsx.
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { createInterface } from "node:readline";

export const OPENCODE_VERSION = "1.19.5";

const APP_ACTIONS = [
  ["agents", "Switch agent"],
  ["connect", "Connect provider"],
  ["debug", "View debug info"],
  ["editor", "Open editor"],
  ["exit", "Exit the app", ["quit", "q"]],
  ["help", "Help"],
  ["mcps", "Toggle MCPs"],
  ["models", "Switch model", ["mo"], "model"],
  ["move", "Move session"],
  ["new", "New session", ["clear"]],
  ["sessions", "Switch session", ["resume", "continue"]],
  ["skills", "Skills"],
  ["status", "View status"],
  ["themes", "Switch theme"],
];

const SESSION_ACTIONS = [
  ["compact", "Compact session", ["summarize"], null],
  ["copy", "Copy session transcript"],
  ["export", "Export session transcript"],
  ["fork", "Fork session"],
  ["rename", "Rename session"],
  ["thinking", "Toggle thinking", ["toggle-thinking"]],
  ["timeline", "Jump to message"],
  ["timestamps", "Toggle timestamps", ["toggle-timestamps"]],
  ["undo", "Undo previous message"],
];

export function buildOpencodeCommands(commands, { hasSession }) {
  const results = new Map();
  const reserved = new Set();
  for (const [name, description, aliases = [], opens = "terminal"] of [
    ...APP_ACTIONS,
    ...(hasSession ? SESSION_ACTIONS : []),
  ]) {
    results.set(name, { name, description, aliases, opens, hint: null, args: null });
    for (const alias of aliases) reserved.add(alias);
  }
  for (const command of commands) {
    // Skills have their own picker. The TUI's own commands take precedence.
    if (command.source === "skill" || results.has(command.name) || reserved.has(command.name)) continue;
    const numbered = command.hints.filter((hint) => /^\$\d+$/.test(hint));
    results.set(command.name, {
      name: command.name,
      ...(command.source === "mcp" ? { label: `${command.name}:mcp` } : {}),
      description: (command.description || "").slice(0, 400),
      hint: numbered.length ? numbered.map((hint) => `<arg ${hint.slice(1)}>`).join(" ") : null,
      args: numbered.length ? "required" : null,
      aliases: [],
      opens: null,
      prompt: true,
    });
  }
  return [...results.values()].sort((a, b) => (a.label ?? a.name).localeCompare(b.label ?? b.name));
}

export async function readOpencodeCommands({ bin, cwd, env }) {
  const password = randomBytes(24).toString("base64url");
  const signal = AbortSignal.timeout(10_000);
  const child = spawn(bin, ["serve", "--hostname", "127.0.0.1", "--port", "0", "--no-mdns"], {
    cwd,
    env: { ...env, OPENCODE_SERVER_USERNAME: "opencode", OPENCODE_SERVER_PASSWORD: password },
    detached: true,
    stdio: ["ignore", "pipe", "ignore"],
  });
  const closed = new Promise((resolve) => child.once("close", resolve));
  const lines = createInterface({ input: child.stdout });
  const address = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code) => reject(new Error(`OpenCode's command server exited (${code})`)));
    signal.addEventListener("abort", () => reject(new Error("OpenCode's command server did not start")), { once: true });
    lines.on("line", (line) => {
      const match = /^opencode server listening on (http:\/\/127\.0\.0\.1:\d+)$/.exec(line);
      if (match) resolve(match[1]);
    });
  });
  try {
    const url = new URL("/command", await address);
    url.searchParams.set("directory", cwd);
    const response = await fetch(url, {
      headers: { Authorization: `Basic ${Buffer.from(`opencode:${password}`).toString("base64")}` },
      redirect: "error",
      signal,
    });
    if (!response.ok) throw new Error(`OpenCode could not list commands (HTTP ${response.status})`);
    return await response.json();
  } finally {
    lines.close();
    if (child.pid !== undefined) {
      try {
        process.kill(-child.pid, "SIGKILL");
      } catch (error) {
        if (error.code !== "ESRCH") throw error;
      }
    }
    await closed;
  }
}
