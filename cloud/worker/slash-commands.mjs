// What "/" offers in an agent's own CLI, for the "/" menu in the app's chat
// composer. Each list is built on the home ahead of time, per agent working
// directory, kept on disk across restarts, and kept on each device too
// (cloud/app/slash-menu.js), so the menu opens at once.
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
// says which built-ins open a picker or a panel in the TUI. That answer alone
// is not the menu: on 2.1.327 it lacks 53 of the TUI's built-ins (/resume,
// /plan, /status...), every MCP prompt and some skills, and lists hidden ones.
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
//
// OpenCode. The CLI's local /command endpoint resolves project, user and MCP
// commands. The TUI registers its own actions separately.
//
// Antigravity. `agy` lists every command its "/" popup offers (built-ins,
// its skills, the user's and the project's) on the commands tab of /help, in
// the popup's order with aliases and descriptions. An interactive session in
// the agent's directory, started off-screen in a private tmux, is sent /help
// and that tab is read (readAntigravityMenu). Nothing reaches a model.
// `agy -p /help` is not the list: print mode has 12 of the 51 (1.5.18).

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import { buildOpencodeCommands, OPENCODE_VERSION, readOpencodeCommands } from "./opencode-commands.mjs";

const MAX_DESCRIPTION = 400;

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

// In the chat these open its model sheet, the app's control for both.
const CLAUDE_MODEL_COMMANDS = new Set(["model", "effort"]);
// The chat shows what these do itself (/goal: the header's goal chip), so
// they never bring up the terminal view.
const CLAUDE_CHAT_COMMANDS = new Set(["goal"]);
// These open UI in the TUI whatever follows them: a picker, a form, a wizard
// or an answer drawn over the prompt that the session never records. Every
// other command that opens UI does so only bare: given its argument
// (/rename name, /fast on, /add-dir path, /color red) it applies it and
// prints one line, which the chat shows, so the terminal stays closed.
const CLAUDE_TUI_WITH_ARGS = new Set(["auto-mode-setup", "btw", "bug", "cd", "context", "copy", "feedback", "help", "ide", "import", "mcp", "resume"]);

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
// The popup fills in over about 300 ms (skills, plugins, account-gated rows),
// then MCP servers' prompts join it as the servers connect, remote ones a
// second or more later. Its first reading is taken once it has held still for
// MENU_FIRST_MS; the list is read once it has not changed for MENU_STILL_MS,
// no sooner than MENU_SETTLE_MS after "/".
const MENU_FIRST_MS = 300;
const MENU_SETTLE_MS = 3000;
const MENU_STILL_MS = 2000;
const MENU_POLL_MS = 100;
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
 * private tmux server with the agent's environment, model and permission
 * mode and hooks off, sent "/". Returns [{ display, description }] in
 * Claude's order. `onFirst` is awaited with the popup's first reading, before
 * MCP servers' prompts have all joined it.
 *
 * The permission mode is the agent's own (launch.rs claude_session_command):
 * under bypassPermissions Claude starts a project's .mcp.json servers without
 * asking, so their prompts are in the agent's menu. Without it, a folder whose
 * .mcp.json servers were never approved opened Claude's "New MCP server found"
 * dialog, and the probe waited its whole timeout for a prompt (the app's
 * request gave up first).
 */
export async function readClaudeMenu({ bin, cwd, env, model = null, permissionMode, onFirst = null, timeoutMs = 45_000 }) {
  if (permissionMode !== "bypassPermissions" && permissionMode !== "plan") {
    throw new Error(`readClaudeMenu needs the agent's permission mode, not ${JSON.stringify(permissionMode)}`);
  }
  const tmux = which("tmux", env);
  // tmux leaves its socket behind when the server is killed.
  const socket = path.join(os.tmpdir(), `rudder-slash-${process.pid}-${menuSeq++}`);
  const t = (...args) => run(tmux, ["-S", socket, "-f", "/dev/null", ...args], { cwd, env });
  const screen = async () => (await t("capture-pane", "-p", "-t", "menu")).stdout;
  const deadline = Date.now() + timeoutMs;
  const pause = () => new Promise((resolve) => setTimeout(resolve, MENU_POLL_MS));
  // The pane's process leads its own process group (tmux starts it in a new
  // session); Claude's MCP servers run in it too.
  let group = null;
  try {
    // remain-on-exit keeps a Claude that quit on screen, so its error is read.
    await t("set-option", "-g", "remain-on-exit", "on", ";",
      "new-session", "-d", "-s", "menu", "-x", String(MENU_COLUMNS), "-y", String(MENU_ROWS), "-c", cwd,
      // Rudder starts every Claude agent with the inline renderer (launch.rs).
      "env", "-u", "TMUX", "-u", "TMUX_PANE", "TERM=xterm-256color", "CLAUDE_CODE_NO_FLICKER=0",
      bin, "--settings", JSON.stringify({ disableAllHooks: true }), "--permission-mode", permissionMode,
      ...(model ? ["--model", model] : []));
    const panePid = (await t("display-message", "-p", "-t", "menu", "#{pane_pid}")).stdout.trim();
    if (!/^\d+$/.test(panePid)) throw new Error(`tmux gave no process id for Claude Code's pane: "${panePid}"`);
    group = Number(panePid);
    let text = "";
    for (;;) {
      text = await screen();
      // A dialog before the prompt can put its own "❯" on screen: say which
      // one at once instead of typing "/" into it and waiting out the timeout.
      if (/Is this a project you created or one you trust/.test(text)) {
        throw new Error(`Claude Code has not been trusted in ${cwd}`);
      }
      if (/New MCP servers? found in this project/.test(text)) {
        throw new Error(`Claude Code asks whether to use the project's MCP servers in ${cwd} (${permissionMode} mode)`);
      }
      if (/running in Bypass Permissions mode/.test(text)) {
        throw new Error("Claude Code asks to accept Bypass Permissions mode for this account");
      }
      if (/^❯\s/m.test(text)) break;
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
    let first = !onFirst;
    for (;;) {
      await pause();
      const rows = parseClaudeMenu(await screen());
      const sig = JSON.stringify(rows);
      if (sig !== last) {
        last = sig;
        since = Date.now();
      } else if (rows && Date.now() - since >= MENU_STILL_MS && Date.now() - sent >= MENU_SETTLE_MS) {
        return rows;
      } else if (rows && !first && Date.now() - since >= MENU_FIRST_MS) {
        first = true;
        await onFirst(rows);
      }
      if (Date.now() > deadline) {
        throw new Error(rows ? "Claude Code's menu kept changing" : "Claude Code showed no menu for /");
      }
    }
  } finally {
    // No pane means new-session failed and there is no server to stop.
    if (group === null) await t("kill-server").catch(() => {});
    else {
      await t("kill-server");
      // kill-server returns before Claude exits; it and its MCP servers still
      // write to its config while they shut down. Return only once they are gone.
      await reapGroup(group);
    }
    fs.rmSync(socket, { force: true });
  }
}

// Signals the group; false once no process of ours is left in it. ESRCH: the
// group is gone. EPERM: only members we may not signal remain, which on macOS
// are its exiting (zombie) processes: they run no code and write nothing.
function signalGroup(group, signal) {
  try {
    process.kill(-group, signal);
    return true;
  } catch (error) {
    if (error.code === "ESRCH" || error.code === "EPERM") return false;
    throw error;
  }
}

async function reapGroup(group) {
  const settle = async (ms) => {
    const until = Date.now() + ms;
    while (signalGroup(group, 0) && Date.now() < until) await new Promise((resolve) => setTimeout(resolve, 50));
    return !signalGroup(group, 0);
  };
  if (await settle(5_000)) return;
  signalGroup(group, "SIGKILL");
  if (!(await settle(2_000))) throw new Error(`Claude Code's menu probe (process group ${group}) did not exit`);
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
    const opens = tui && CLAUDE_MODEL_COMMANDS.has(name) ? "model" : interactive && !CLAUDE_CHAT_COMMANDS.has(name) ? "terminal" : null;
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
      ...(opens === "terminal" && CLAUDE_TUI_WITH_ARGS.has(name) ? { when: "always" } : {}),
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
  ["goal", "set or view the goal for a long-running task", { args: true, gate: "goals" }],
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
  if (!features.cli_daybreak) return null;
  if (!chatgpt && !(apiKey && features.api_key_cyber_access_programs)) return null;
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
    case "goals": return state.features.goals === true;
    case "plugins": return state.features.plugins === true;
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
 * Codex's "/" popup for one agent. `features`: { name: enabled }, as
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
        const enabled = tier.id === "flex" ? true : tier.id === "ultrafast" ? features.ultrafast_mode === true : features.fast_mode === true;
        if (!enabled) continue;
        commands.push({ name: String(tier.name).toLowerCase(), description: clip(tier.description), hint: null, args: null, opens: null, immediate: true, aliases: [] });
      }
    }
  }
  return commands;
}

/** `codex features list` output -> { name: enabled } (plain, so a saved list keeps it). */
export function parseCodexFeatures(text) {
  const features = {};
  for (const line of text.split("\n")) {
    const m = /^([a-z0-9_]+)\s+.+\s(true|false)\s*$/.exec(line.trim());
    if (m) features[m[1]] = m[2] === "true";
  }
  if (!Object.keys(features).length) {
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

// ---------------------------------------------------------------------------
// Antigravity
// ---------------------------------------------------------------------------

// In the chat these open its model sheet, the app's control for both.
const ANTIGRAVITY_MODEL_COMMANDS = new Set(["model", "effort"]);
// These draw a panel, a picker or a form over the prompt that the
// conversation never records, so the chat brings up the terminal with them.
const ANTIGRAVITY_TUI_COMMANDS = new Set(["agents", "artifact", "changelog", "config", "context", "credits", "diff", "feedback", "help",
  "hooks", "keybindings", "mcp", "open", "permissions", "plugin", "projects", "record", "resume", "rewind", "skills", "tasks", "usage", "voice"]);
// Named with what they act on (`/swarm <task>`, `/codesearch <query>`): picked
// bare, the menu fills the name in and waits for the rest.
const ANTIGRAVITY_NEEDS_ARGS = new Set(["btw", "codesearch", "goal", "schedule", "swarm"]);

/** The rows of the commands tab of agy's /help: [{ name, aliases, description }], or null when it is not showing. */
export function parseAntigravityMenu(screen) {
  const lines = screen.split("\n").map((line) => line.trimEnd());
  const start = lines.findIndex((line) => line.trim() === "Available Commands");
  if (start < 0) return null;
  const rows = [];
  for (const line of lines.slice(start + 1)) {
    if (/^\s*[↑↓] \d+ more/.test(line)) throw new Error("Antigravity's command list did not fit the off-screen terminal");
    // "> /name (alias, alias)  description"; the first row carries the cursor.
    const m = /^[> ] \/(\S+)(?: \(([^)]*)\))?(?: {2,}(.*))?$/.exec(line);
    if (!m) {
      if (rows.length) break;
      continue;
    }
    rows.push({ name: m[1], aliases: m[2] ? m[2].split(",").map((alias) => alias.trim()).filter(Boolean) : [], description: (m[3] ?? "").trim() });
  }
  return rows.length ? rows : null;
}

/** The app's rows for agy's command list (`menu`, parseAntigravityMenu). */
export function buildAntigravityCommands(menu) {
  return menu.map(({ name, aliases, description }) => ({
    name,
    description: clip(description),
    hint: null,
    args: ANTIGRAVITY_NEEDS_ARGS.has(name) ? "required" : null,
    opens: ANTIGRAVITY_MODEL_COMMANDS.has(name) ? "model" : ANTIGRAVITY_TUI_COMMANDS.has(name) ? "terminal" : null,
    aliases,
  }));
}

/**
 * agy's command list in `cwd`: an interactive session started in a private
 * tmux server with the agent's environment, sent /help and moved to its
 * commands tab. Returns { version, menu }. The session is given no prompt, so
 * no model is called and no conversation is kept.
 */
export async function readAntigravityMenu({ bin, cwd, env, timeoutMs = 45_000 }) {
  const tmux = which("tmux", env);
  const socket = path.join(os.tmpdir(), `rudder-slash-${process.pid}-${menuSeq++}`);
  const t = (...args) => run(tmux, ["-S", socket, "-f", "/dev/null", ...args], { cwd, env });
  const screen = async () => (await t("capture-pane", "-p", "-t", "menu")).stdout;
  const deadline = Date.now() + timeoutMs;
  const pause = () => new Promise((resolve) => setTimeout(resolve, MENU_POLL_MS));
  // Waits until `test(screen)` gives a value; `what` names it in the error.
  const until = async (test, what) => {
    for (;;) {
      const text = await screen();
      if (/Do you trust the contents of this project\?/.test(text)) {
        throw new Error(`Antigravity has not been trusted in ${cwd}`);
      }
      const found = test(text);
      if (found) return found;
      const dead = (await t("display-message", "-p", "-t", "menu", "#{pane_dead}")).stdout.trim() === "1";
      if (dead || Date.now() > deadline) {
        const said = text.split("\n").map((line) => line.trim()).filter(Boolean).slice(0, 3).join(" ");
        throw new Error(`Antigravity ${dead ? "exited" : `showed no ${what}`} before its commands could be read${said ? `: ${said}` : ""}`);
      }
      await pause();
    }
  };
  let group = null;
  try {
    await t("set-option", "-g", "remain-on-exit", "on", ";",
      "new-session", "-d", "-s", "menu", "-x", String(MENU_COLUMNS), "-y", String(MENU_ROWS), "-c", cwd,
      // No Rudder run: the global hook and MCP entries answer for themselves
      // (signals.rs), and the session reports to nobody.
      "env", "-u", "TMUX", "-u", "TMUX_PANE", "-u", "RUDDER_AGY_HOOK", "-u", "RUDDER_AGY_MCP", "TERM=xterm-256color",
      // As Rudder starts every agy agent (launch.rs antigravity_mode_args).
      bin, "--dangerously-skip-permissions");
    const panePid = (await t("display-message", "-p", "-t", "menu", "#{pane_pid}")).stdout.trim();
    if (!/^\d+$/.test(panePid)) throw new Error(`tmux gave no process id for Antigravity's pane: "${panePid}"`);
    group = Number(panePid);
    // The banner names the version above the composer ("Antigravity CLI 1.5.18").
    const version = await until((text) => (/^>\s*$/m.test(text) || /^> \S/m.test(text)) && /Antigravity CLI (\d+\.\d+\.\d+\S*)/.exec(text)?.[1], "prompt");
    await t("send-keys", "-t", "menu", "-l", "/help");
    // The popup narrows to /help before Enter runs it.
    await until((text) => /^> \/help\s/m.test(text.split("\n").filter((line) => /^[> ] \/\S/.test(line)).join("\n")), "/help in its menu");
    await t("send-keys", "-t", "menu", "Enter");
    await until((text) => /^Antigravity CLI\s+general\s+commands\s+shortcuts/m.test(text), "help panel");
    await t("send-keys", "-t", "menu", "Right");
    let last = null;
    let since = 0;
    for (;;) {
      await pause();
      const rows = parseAntigravityMenu(await screen());
      const sig = JSON.stringify(rows);
      if (sig !== last) {
        last = sig;
        since = Date.now();
      } else if (rows && Date.now() - since >= MENU_STILL_MS) {
        return { version, menu: rows };
      }
      if (Date.now() > deadline) {
        throw new Error(rows ? "Antigravity's command list kept changing" : "Antigravity showed no command list in /help");
      }
    }
  } finally {
    if (group === null) await t("kill-server").catch(() => {});
    else {
      await t("kill-server");
      await reapGroup(group);
    }
    fs.rmSync(socket, { force: true });
  }
}

// What agy's command list depends on: the CLI, its built-in skills and
// plugins, the user's skills, plugins and MCP servers (~/.gemini/config), and
// the project's (.agents in the folder and its parents).
function antigravityStamp({ bin, cwd, env }) {
  const gemini = path.join(env.HOME || os.homedir(), ".gemini");
  const real = fs.realpathSync(bin);
  const parts = [real, fileStamp(real), fileStamp(path.join(gemini, "antigravity-cli", "builtin", ".manifest"))];
  const customizations = (dir) => {
    for (const name of ["skills.json", "plugins.json", "mcp_config.json"]) parts.push(fileStamp(path.join(dir, name)));
    for (const name of ["skills", "plugins"]) treeStamp(path.join(dir, name), 2, parts);
  };
  customizations(path.join(gemini, "config"));
  for (let dir = path.resolve(cwd); ; dir = path.dirname(dir)) {
    customizations(path.join(dir, ".agents"));
    if (path.dirname(dir) === dir) break;
  }
  return createHash("sha256").update(parts.join("\n")).digest("hex");
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
const WARM_MS = 5_000; // how soon a new agent's list starts building
const WARM_AGE_MS = 24 * 3600_000; // agents active this recently get a list at start
const SAVE_MS = 2_000; // built lists reach the disk this soon after a build

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

// Claude's config dir for `env`: the agent's account (src/accounts.ts).
function claudeConfigDir(env) {
  return env.CLAUDE_CONFIG_DIR?.trim() || path.join(env.HOME || os.homedir(), ".claude");
}

// What every list of one account shares, and changes over time without
// changing which folders share a list: the CLI, the account's own commands,
// skills, settings and plugins, and the sign-in and MCP servers in its global
// config. Files count by content: Claude Code rewrites some of them
// unchanged. A change rebuilds the list while it keeps serving; nothing else
// does. Left out: skill usage, which orders the most used skills first in
// Claude's own popup and changes on every skill an agent runs, and the
// marketplaces file, whose lastUpdated every Claude Code start rewrites (our
// own builds too, so each one set off the next); installed_plugins.json names
// what the menu gets from them.
export function claudeStamp({ bin, env }) {
  const config = claudeConfigDir(env);
  const real = fs.realpathSync(bin);
  const files = new Map();
  for (const name of ["settings.json", "settings.local.json", "plugins/installed_plugins.json"]) {
    contentTree(path.join(config, name), name, 0, files);
  }
  contentTree(path.join(config, "commands"), "commands", 4, files);
  const skills = path.join(config, "skills");
  for (const name of statOrNull(skills)?.isDirectory() ? fs.readdirSync(skills).sort() : []) {
    if (name !== "synced") contentTree(path.join(skills, name), `skills/${name}`, 1, files);
  }
  const parts = [real, fileStamp(real), JSON.stringify([...files])];
  syncedSkillsStamp(path.join(skills, "synced"), parts);
  const { oauthAccount, mcpServers } = claudeGlobalConfig(env);
  parts.push(JSON.stringify([oauthAccount, mcpServers]));
  return parts.join("\n");
}

// A file's sha256, read again only when its inode, size or mtime changes.
const contentHashes = new Map(); // file -> { sig, hash }
function contentHash(file, stat) {
  const sig = `${stat.ino}:${stat.size}:${stat.mtimeMs}`;
  const cached = contentHashes.get(file);
  if (cached?.sig === sig) return cached.hash;
  const hash = createHash("sha256").update(fs.readFileSync(file)).digest("hex");
  contentHashes.set(file, { sig, hash });
  return hash;
}

function statOrNull(file) {
  try {
    return fs.statSync(file);
  } catch (error) {
    if (error.code === "ENOENT" || error.code === "ENOTDIR") return null;
    throw error;
  }
}

// `rel` -> content hash of the file at `file`, or of each file in the folder
// there, `depth` folder levels down (a folder deeper than that counts by its
// name).
function contentTree(file, rel, depth, out) {
  const stat = statOrNull(file);
  if (!stat) return;
  if (stat.isFile()) {
    out.set(rel, contentHash(file, stat));
    return;
  }
  if (!stat.isDirectory()) return;
  if (depth === 0) {
    out.set(rel, "folder");
    return;
  }
  for (const name of fs.readdirSync(file).sort()) {
    if (out.size > 4000) return;
    contentTree(path.join(file, name), `${rel}/${name}`, depth - 1, out);
  }
}

/**
 * What a folder's own files add to Claude's menu, from `cwd` up to "/":
 * project commands, skills and settings, .mcp.json, and the folder's MCP
 * entries in the account's config. Claude reads these from the working folder
 * up to the checkout (a jj workspace has no .git of its own), and a name found
 * twice with the same content counts once. So each file is named by its path
 * inside its folder, and the view lists each name's distinct contents, the
 * nearest folder first. A branch's workspace that holds nothing of its own,
 * or the same files as its checkout, has its checkout's view: they share one
 * list, built once.
 */
function claudeProjectView(cwd, env) {
  const { projects } = claudeGlobalConfig(env);
  const view = new Map(); // name -> [hash, ...]
  for (let dir = path.resolve(cwd); ; dir = path.dirname(dir)) {
    const found = new Map();
    contentTree(path.join(dir, ".mcp.json"), ".mcp.json", 0, found);
    const claude = path.join(dir, ".claude");
    if (statOrNull(claude)?.isDirectory()) {
      for (const name of ["settings.json", "settings.local.json"]) {
        contentTree(path.join(claude, name), `.claude/${name}`, 0, found);
      }
      contentTree(path.join(claude, "commands"), ".claude/commands", 4, found);
      contentTree(path.join(claude, "skills"), ".claude/skills", 2, found);
    }
    const project = projects?.[dir];
    const mcp = project && [project.mcpServers, project.enabledMcpjsonServers, project.disabledMcpjsonServers, project.disabledMcpServers];
    if (mcp?.some((value) => value !== undefined)) {
      found.set("projects", createHash("sha256").update(JSON.stringify(mcp)).digest("hex"));
    }
    for (const [name, hash] of found) {
      const hashes = view.get(name) ?? [];
      if (!hashes.includes(hash)) hashes.push(hash);
      view.set(name, hashes);
    }
    if (path.dirname(dir) === dir) break;
  }
  return [...view].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
}

// claude.ai skills (skills/synced/<account>/). Each Claude Code sync round
// rewrites the account's manifest.json, unchanged, and a round marker: about
// every 10 minutes on a home with agents working. Stamped by mtime, every
// round would restart Claude Code for every list, so the manifest's content
// is the stamp.
function syncedSkillsStamp(dir, out) {
  let accounts;
  try {
    accounts = fs.readdirSync(dir, { withFileTypes: true });
  } catch (error) {
    if (error.code === "ENOENT" || error.code === "ENOTDIR") return;
    throw error;
  }
  for (const account of accounts) {
    if (!account.isDirectory()) continue;
    const manifest = path.join(dir, account.name, "manifest.json");
    let body;
    try {
      body = fs.readFileSync(manifest);
    } catch (error) {
      if (error.code === "ENOENT") {
        out.push(`${manifest}:-`);
        continue;
      }
      throw error;
    }
    out.push(`${manifest}:${createHash("sha256").update(body).digest("hex")}`);
  }
}

/**
 * Which Claude list an agent gets: one per account, model ("Switch model
 * (currently …)"), permission mode and project view (claudeProjectView), not
 * one per folder, so a new branch's workspace gets the list its checkout
 * already has.
 */
export function claudeListKey({ cwd, model, permissionMode }, env) {
  return `claude\n${createHash("sha256").update(JSON.stringify({
    config: claudeConfigDir(env), model: model ?? null, permissionMode, project: claudeProjectView(cwd, env),
  })).digest("hex")}`;
}

// The account's global config (.claude.json): the sign-in and MCP servers
// gate rows. The file changes all the time for other reasons, so only those
// parts count.
const globalConfigCache = new Map(); // file -> { sig, config }
function claudeGlobalConfig(env) {
  const file = env.CLAUDE_CONFIG_DIR?.trim()
    ? path.join(env.CLAUDE_CONFIG_DIR.trim(), ".claude.json")
    : path.join(env.HOME || os.homedir(), ".claude.json");
  const stat = statOrNull(file);
  if (!stat) return {};
  const sig = `${stat.ino}:${stat.size}:${stat.mtimeMs}`;
  let cached = globalConfigCache.get(file);
  if (cached?.sig !== sig) {
    cached = { sig, config: JSON.parse(fs.readFileSync(file, "utf8")) };
    globalConfigCache.set(file, cached);
  }
  return cached.config;
}

function codexStamp({ bin, cwd, env }) {
  const home = env.CODEX_HOME?.trim() || path.join(env.HOME || os.homedir(), ".codex");
  const real = fs.realpathSync(bin);
  return [real, fileStamp(real), ...["config.toml", "auth.json"].map((name) => fileStamp(path.join(home, name))),
    fileStamp(path.join(cwd, ".codex", "config.toml"))].join("\n");
}

function opencodeStamp({ bin, cwd, env }) {
  const home = env.HOME || os.homedir();
  const config = path.join(env.XDG_CONFIG_HOME || path.join(home, ".config"), "opencode");
  const real = fs.realpathSync(bin);
  const parts = [real, fileStamp(real), env.OPENCODE_CONFIG_CONTENT || ""];
  const configFiles = (dir) => {
    for (const name of ["opencode.json", "opencode.jsonc"]) parts.push(fileStamp(path.join(dir, name)));
  };
  for (const dir of [config, env.OPENCODE_CONFIG_DIR].filter(Boolean)) {
    configFiles(dir);
    for (const name of ["command", "commands"]) treeStamp(path.join(dir, name), 4, parts);
  }
  if (env.OPENCODE_CONFIG) parts.push(fileStamp(env.OPENCODE_CONFIG));
  for (let dir = path.resolve(cwd); ; dir = path.dirname(dir)) {
    configFiles(dir);
    const dot = path.join(dir, ".opencode");
    configFiles(dot);
    for (const name of ["command", "commands"]) treeStamp(path.join(dot, name), 4, parts);
    if (path.dirname(dir) === dir) break;
  }
  // Persist a digest, since OPENCODE_CONFIG_CONTENT can contain credentials.
  return createHash("sha256").update(parts.join("\n")).digest("hex");
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
 * One build at a time, so a start with many agents does not stack processes.
 * A build the app waits on runs before warm-up builds: a home that just
 * started queues one Claude Code start (about 5s) per recent agent, and the
 * app's request gives up after 20s. `add(fn, now)` returns { promise, hurry };
 * hurry() moves a queued warm-up build ahead of the others.
 */
export function buildQueue() {
  const urgent = [];
  const later = [];
  let busy = false;
  const next = () => {
    if (busy) return;
    const job = urgent.shift() ?? later.shift();
    if (!job) return;
    busy = true;
    Promise.resolve().then(job.fn).then(job.resolve, job.reject).finally(() => {
      busy = false;
      next();
    });
  };
  return {
    add(fn, now) {
      const job = { fn };
      const promise = new Promise((resolve, reject) => Object.assign(job, { resolve, reject }));
      (now ? urgent : later).push(job);
      next();
      const hurry = () => {
        const at = later.indexOf(job);
        if (at >= 0) urgent.push(...later.splice(at, 1));
      };
      return { promise, hurry };
    },
  };
}

// Promise.all that waits for every promise before it rejects.
async function settled(promises) {
  const results = await Promise.allSettled(promises);
  const failed = results.find((result) => result.status === "rejected");
  if (failed) throw failed.reason;
  return results.map((result) => result.value);
}

const newEntry = (backend, input) => ({
  backend, input, stamp: null, value: null, error: null, builtAt: 0, checkedAt: 0, building: null, ready: null, hurry: null, settling: false,
});

// The lists `file` holds (saveLists), for folders that still exist and whose
// inputs still give the key they were saved under (`keyOf`). Each is checked
// against its inputs when first asked for, and served meanwhile.
export function loadLists(file, log, keyOf) {
  const lists = new Map();
  let saved;
  try {
    saved = JSON.parse(fs.readFileSync(file, "utf8"));
  } catch (error) {
    if (error.code !== "ENOENT") log(`slash commands: starting without saved lists: ${file}: ${error.message}`);
    return lists;
  }
  for (const [key, { backend, input, stamp, value, builtAt }] of Object.entries(saved)) {
    if (!fs.existsSync(input.cwd)) continue;
    let current;
    try {
      current = keyOf(backend, input);
    } catch (error) {
      log(`slash commands: dropping the saved list for ${input.cwd}: ${error.message}`);
      continue;
    }
    if (current !== key) continue;
    lists.set(key, { ...newEntry(backend, input), stamp, value, builtAt });
  }
  return lists;
}

// Finished lists only: a first reading still settling is not saved.
export function saveLists(file, lists) {
  const out = {};
  for (const [key, { backend, input, stamp, value, builtAt, settling }] of lists) {
    if (value && !settling) out[key] = { backend, input, stamp, value, builtAt };
  }
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(out));
  fs.renameSync(tmp, file);
}

/**
 * The home's "/" lists, kept in `file` across restarts. `projectDirs()` names
 * the open projects; their recent agents get a list at start, and a new agent
 * within WARM_MS. `forRun(repoDir, runId)` answers the app. `envFor(backend,
 * account)` is the environment an agent on that account runs with (its
 * CLAUDE_CONFIG_DIR or CODEX_HOME), so each list is the agent's own.
 */
export function createSlashCommands({ envFor, log, projectDirs, file }) {
  let saveTimer = null;
  const save = () => {
    saveTimer ??= setTimeout(() => {
      saveTimer = null;
      try {
        saveLists(file, lists);
      } catch (error) {
        log(`slash commands: could not save ${file}: ${error.message}`);
      }
    }, SAVE_MS);
    saveTimer.unref?.();
  };
  const binaryCache = new Map(); // claude realpath -> Promise<Map>
  // Codex reads its state in milliseconds; it does not wait behind Claude.
  const queues = { claude: buildQueue(), codex: buildQueue(), opencode: buildQueue(), antigravity: buildQueue() };
  const antigravityBinary = (env) => which(env.RUDDER_AGY_BIN?.trim() || "agy", env);
  const opencodeBinary = (env) => which(env.RUDDER_OPENCODE_BIN?.trim() || "opencode", env);

  const claudeBinary = (bin) => {
    const real = fs.realpathSync(bin);
    if (!binaryCache.has(real)) {
      const read = readClaudeBinaryCommands(real);
      read.catch(() => binaryCache.delete(real));
      binaryCache.set(real, read);
    }
    return binaryCache.get(real);
  };

  const claudeBin = (env) => which(env.RUDDER_CLAUDE_BIN?.trim() || "claude", env);
  const sources = {
    claude: {
      key: claudeListKey,
      stamp: (input, env) => claudeStamp({ bin: claudeBin(env), env }),
      // The popup's first reading is published about a second in; the list
      // replaces it once MCP servers' prompts have joined.
      build: async ({ cwd, model, permissionMode }, publish, env) => {
        const bin = claudeBin(env);
        const info = Promise.all([claudeInitialize({ bin, cwd, env }), claudeBinary(bin)]);
        const shape = async (menu) => {
          const [answer, binary] = await info;
          return { version: answer.version, commands: buildClaudeCommands({ menu, headless: answer.commands, binary }) };
        };
        // Both settle before the build ends, so no probe outlives its build.
        const [menu] = await settled([readClaudeMenu({ bin, cwd, env, model, permissionMode, onFirst: async (rows) => publish(await shape(rows)) }), info]);
        return shape(menu);
      },
    },
    codex: {
      key: ({ cwd, account, plugins }) => `codex\n${account}\n${cwd}\n${plugins}`,
      stamp: ({ cwd }, env) => codexStamp({ bin: which(env.RUDDER_CODEX_BIN?.trim() || "codex", env), cwd, env }),
      build: async ({ cwd, plugins }, publish, env) => {
        const bin = which(env.RUDDER_CODEX_BIN?.trim() || "codex", env);
        return readCodexState({ bin, cwd, env, plugins });
      },
    },
    opencode: {
      key: ({ cwd }) => `opencode\n${cwd}`,
      stamp: ({ cwd }, env) => opencodeStamp({ bin: opencodeBinary(env), cwd, env }),
      build: async ({ cwd }, publish, env) => {
        const bin = opencodeBinary(env);
        const version = (await run(bin, ["--version"], { cwd, env })).stdout.trim();
        if (version !== OPENCODE_VERSION) throw new Error(`OpenCode ${version} has an unverified command list`);
        const commands = await readOpencodeCommands({ bin, cwd, env });
        // Save only picker rows. Server responses contain project prompt templates.
        return {
          version,
          withSession: buildOpencodeCommands(commands, { hasSession: true }),
          withoutSession: buildOpencodeCommands(commands, { hasSession: false }),
        };
      },
    },
    antigravity: {
      key: ({ cwd }) => `antigravity\n${cwd}`,
      stamp: ({ cwd }, env) => antigravityStamp({ bin: antigravityBinary(env), cwd, env }),
      build: async ({ cwd }, publish, env) => {
        const { version, menu } = await readAntigravityMenu({ bin: antigravityBinary(env), cwd, env });
        return { version, commands: buildAntigravityCommands(menu) };
      },
    },
  };
  const keyOf = (backend, input) => {
    const source = sources[backend];
    if (!source) throw new Error(`no "/" list for ${backend}`);
    if (typeof input?.account !== "string" || !input.account) throw new Error("the list names no account");
    return source.key(input, envFor(backend, input.account));
  };
  const lists = loadLists(file, log, keyOf);

  async function ensure(backend, input, { wait }) {
    const source = sources[backend];
    const env = envFor(backend, input.account);
    const key = source.key(input, env);
    let entry = lists.get(key);
    // Rebuilt in the folder of whichever agent asked last, one that exists.
    if (entry) entry.input = input;
    const now = Date.now();
    if (entry?.building) {
      if (wait && !entry.value) {
        entry.hurry();
        await entry.ready;
      }
      return entry;
    }
    // A list is built again only when an input changed, or when it failed
    // and a request (not the warm-up) asks for it.
    const retry = wait && entry?.error;
    if (entry && !retry && now - entry.checkedAt < CHECK_MS) {
      return entry;
    }
    const stamp = source.stamp(input, env);
    if (entry && !retry && entry.stamp === stamp) {
      entry.checkedAt = now;
      return entry;
    }
    entry = entry ?? newEntry(backend, input);
    lists.set(key, entry);
    // A list not served yet is served from a build's first reading (Claude's
    // popup before its MCP prompts are all in) until the build ends.
    let firstReading;
    const first = new Promise((resolve) => { firstReading = resolve; });
    const publish = (value) => {
      if (entry.value) return;
      Object.assign(entry, { value, error: null, settling: true });
      firstReading();
    };
    // A rebuild of a list already served is not urgent: it keeps serving.
    const job = queues[backend].add(() => source.build(input, publish, env), wait && !entry.value);
    entry.hurry = job.hurry;
    entry.building = job.promise.then(
      (value) => Object.assign(entry, { value, error: null, settling: false, stamp, builtAt: Date.now(), checkedAt: Date.now() }),
      (error) => {
        log(`slash commands (${backend}, ${input.cwd}): ${error.message}`);
        Object.assign(entry, { value: null, error, settling: false, stamp, builtAt: Date.now(), checkedAt: Date.now() });
      },
    ).finally(() => {
      entry.building = null;
      save();
    });
    entry.ready = Promise.race([entry.building, first]);
    // A list already served keeps serving while its rebuild runs.
    if (wait && !entry.value) await entry.ready;
    return entry;
  }

  function inputFor(record) {
    const backend = record.backend ?? "claude";
    const cwd = record.workspace?.path;
    if (typeof cwd !== "string" || !cwd) {
      throw new Error("this agent has no working directory yet");
    }
    // A run.json without an account ran on the CLI's own dir (home.mjs agentSummary).
    const account = record.account ?? "main";
    if (backend === "claude") {
      // As launch.rs claude_session_command resumes it.
      const permissionMode = record.mode === "plan" ? "plan" : "bypassPermissions";
      return { backend, input: { cwd, account, model: record.model?.trim() || null, permissionMode } };
    }
    if (backend === "codex") return { backend, input: { cwd, account, plugins: codexPluginsFor(record.mode) } };
    if (backend === "opencode" || backend === "antigravity") return { backend, input: { cwd, account } };
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
      // settling: a first reading; the app asks again for the finished list.
      return { backend, ...entry.value, ...(entry.settling ? { settling: true } : {}) };
    }
    if (backend === "antigravity") return { backend, ...entry.value };
    if (backend === "opencode") {
      const { version, withSession, withoutSession } = entry.value;
      return { backend, version, commands: record.session?.nativeSessionId ? withSession : withoutSession };
    }
    const { models, configModel } = readCodexModels(envFor(backend, input.account));
    const model = record.model || configModel;
    const commands = buildCodexCommands({ ...entry.value, models, model });
    return { backend, version: entry.value.version, model, commands };
  }

  // At start and every WARM_MS: lists for the open projects' recent agents. A
  // list built or checked within CHECK_MS is left alone.
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
  setInterval(warm, WARM_MS).unref?.();

  return { forRun };
}
