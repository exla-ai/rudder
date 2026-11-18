// What "/" offers in an agent's own CLI, for the "/" menu in the app's chat
// composer. Each list is built on the home ahead of time, per agent working
// directory, so the menu opens at once.
//
// Claude Code. The menu comes from Claude Code itself: an interactive session
// in the agent's directory, started off-screen in a private tmux with hooks
// off, is sent "/" and its popup is read (readClaudeMenu). That gives the rows
// in Claude's own order (its five most used skills, then built-ins, user,
// project, policy and other commands, each A to Z) with the descriptions it
// shows, and every gate Claude decides at run time: account, plan, org
// policy, feature flags, platform, MCP servers' prompts. Nothing is sent, so
// no model is called and no transcript is written. The popup has no argument
// hints and no aliases: the headless `initialize` answer (no hooks, no MCP)
// and the command objects in the installed binary give those, and the binary
// says which built-ins open a picker or a panel in the TUI.
//
// Codex. Its "/" popup lists built-ins in a fixed order, gated by features,
// the sign-in type, the platform and the install (voice), plus one row per
// service tier of the current model (/fast). It has no custom prompts and no
// skills (skills are `$name` mentions). CODEX_BUILTINS is
// codex-rs/tui/src/slash_command.rs with the gates of
// bottom_pane/slash_commands.rs, command_popup.rs and chatwidget. Those files
// are identical at rust-v0.169.0 and rust-v0.170.0 (diffed), and both
// versions' popups were read in a terminal, so both are in CODEX_VERSIONS; any
// other Codex version is refused rather than guessed.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { createRequire } from "node:module";

const MAX_DESCRIPTION = 400;

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

// In the chat these open its model sheet, the app's control for both.
const CLAUDE_MODEL_COMMANDS = new Set(["model", "effort"]);

/**
 * Command objects in a chunk of Claude Code's bundled JavaScript (read as
 * latin1, one char per byte). Adds each to `found` (name -> variants) and
 * only looks at objects whose `type:` starts in [from, limit).
 */
function collectClaudeCommandObjects(text, found, from, limit) {
  const re = /type:"(local-jsx|local|prompt)"/g;
  re.lastIndex = from;
  let match;
  while ((match = re.exec(text)) && match.index < limit) {
    const object = enclosingObject(text, match.index);
    if (!object) {
      continue;
    }
    const fields = topLevelFields(object);
    const name = literal(fields.get("name"));
    if (!name) {
      continue;
    }
    const type = match[1];
    // Prompt built-ins reach the headless list, except one marked as never
    // running headless: only the TUI offers that one.
    if (type === "prompt" && fields.get("disableNonInteractive") !== "!0") {
      continue;
    }
    const variant = {
      type,
      description: literal(fields.get("description")) ?? getterLiteral(fields.get("get description")),
      argumentHint: literal(fields.get("argumentHint")) ?? getterLiteral(fields.get("get argumentHint")),
      aliases: stringArray(fields.get("aliases")),
      headless: type === "prompt" ? false : fields.get("supportsNonInteractive") === "!0",
      hidden: fields.get("isHidden") === "!0",
    };
    const list = found.get(name) ?? [];
    if (!list.some((seen) => JSON.stringify(seen) === JSON.stringify(variant))) {
      list.push(variant);
    }
    found.set(name, list);
  }
  return found;
}

// The object literal around `at`: back to its unmatched "{", then forward to
// the matching "}" (strings skipped). Null when that fails within a few KB.
function enclosingObject(text, at) {
  let depth = 0;
  let start = -1;
  for (let i = at; i >= 0 && at - i < 4000; i -= 1) {
    const c = text[i];
    if (c === "}") depth += 1;
    else if (c === "{") {
      if (depth === 0) { start = i; break; }
      depth -= 1;
    }
  }
  if (start < 0) {
    return null;
  }
  const end = skipBalanced(text, start, start + 12_000);
  return end > at ? text.slice(start, end + 1) : null;
}

// Index of the bracket closing the one at `open`, or -1.
function skipBalanced(text, open, max) {
  let depth = 0;
  for (let i = open; i < text.length && i < max; i += 1) {
    const c = text[i];
    if (c === '"' || c === "'" || c === "`") {
      i = skipString(text, i);
      if (i < 0) return -1;
      continue;
    }
    if (c === "{" || c === "[" || c === "(") depth += 1;
    else if (c === "}" || c === "]" || c === ")") {
      depth -= 1;
      if (depth === 0) return i;
    }
  }
  return -1;
}

function skipString(text, at) {
  const quote = text[at];
  for (let i = at + 1; i < text.length; i += 1) {
    if (text[i] === "\\") { i += 1; continue; }
    if (text[i] === quote) return i;
    if (quote !== "`" && text[i] === "\n") return -1;
  }
  return -1;
}

// key -> raw value text for the object's own fields. A getter is stored under
// "get <key>" with its body; a method (`isEnabled(){...}`) under its name.
function topLevelFields(object) {
  const fields = new Map();
  let i = 1;
  while (i < object.length - 1) {
    const rest = object.slice(i);
    const head = /^(get |set |async )?([A-Za-z_$][\w$]*)\s*(:|\()/.exec(rest);
    let end;
    if (head) {
      const valueStart = i + head[0].length;
      end = fieldEnd(object, head[3] === "(" ? valueStart - 1 : valueStart);
      const key = head[1] === "get " ? `get ${head[2]}` : head[2];
      fields.set(key, object.slice(head[3] === "(" ? valueStart - 1 : valueStart, end).trim());
    } else {
      end = fieldEnd(object, i);
    }
    i = end + 1;
  }
  return fields;
}

// Index of the comma (or closing brace) ending the field value starting at i.
function fieldEnd(object, i) {
  for (let j = i; j < object.length; j += 1) {
    const c = object[j];
    if (c === '"' || c === "'" || c === "`") {
      const close = skipString(object, j);
      if (close < 0) return object.length - 1;
      j = close;
      continue;
    }
    if (c === "{" || c === "[" || c === "(") {
      const close = skipBalanced(object, j, object.length);
      if (close < 0) return object.length - 1;
      j = close;
      continue;
    }
    if (c === "," || c === "}") return j;
  }
  return object.length - 1;
}

// A JS string literal ("..." or '...', or `...` without ${}) as text, else null.
function literal(raw) {
  if (!raw) return null;
  const quote = raw[0];
  if (!(quote === '"' || quote === "'" || quote === "`") || raw.at(-1) !== quote || raw.length < 2) return null;
  const body = raw.slice(1, -1);
  if (quote === "`" && body.includes("${")) return null;
  // The binary holds UTF-8; the scan read it as latin1.
  return unescapeJs(Buffer.from(body, "latin1").toString("utf8"));
}

const SIMPLE_ESCAPES = { n: "\n", t: "\t", r: "\r", b: "\b", f: "\f", v: "\v", 0: "\0" };
function unescapeJs(body) {
  let out = "";
  for (let i = 0; i < body.length; i += 1) {
    const c = body[i];
    if (c !== "\\") { out += c; continue; }
    const next = body[i + 1];
    i += 1;
    if (next in SIMPLE_ESCAPES) out += SIMPLE_ESCAPES[next];
    else if (next === "x") { out += String.fromCharCode(parseInt(body.slice(i + 1, i + 3), 16)); i += 2; }
    else if (next === "u" && body[i + 1] === "{") {
      const close = body.indexOf("}", i);
      out += String.fromCodePoint(parseInt(body.slice(i + 2, close), 16));
      i = close;
    } else if (next === "u") { out += String.fromCharCode(parseInt(body.slice(i + 1, i + 5), 16)); i += 4; }
    else if (next === "\n") { /* line continuation */ }
    else out += next;
  }
  return out;
}

// `(){return"text"}` -> text, else null.
function getterLiteral(raw) {
  const m = raw && /^\(\)\{return\s*(["'`][\s\S]*["'`])\}$/.exec(raw);
  return m ? literal(m[1]) : null;
}

function stringArray(raw) {
  if (!raw || raw[0] !== "[") return [];
  return [...raw.matchAll(/"([^"\\]*)"/g)].map((m) => m[1]);
}

/**
 * Command objects of the Claude Code binary at `file`, read in chunks. Each
 * read reaches `overlap` bytes into both neighbours, so an object cut by a
 * chunk edge is whole in the chunk its `type:` falls in.
 */
export async function readClaudeBinaryCommands(file) {
  const handle = await fs.promises.open(file, "r");
  try {
    const { size } = await handle.stat();
    const chunk = 2 << 20;
    const overlap = 64 << 10;
    const buffer = Buffer.alloc(overlap + chunk + overlap);
    const found = new Map();
    for (let pos = 0; pos < size; pos += chunk) {
      const start = Math.max(0, pos - overlap);
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, start);
      const text = buffer.toString("latin1", 0, bytesRead);
      collectClaudeCommandObjects(text, found, pos - start, Math.min(pos - start + chunk, bytesRead));
    }
    if (!found.has("resume") || !found.has("help") || !found.has("model")) {
      throw new Error(`could not read the built-in commands of ${file}`);
    }
    return found;
  } finally {
    await handle.close();
  }
}

/** The headless `initialize` answer of Claude Code in `cwd`: { version, commands }. */
export function claudeInitialize({ bin, cwd, env, timeoutMs = 30_000 }) {
  const args = [
    "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
    // Nothing starts: no hooks, no MCP servers (their prompts are not listed).
    "--settings", JSON.stringify({ disableAllHooks: true }),
    "--strict-mcp-config", "--mcp-config", JSON.stringify({ mcpServers: {} }),
  ];
  const request = { type: "control_request", request_id: "rudder-slash", request: { subtype: "initialize" } };
  return new Promise((resolve, reject) => {
    const child = spawn(bin, args, { cwd, env, stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    let settled = false;
    const finish = (error, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      child.stdin.end();
      if (error) {
        child.kill("SIGKILL");
        reject(error);
      } else {
        resolve(value);
      }
    };
    const timer = setTimeout(() => finish(new Error(`claude did not answer initialize within ${timeoutMs / 1000}s`)), timeoutMs);
    child.on("error", (error) => finish(new Error(`could not start ${bin}: ${error.message}`)));
    child.stderr.on("data", (data) => { stderr += data; });
    child.stdout.on("data", (data) => {
      stdout += data;
      let newline;
      while ((newline = stdout.indexOf("\n")) >= 0) {
        const line = stdout.slice(0, newline).trim();
        stdout = stdout.slice(newline + 1);
        if (!line.startsWith("{")) continue;
        let message;
        try {
          message = JSON.parse(line);
        } catch {
          continue;
        }
        if (message.type !== "control_response" || message.response?.request_id !== request.request_id) continue;
        if (message.response.subtype !== "success") {
          finish(new Error(`claude initialize failed: ${message.response.error || "no reason given"}`));
          return;
        }
        const answer = message.response.response;
        if (!Array.isArray(answer?.commands) || typeof answer.claude_code_version !== "string") {
          finish(new Error("claude initialize answered without its commands"));
          return;
        }
        finish(null, { version: answer.claude_code_version, commands: answer.commands });
        return;
      }
    });
    child.on("close", (code) => finish(new Error(`claude exited (${code}) before answering initialize${stderr.trim() ? `: ${stderr.trim().split("\n")[0]}` : ""}`)));
    child.stdin.write(`${JSON.stringify(request)}\n`);
  });
}

const clip = (text) => {
  const value = String(text || "").replace(/\s+/g, " ").trim();
  return value.length > MAX_DESCRIPTION ? `${value.slice(0, MAX_DESCRIPTION - 1)}…` : value;
};

// The off-screen terminal: wide enough that no description is cut, tall
// enough that the whole popup shows at once.
const MENU_COLUMNS = 1000;
const MENU_ROWS = 1000;
// MCP servers connect after the prompt shows; their prompts join the popup
// as they do. The popup is read once it has not changed for MENU_STILL_MS,
// no sooner than MENU_SETTLE_MS after "/".
const MENU_SETTLE_MS = 3000;
const MENU_STILL_MS = 2000;
let menuSeq = 0;

/** The popup rows on a Claude Code screen: [{ display, description }], or null when none show. */
export function parseClaudeMenu(screen) {
  const lines = screen.split("\n").map((line) => line.trimEnd());
  // The prompt line reads "❯ /" (a no-break space follows the arrow).
  if (!lines.some((line) => /^❯\s\/$/.test(line))) return null;
  if (lines.some((line) => /^\s+[↑↓] \d+ more/.test(line))) {
    throw new Error("Claude Code's menu did not fit the off-screen terminal");
  }
  // The popup is one block of rows, above or below the prompt as room allows.
  // A long description wraps onto lines indented to its column.
  let rows = [];
  let run = [];
  for (const line of [...lines, ""]) {
    const m = /^ {2}[❯ ]\s\/(.+)$/.exec(line);
    if (m) {
      // The description column starts after a run of spaces; a name has none.
      const gap = m[1].search(/ {2,}/);
      run.push(gap < 0 ? { display: m[1], description: "" } : { display: m[1].slice(0, gap), description: m[1].slice(gap).trim() });
      continue;
    }
    if (run.length && /^ {6,}\S/.test(line)) {
      run.at(-1).description += ` ${line.trim()}`;
      continue;
    }
    if (run.length > rows.length) rows = run;
    run = [];
  }
  return rows.length ? rows : null;
}

/**
 * Claude Code's "/" popup in `cwd`: an interactive session started in a
 * private tmux server with the agent's environment and model and hooks off,
 * sent "/". Returns [{ display, description }] in Claude's order.
 */
export async function readClaudeMenu({ bin, cwd, env, model = null, timeoutMs = 45_000 }) {
  const tmux = which("tmux", env);
  // tmux leaves its socket behind when the server is killed.
  const socket = path.join(os.tmpdir(), `rudder-slash-${process.pid}-${menuSeq++}`);
  const t = (...args) => run(tmux, ["-S", socket, "-f", "/dev/null", ...args], { cwd, env });
  const screen = async () => (await t("capture-pane", "-p", "-t", "menu")).stdout;
  const deadline = Date.now() + timeoutMs;
  const pause = () => new Promise((resolve) => setTimeout(resolve, 250));
  try {
    // remain-on-exit keeps a Claude that quit on screen, so its error is read.
    await t("set-option", "-g", "remain-on-exit", "on", ";",
      "new-session", "-d", "-s", "menu", "-x", String(MENU_COLUMNS), "-y", String(MENU_ROWS), "-c", cwd,
      // Rudder starts every Claude agent with the inline renderer (launch.rs).
      "env", "-u", "TMUX", "-u", "TMUX_PANE", "TERM=xterm-256color", "CLAUDE_CODE_NO_FLICKER=0",
      bin, "--settings", JSON.stringify({ disableAllHooks: true }), ...(model ? ["--model", model] : []));
    let text = "";
    for (;;) {
      text = await screen();
      if (/^❯\s/m.test(text)) break;
      if (/Is this a project you created or one you trust/.test(text)) {
        throw new Error(`Claude Code has not been trusted in ${cwd}`);
      }
      const dead = (await t("display-message", "-p", "-t", "menu", "#{pane_dead}")).stdout.trim() === "1";
      if (dead || Date.now() > deadline) {
        const said = text.split("\n").map((line) => line.trim()).filter(Boolean).slice(0, 3).join(" ");
        throw new Error(`Claude Code ${dead ? "exited" : "showed no prompt"} before its menu could be read${said ? `: ${said}` : ""}`);
      }
      await pause();
    }
    await t("send-keys", "-t", "menu", "-l", "/");
    const sent = Date.now();
    let last = null;
    let since = 0;
    for (;;) {
      await pause();
      const rows = parseClaudeMenu(await screen());
      const sig = JSON.stringify(rows);
      if (sig !== last) {
        last = sig;
        since = Date.now();
      } else if (rows && Date.now() - since >= MENU_STILL_MS && Date.now() - sent >= MENU_SETTLE_MS) {
        return rows;
      }
      if (Date.now() > deadline) {
        throw new Error(rows ? "Claude Code's menu kept changing" : "Claude Code showed no menu for /");
      }
    }
  } finally {
    await t("kill-server").catch(() => {});
    fs.rmSync(socket, { force: true });
  }
}

// What Claude inserts for an MCP prompt, shown as "server:prompt (MCP)".
function claudeCommandName(display) {
  const mcp = /^(.+?):(.+) \(MCP\)$/.exec(display);
  if (!mcp) return display;
  const part = (text) => text.replace(/[^a-zA-Z0-9_-]/g, "_");
  return `mcp__${part(mcp[1])}__${part(mcp[2])}`;
}

/**
 * The app's rows for Claude Code's popup (`menu`), with argument hints and
 * aliases from its headless list (`headless`) and the command objects of its
 * binary (`binary`, name -> variants).
 */
export function buildClaudeCommands({ menu, headless, binary }) {
  const byName = new Map(headless.map((command) => [command.name, command]));
  return menu.map(({ display, description }) => {
    const name = claudeCommandName(display);
    const twin = byName.get(display);
    // A built-in, unless a user, project or plugin command has its name.
    const variants = !twin || twin.builtin ? binary.get(name) : undefined;
    // What the TUI runs: its own definition when there is one.
    const tui = variants && (variants.find((v) => v.type === "local-jsx") ?? variants.find((v) => v.type === "local" && !v.headless)
      ?? variants.find((v) => v.type === "prompt") ?? variants.find((v) => v.type === "local"));
    const interactive = !!tui && (tui.type === "local-jsx" || (tui.type === "local" && !tui.headless));
    const opens = tui && CLAUDE_MODEL_COMMANDS.has(name) ? "model" : interactive ? "terminal" : null;
    // A prompt that names its arguments says so last: "(arguments: who, when)".
    const argNames = /\(arguments: ([^)]*)\)$/.exec(description)?.[1].split(", ");
    return {
      name,
      // The row's own text where it differs from what is typed (MCP prompts).
      ...(display !== name ? { label: display } : {}),
      description: clip(description),
      hint: tui?.argumentHint || twin?.argumentHint || argNames?.map((arg) => `[${arg}]`).join(" ") || null,
      // Picked with Enter, Claude runs a command at once, except a prompt
      // that names its arguments: that one waits for them.
      args: argNames ? "required" : null,
      opens,
      ...(opens === "terminal" ? { when: "always" } : {}),
      aliases: [...new Set([...(twin?.aliases ?? []), ...(variants ?? []).flatMap((v) => v.aliases)])],
    };
  });
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

// The Codex the worker image installs (cloud/worker/Dockerfile pins it).
export const CODEX_VERSION = "0.170.0";
// Every Codex release whose "/" popup source matches CODEX_BUILTINS. Add a
// release only after diffing the three files named at the top of this file.
export const CODEX_VERSIONS = Object.freeze(["0.169.0", CODEX_VERSION]);

// Popup order. args: takes inline arguments. opens: "model" (the chat's model
// sheet) or "terminal" (a picker, panel or TUI-only output), when "bare" (only
// without arguments) or "always". gate: see codexGate.
const CODEX_BUILTINS = [
  ["model", "choose what model and reasoning effort to use", { args: true, opens: "model" }],
  ["presets", "run or save reusable prompts and slash commands", { opens: "terminal" }],
  ["daybreak", null, { opens: "terminal", gate: "daybreak" }],
  ["ide", "include current selection, open files, and other context from your IDE", { args: true, opens: "terminal", when: "always" }],
  ["permissions", "choose what Codex is allowed to do", { args: true, opens: "terminal", when: "always" }],
  ["keymap", "remap TUI shortcuts", { args: true, opens: "terminal", when: "always" }],
  ["vim", "toggle Vim mode for the composer", { opens: "terminal" }],
  ["setup-default-sandbox", "set up elevated agent sandbox", { opens: "terminal", gate: "windows" }],
  ["experimental", "toggle experimental features", { opens: "terminal" }],
  ["approve", "approve one retry of a recent auto-review denial", {}],
  ["memories", "configure memory use and generation", { opens: "terminal" }],
  ["skills", "use skills to improve how Codex performs specific tasks", { opens: "terminal" }],
  ["import", "import setup, this project, and recent chats from Claude Code", { opens: "terminal" }],
  ["hooks", "view and manage lifecycle hooks", { opens: "terminal" }],
  ["review", "review my current changes and find issues", { args: true, opens: "terminal" }],
  ["rename", "rename the current thread", { args: true, opens: "terminal" }],
  ["new", "start a new chat during a conversation", { args: true }],
  ["archive", "archive this session", { opens: "terminal" }],
  ["delete", "permanently delete this session", { opens: "terminal" }],
  ["resume", "resume a saved chat", { args: true, opens: "terminal", when: "always" }],
  ["fork", "fork the current chat", { args: true, opens: "terminal", when: "always" }],
  ["worktree", "start or continue a conversation in a new worktree", { opens: "terminal" }],
  ["app", "continue this session in the Desktop app", { opens: "terminal", gate: "desktop" }],
  ["init", "create an AGENTS.md file with instructions for Codex", {}],
  ["compact", "summarize conversation to prevent hitting the context limit", {}],
  ["config", "configure Codex preferences", { opens: "terminal" }],
  ["recap", "summarize the current conversation now", {}],
  ["plan", "switch to Plan mode", { args: true }],
  ["voice", "start or stop voice; use /voice settings to choose a voice", { args: true, gate: "voice" }],
  ["goal", "set or view the goal for a long-running task", { args: true, opens: "terminal", gate: "goals" }],
  ["agents", "open the agent command center", { opens: "terminal" }],
  ["side", "start a side conversation", { args: true, opens: "terminal", when: "always" }],
  ["btw", "start a side conversation", { args: true, opens: "terminal", when: "always", alias: true }],
  ["copy", "copy the last response or part of it", { opens: "terminal" }],
  ["export", "export the conversation as markdown", { args: true, opens: "terminal", when: "always" }],
  ["raw", "toggle raw scrollback mode for copy-friendly terminal selection", { args: true, opens: "terminal", when: "always" }],
  ["tui", "choose the TUI mode for the next launch", { opens: "terminal" }],
  ["diff", "show git diff (including untracked files)", { opens: "terminal" }],
  ["mention", "mention a file", { opens: "terminal" }],
  ["status", "show current session configuration and token usage", { opens: "terminal" }],
  ["daemon", "Manage the local background server", { opens: "terminal" }],
  ["warnings", "view retained warnings and diagnostic details", { opens: "terminal" }],
  ["cd", "change the current working directory", { args: true, opens: "terminal", when: "always" }],
  ["pwd", "show the current working directory", { args: true, opens: "terminal", when: "always" }],
  ["usage", "view account usage or use a usage limit reset", { args: true, opens: "terminal", when: "always", gate: "chatgpt" }],
  ["debug-config", "show config layers and requirement sources for debugging", { gate: "never" }],
  ["title", "configure which items appear in the terminal title", { args: true, opens: "terminal", when: "always" }],
  ["statusline", "configure which items appear in the status line", { args: true, opens: "terminal", when: "always" }],
  ["theme", "choose a syntax highlighting theme", { opens: "terminal" }],
  ["pets", "choose or hide the terminal pet", { args: true, opens: "terminal", when: "always" }],
  ["mcp", "list MCP tools; use /mcp verbose or /mcp login <name>", { args: true, opens: "terminal", when: "always" }],
  ["apps", "manage apps", { gate: "never" }],
  ["plugins", "browse plugins", { opens: "terminal", gate: "plugins" }],
  ["logout", "log out of Codex", { opens: "terminal" }],
  ["quit", "exit Codex", { alias: true }],
  ["exit", "exit Codex", {}],
  ["feedback", "send logs to maintainers or save a ZIP", { opens: "terminal" }],
  ["rollout", "print the rollout file path", { gate: "never" }],
  ["ps", "list background terminals", { opens: "terminal" }],
  ["stop", "stop all background terminals", { opens: "terminal" }],
  ["clear", "clear the terminal and start a new chat", { args: true }],
  ["test-approval", "test approval request", { gate: "never" }],
  ["subagents", "switch between this session's subagents", { opens: "terminal" }],
  ["debug-m-drop", "DO NOT USE", { gate: "never" }],
  ["debug-m-update", "DO NOT USE", { gate: "never" }],
];

// Agent modes Rudder starts with plugins off (native launch.rs
// push_codex_planner_config_overrides); every other mode has them on.
const CODEX_PLANNER_MODES = new Set(["plan", "rudder-plan", "gam-adversarial"]);
export const codexPluginsFor = (mode) => !CODEX_PLANNER_MODES.has(mode);

const isDaybreak = (program) => String(program).startsWith("daybreak_");
// chatwidget/settings.rs daybreak_command_description for a session that
// has not turned Daybreak on; null hides the command. An API-key sign-in
// needs one more feature (daybreak_account_eligible).
function daybreakDescription({ features, chatgpt, apiKey, models }) {
  if (!features.get("cli_daybreak")) return null;
  if (!chatgpt && !(apiKey && features.get("api_key_cyber_access_programs"))) return null;
  if (models.some((m) => (m.available_access_programs?.cyber ?? []).some(isDaybreak))) return "Enable broader access for cybersecurity work";
  if (!models.length) return "Manage broader access for cybersecurity work";
  if (models.every((m) => (m.available_access_programs?.cyber ?? []).length > 0)) return "Learn about broader access for cybersecurity work";
  return "Manage broader access for cybersecurity work";
}

function codexGate(gate, state) {
  switch (gate) {
    case undefined: return true;
    case "never": return false;
    case "windows": return state.platform === "win32";
    case "desktop": return state.platform === "darwin" || state.platform === "win32";
    case "goals": return state.features.get("goals") === true;
    case "plugins": return state.features.get("plugins") === true;
    case "chatgpt": return state.chatgpt;
    case "daybreak": return daybreakDescription(state) !== null;
    case "voice": return state.voice;
    default: throw new Error(`unknown Codex gate ${gate}`);
  }
}

// codex-cli/bin/codex.js: the platform package whose binary the npm launcher runs.
const CODEX_NPM_TARGETS = {
  "linux-x64": ["@openai/codex-linux-x64", "x86_64-unknown-linux-musl"],
  "linux-arm64": ["@openai/codex-linux-arm64", "aarch64-unknown-linux-musl"],
  "darwin-x64": ["@openai/codex-darwin-x64", "x86_64-apple-darwin"],
  "darwin-arm64": ["@openai/codex-darwin-arm64", "aarch64-apple-darwin"],
  "win32-x64": ["@openai/codex-win32-x64", "x86_64-pc-windows-msvc"],
  "win32-arm64": ["@openai/codex-win32-arm64", "aarch64-pc-windows-msvc"],
};

/** The package folder (codex-package.json) of the Codex that `bin` runs. */
export function codexPackageDir(bin) {
  const real = fs.realpathSync(bin);
  // A native install runs <package>/bin/codex itself (install-context from_exe).
  const native = path.dirname(path.dirname(real));
  if (path.basename(path.dirname(real)) === "bin" && fs.existsSync(path.join(native, "codex-package.json"))) {
    return native;
  }
  const target = CODEX_NPM_TARGETS[`${process.platform}-${process.arch}`];
  if (!target) {
    throw new Error(`Codex has no package for ${process.platform} ${process.arch}`);
  }
  const [pkg, triple] = target;
  // The launcher looks in its platform package, then in its own vendor folder.
  let vendor;
  try {
    vendor = path.join(path.dirname(createRequire(real).resolve(`${pkg}/package.json`)), "vendor");
  } catch (error) {
    if (error.code !== "MODULE_NOT_FOUND") throw error;
    vendor = path.join(path.dirname(real), "..", "vendor");
  }
  const dir = path.join(vendor, triple);
  if (!fs.existsSync(path.join(dir, "codex-package.json"))) {
    throw new Error(`${real} runs no Codex package Rudder can find (looked in ${dir})`);
  }
  return dir;
}

// realtime-webrtc session.rs is_supported: /voice shows when the package
// carries the voice runtime (macOS, Linux and Windows builds that ship it).
export function codexHasVoice(packageDir, platform = process.platform) {
  const voice = path.join(packageDir, "codex-resources", "voice");
  const [helper, runtime] = platform === "darwin" ? ["bin/codex-voice-host", "lib/libgstreamer-1.0.0.dylib"]
    : platform === "win32" ? ["bin/codex-voice-host.exe", "bin/gstreamer-1.0-0.dll"]
      : ["bin/codex-voice-host", "lib/libgstreamer-1.0.so.0"];
  const file = (rel) => fs.statSync(path.join(voice, rel), { throwIfNoEntry: false })?.isFile() === true;
  return file(helper) && file(runtime);
}

/**
 * Codex's "/" popup for one agent. `features`: name -> enabled, as
 * `codex features list` prints them with Rudder's launch flags; `chatgpt` and
 * `apiKey`: the sign-in; `voice`: the install has the voice runtime;
 * `models`: the model catalog; `model`: the agent's model.
 */
export function buildCodexCommands({ version, features, chatgpt, apiKey, voice, models, model, platform = process.platform }) {
  if (!CODEX_VERSIONS.includes(version)) {
    throw new Error(`Rudder knows the commands of Codex ${CODEX_VERSIONS.join(" and ")}; this machine has Codex ${version}. Install ${CODEX_VERSION} with: npm install -g @openai/codex@${CODEX_VERSION}`);
  }
  const state = { features, chatgpt, apiKey, voice, models, platform };
  const commands = [];
  for (const [name, description, spec] of CODEX_BUILTINS) {
    if (!codexGate(spec.gate, state)) continue;
    commands.push({
      name,
      description: name === "daybreak" ? daybreakDescription(state) : description,
      hint: null,
      args: spec.args ? "optional" : null,
      opens: spec.opens ?? null,
      ...(spec.opens === "terminal" ? { when: spec.when ?? "bare" } : {}),
      aliases: [],
      ...(spec.alias ? { alias: true } : {}),
    });
    if (name === "model") {
      const preset = models.find((m) => m.slug === model);
      for (const tier of preset?.service_tiers ?? []) {
        const enabled = tier.id === "flex" ? true : tier.id === "ultrafast" ? features.get("ultrafast_mode") === true : features.get("fast_mode") === true;
        if (!enabled) continue;
        commands.push({ name: String(tier.name).toLowerCase(), description: clip(tier.description), hint: null, args: null, opens: "terminal", when: "always", aliases: [] });
      }
    }
  }
  return commands;
}

/** `codex features list` output -> Map(name -> enabled). */
export function parseCodexFeatures(text) {
  const features = new Map();
  for (const line of text.split("\n")) {
    const m = /^([a-z0-9_]+)\s+.+\s(true|false)\s*$/.exec(line.trim());
    if (m) features.set(m[1], m[2] === "true");
  }
  if (!features.size) {
    throw new Error("codex features list printed no features");
  }
  return features;
}

// The `model` key at the top of a Codex config.toml (before any table).
export function codexConfigModel(text) {
  for (const line of text.split("\n")) {
    if (/^\s*\[/.test(line)) return null;
    const m = /^\s*model\s*=\s*"([^"]+)"/.exec(line);
    if (m) return m[1];
  }
  return null;
}

function run(bin, args, { cwd, env, timeoutMs = 20_000 }) {
  return new Promise((resolve, reject) => {
    const child = spawn(bin, args, { cwd, env, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => { child.kill("SIGKILL"); reject(new Error(`${path.basename(bin)} ${args.join(" ")} timed out`)); }, timeoutMs);
    child.stdout.on("data", (d) => { stdout += d; });
    child.stderr.on("data", (d) => { stderr += d; });
    child.on("error", (error) => { clearTimeout(timer); reject(new Error(`could not start ${bin}: ${error.message}`)); });
    child.on("close", (code) => {
      clearTimeout(timer);
      if (code !== 0) reject(new Error(`${path.basename(bin)} ${args.join(" ")} exited ${code}: ${(stderr || stdout).trim().split("\n")[0]}`));
      else resolve({ stdout, stderr });
    });
  });
}

/** What Codex's popup depends on, read from the installed CLI in `cwd`. */
export async function readCodexState({ bin, cwd, env, plugins }) {
  const [versionOut, featuresOut, loginOut] = await Promise.all([
    run(bin, ["--version"], { cwd, env }),
    // Rudder starts every Codex agent with these (native launch.rs).
    run(bin, ["--enable", "goals", "-c", `features.plugins=${plugins}`, "features", "list"], { cwd, env }),
    run(bin, ["login", "status"], { cwd, env }).catch((error) => {
      // `login status` exits 1 when signed out; that is an answer.
      if (/Not logged in/i.test(error.message)) return { stdout: "Not logged in", stderr: "" };
      throw error;
    }),
  ]);
  const version = /codex-cli\s+(\S+)/.exec(versionOut.stdout)?.[1];
  if (!version) {
    throw new Error(`could not read the Codex version from "${versionOut.stdout.trim()}"`);
  }
  const login = `${loginOut.stdout}\n${loginOut.stderr}`;
  return {
    version,
    features: parseCodexFeatures(featuresOut.stdout),
    chatgpt: /using ChatGPT/i.test(login),
    apiKey: /^API key configured/m.test(login),
    voice: codexHasVoice(codexPackageDir(bin)),
  };
}

// ---------------------------------------------------------------------------
// The home's lists: built ahead, rebuilt when an input changes.
// ---------------------------------------------------------------------------

const CHECK_MS = 60_000; // how often known lists check their inputs
const FULL_MS = 30 * 60_000; // rebuilt anyway (claude.ai skills, feature flags)
const WARM_AGE_MS = 24 * 3600_000; // agents active this recently get a list at start

function which(name, env) {
  if (name.includes("/")) return name;
  for (const dir of String(env.PATH || "").split(path.delimiter)) {
    if (!dir) continue;
    const file = path.join(dir, name);
    try {
      fs.accessSync(file, fs.constants.X_OK);
      return file;
    } catch {
      // not in this directory
    }
  }
  throw new Error(`${name} is not on PATH`);
}

// mtime and size of each file under `dir` (a few levels), as one string.
function treeStamp(dir, depth = 4, out = []) {
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch (error) {
    if (error.code === "ENOENT" || error.code === "ENOTDIR") return out;
    throw error;
  }
  for (const entry of entries) {
    const file = path.join(dir, entry.name);
    if (entry.isDirectory() || entry.isSymbolicLink()) {
      out.push(fileStamp(file));
      if (depth > 0) treeStamp(file, depth - 1, out);
    } else if (entry.isFile()) {
      out.push(fileStamp(file));
    }
    if (out.length > 4000) break;
  }
  return out;
}

function fileStamp(file) {
  try {
    const stat = fs.statSync(file);
    return `${file}:${stat.mtimeMs}:${stat.size}`;
  } catch (error) {
    if (error.code === "ENOENT") return `${file}:-`;
    throw error;
  }
}

function claudeStamp({ bin, cwd, env }) {
  const config = env.CLAUDE_CONFIG_DIR?.trim() || path.join(env.HOME || os.homedir(), ".claude");
  const parts = [fileStamp(fs.realpathSync(bin)), fs.realpathSync(bin)];
  for (const name of ["settings.json", "settings.local.json", "plugins/installed_plugins.json", "plugins/known_marketplaces.json"]) {
    parts.push(fileStamp(path.join(config, name)));
  }
  treeStamp(path.join(config, "commands"), 4, parts);
  treeStamp(path.join(config, "skills"), 2, parts);
  // Project commands, skills and settings, here and in each parent.
  for (let dir = path.resolve(cwd); ; dir = path.dirname(dir)) {
    const claude = path.join(dir, ".claude");
    parts.push(fileStamp(claude));
    if (fs.existsSync(claude)) {
      parts.push(fileStamp(path.join(claude, "settings.json")), fileStamp(path.join(claude, "settings.local.json")));
      treeStamp(path.join(claude, "commands"), 4, parts);
      treeStamp(path.join(claude, "skills"), 2, parts);
    }
    parts.push(fileStamp(path.join(dir, ".mcp.json")));
    if (path.dirname(dir) === dir) break;
  }
  parts.push(claudeGlobalStamp(env, path.resolve(cwd)));
  return parts.join("\n");
}

// The parts of Claude's global config the menu shows: the most used skills
// lead it, and the account and MCP servers gate rows. The file changes all
// the time for other reasons, so only these parts count.
const globalConfigCache = new Map(); // file -> { mtimeMs, config }
function claudeGlobalStamp(env, cwd) {
  const file = env.CLAUDE_CONFIG_DIR?.trim()
    ? path.join(env.CLAUDE_CONFIG_DIR.trim(), ".claude.json")
    : path.join(env.HOME || os.homedir(), ".claude.json");
  let stat;
  try {
    stat = fs.statSync(file);
  } catch (error) {
    if (error.code === "ENOENT") return `${file}:-`;
    throw error;
  }
  let cached = globalConfigCache.get(file);
  if (cached?.mtimeMs !== stat.mtimeMs) {
    cached = { mtimeMs: stat.mtimeMs, config: JSON.parse(fs.readFileSync(file, "utf8")) };
    globalConfigCache.set(file, cached);
  }
  const { skillUsage, oauthAccount, mcpServers, projects } = cached.config;
  const project = projects?.[cwd] ?? {};
  return JSON.stringify([skillUsage, oauthAccount, mcpServers, project.mcpServers, project.enabledMcpjsonServers, project.disabledMcpjsonServers, project.disabledMcpServers]);
}

function codexStamp({ bin, cwd, env }) {
  const home = env.CODEX_HOME?.trim() || path.join(env.HOME || os.homedir(), ".codex");
  const real = fs.realpathSync(bin);
  return [real, fileStamp(real), ...["config.toml", "auth.json"].map((name) => fileStamp(path.join(home, name))),
    fileStamp(path.join(cwd, ".codex", "config.toml"))].join("\n");
}

function readCodexModels(env) {
  const home = env.CODEX_HOME?.trim() || path.join(env.HOME || os.homedir(), ".codex");
  const file = path.join(home, "models_cache.json");
  let body;
  try {
    body = fs.readFileSync(file, "utf8");
  } catch (error) {
    if (error.code === "ENOENT") return { models: [], configModel: readConfigModel(home) };
    throw error;
  }
  const models = JSON.parse(body).models;
  if (!Array.isArray(models)) {
    throw new Error(`${file} has no models list`);
  }
  return { models, configModel: readConfigModel(home) };
}

function readConfigModel(home) {
  try {
    return codexConfigModel(fs.readFileSync(path.join(home, "config.toml"), "utf8"));
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}

function readRun(repoDir, runId) {
  if (!/^[A-Za-z0-9._-]+$/.test(runId) || runId.startsWith(".")) {
    throw new Error("unknown agent");
  }
  let record;
  try {
    record = JSON.parse(fs.readFileSync(path.join(repoDir, ".rudder", "runs", runId, "run.json"), "utf8"));
  } catch (error) {
    if (error.code === "ENOENT") throw new Error("that agent is not on the home machine");
    throw error;
  }
  return record;
}

/**
 * The home's "/" lists. `projectDirs()` names the open projects; their recent
 * agents get a list at start. `forRun(repoDir, runId)` answers the app.
 */
export function createSlashCommands({ env, log, projectDirs }) {
  const lists = new Map(); // key -> { stamp, value, error, builtAt, checkedAt, building }
  const binaryCache = new Map(); // claude realpath -> Promise<Map>
  let queue = Promise.resolve();
  // One build at a time: a start with many agents does not stack processes.
  const serial = (fn) => {
    const next = queue.then(fn, fn);
    queue = next.catch(() => {});
    return next;
  };

  const claudeBinary = (bin) => {
    const real = fs.realpathSync(bin);
    if (!binaryCache.has(real)) {
      const read = readClaudeBinaryCommands(real);
      read.catch(() => binaryCache.delete(real));
      binaryCache.set(real, read);
    }
    return binaryCache.get(real);
  };

  const sources = {
    claude: {
      // The menu names the agent's model ("Switch model (currently …)").
      key: ({ cwd, model }) => `claude\n${cwd}\n${model ?? ""}`,
      stamp: ({ cwd }) => claudeStamp({ bin: which(env.RUDDER_CLAUDE_BIN?.trim() || "claude", env), cwd, env }),
      build: async ({ cwd, model }) => {
        const bin = which(env.RUDDER_CLAUDE_BIN?.trim() || "claude", env);
        const [menu, answer, binary] = await Promise.all([
          readClaudeMenu({ bin, cwd, env, model }), claudeInitialize({ bin, cwd, env }), claudeBinary(bin),
        ]);
        return { version: answer.version, commands: buildClaudeCommands({ menu, headless: answer.commands, binary }) };
      },
    },
    codex: {
      key: ({ cwd, plugins }) => `codex\n${cwd}\n${plugins}`,
      stamp: ({ cwd }) => codexStamp({ bin: which(env.RUDDER_CODEX_BIN?.trim() || "codex", env), cwd, env }),
      build: async ({ cwd, plugins }) => {
        const bin = which(env.RUDDER_CODEX_BIN?.trim() || "codex", env);
        return readCodexState({ bin, cwd, env, plugins });
      },
    },
  };

  async function ensure(backend, input, { wait }) {
    const source = sources[backend];
    const key = source.key(input);
    let entry = lists.get(key);
    const now = Date.now();
    if (entry?.building) {
      if (wait && !entry.value) await entry.building;
      return entry;
    }
    // A failed build is not served for long: a request tries again.
    if (entry && (entry.error ? !wait && now - entry.checkedAt < CHECK_MS : now - entry.checkedAt < CHECK_MS)) {
      return entry;
    }
    const stamp = source.stamp(input);
    if (entry && !entry.error && entry.stamp === stamp && now - entry.builtAt < FULL_MS) {
      entry.checkedAt = now;
      return entry;
    }
    entry = entry ?? { stamp: null, value: null, error: null, builtAt: 0, checkedAt: 0, building: null };
    lists.set(key, entry);
    entry.building = serial(() => source.build(input)).then(
      (value) => Object.assign(entry, { value, error: null, stamp, builtAt: Date.now(), checkedAt: Date.now() }),
      (error) => {
        log(`slash commands (${backend}, ${input.cwd}): ${error.message}`);
        Object.assign(entry, { value: null, error, stamp, builtAt: Date.now(), checkedAt: Date.now() });
      },
    ).finally(() => { entry.building = null; });
    // A list already served keeps serving while its rebuild runs.
    if (wait && !entry.value) await entry.building;
    return entry;
  }

  function inputFor(record) {
    const backend = record.backend ?? "claude";
    const cwd = record.workspace?.path;
    if (typeof cwd !== "string" || !cwd) {
      throw new Error("this agent has no working directory yet");
    }
    if (backend === "claude") return { backend, input: { cwd, model: record.model?.trim() || null } };
    if (backend === "codex") return { backend, input: { cwd, plugins: codexPluginsFor(record.mode) } };
    throw new Error(`the app has no "/" list for ${backend} agents`);
  }

  async function forRun(repoDir, runId) {
    const record = readRun(repoDir, runId);
    const { backend, input } = inputFor(record);
    const entry = await ensure(backend, input, { wait: true });
    if (!entry.value) {
      throw entry.error ?? new Error("the list is still being built");
    }
    if (backend === "claude") {
      return { backend, ...entry.value };
    }
    const { models, configModel } = readCodexModels(env);
    const model = record.model || configModel;
    const commands = buildCodexCommands({ ...entry.value, models, model });
    return { backend, version: entry.value.version, model, commands };
  }

  // At start and every CHECK_MS: lists for the open projects' recent agents.
  function warm() {
    for (const dir of projectDirs()) {
      let ids;
      try {
        ids = fs.readdirSync(path.join(dir, ".rudder", "runs"));
      } catch {
        continue;
      }
      for (const id of ids) {
        let record;
        try {
          record = readRun(dir, id);
          const updated = Date.parse(record.updatedAt ?? record.createdAt ?? "");
          if (!(Date.now() - updated < WARM_AGE_MS) && record.mode !== "main") continue;
          const { backend, input } = inputFor(record);
          if (!fs.existsSync(input.cwd)) continue;
          void ensure(backend, input, { wait: false }).catch((error) => log(`slash commands: ${error.message}`));
        } catch {
          // an agent without a list (still starting, another backend)
        }
      }
    }
  }
  setTimeout(warm, 5_000).unref?.();
  setInterval(warm, CHECK_MS).unref?.();

  return { forRun };
}
