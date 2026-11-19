// Moving a chat to another harness: the app's model control picks a model of
// another harness (Claude Code to Codex, say), and the dashboard starts that
// harness in the same agent and workspace on a new conversation (native
// agent_model.rs deliver_harness_request). Its first prompt is this handoff.
//
// The handoff is built here from the old conversation's newest messages, not
// written by a model: asking the old agent (or any model) for a summary would
// cost a whole turn before the new harness could start. The new harness gets
// the goal, where the old one stopped, the files it changed and as much of
// the recent conversation as fits, newest kept first, and it writes the
// summary for the user itself as its first reply, shaped by the user's
// instructions.

import path from "node:path";

const HEADER = "Rudder handoff: this chat moved to you from ";
// Its own line: a mode that frames the prompt (plan mode) puts words before it.
const HEADER_LINE = /(?:^|\n)Rudder handoff: this chat moved to you from ([^\n]{1,120})\.\n/;
const NEXT = "\n## What to do now\n";
// How much of the conversation the handoff carries, in characters: about 4k
// tokens, enough for the last several turns and small enough that the new
// harness starts its first reply at once.
const CONVERSATION_BUDGET = 14_000;
const MESSAGE_MAX = 1_600;
const SUMMARY_MAX = 3_000;
const TOOL_MAX = 100;
const TOOLS_PER_STEP = 6;
const FILES_MAX = 30;
const CLAUDE_EDIT_TOOLS = new Set(["Edit", "Write", "MultiEdit", "NotebookEdit"]);
const OPENCODE_EDIT_TOOLS = new Set(["edit", "write", "multiedit"]);
// A Codex patch's file headers, also inside a code-mode script's string
// (`tools.apply_patch("*** Begin Patch\n*** Update File: ...")`).
const PATCH_FILE = /\*\*\* (?:Add|Update|Delete) File: (.+?)(?=\\n|\n|"|$)/g;
// Turns Codex adds as the user that the user did not type.
const CODEX_INJECTED = /^<codex_internal_context\b/;

const clip = (text, max) => {
  const value = String(text || "").trim();
  return value.length > max ? `${value.slice(0, max - 1).trimEnd()}…` : value;
};

/**
 * The handoff prompt. `messages` are the chat's newest messages as the app
 * shows them (transcripts.mjs recentConversation), `lines` the session's
 * JSON lines they came from (OpenCode's: its tool parts), `backend` the old
 * harness, `from` and `to` the harness and model in words
 * ("Claude · Opus 5.5"), `working` whether the old one was mid-turn.
 */
export function handoffPrompt({ goal, messages, lines, backend, cwd, from, to, instructions, working }) {
  const fromHarness = from.split(" · ")[0];
  const parts = [
    `${HEADER}${from}.`,
    `You are ${to}. You are taking over this chat from another coding agent, and you have not seen its conversation, so this is what it knew. You work in the same workspace and files.`,
    "",
    "## Goal",
    clip(goal, 2_000) || "Not recorded.",
  ];
  const asks = String(instructions || "").trim();
  if (asks) {
    parts.push("", "## The user's instructions for this handoff", clip(asks, 4_000));
  }
  const shown = messages.filter((m) => m.role === "user" || m.role === "assistant" || m.role === "tool" || m.kind === "compact" || m.kind === "handoff" || (m.kind === "peer" && m.text));
  parts.push("", "## Where it stopped", stopped(shown, working, cwd));
  const files = changedFiles(lines, backend, cwd);
  if (files.length) {
    // Inline code, so the chat's handoff marker opens each one (cloud/app/file-refs.js).
    parts.push("", "## Files it changed, newest first", ...files.map((file) => `- \`${file}\``));
  }
  const conversation = recent(shown, fromHarness, cwd);
  if (conversation) {
    parts.push("", "## Recent conversation, oldest first", conversation);
  }
  const summary = "Start with a short summary for the user, at most 8 lines: the goal, decisions made, the current state, files touched and next steps. Follow the user's instructions above, if any.";
  parts.push(
    "",
    NEXT.trim(),
    working
      ? `${summary} Then say what it was doing when it stopped, and carry on with that work.`
      : `${summary} Then wait for the user's next message.`,
  );
  return parts.join("\n");
}

/**
 * A handoff prompt as the chat shows it: where the chat came from and what
 * the new harness was told (transcripts.mjs shows it as one marker). Null
 * for any other message.
 */
export function handoffShown(text) {
  const value = String(text || "");
  const match = HEADER_LINE.exec(value);
  if (!match) {
    return null;
  }
  const body = value.slice(match.index + match[0].length);
  const start = body.indexOf("## ");
  const end = body.indexOf(NEXT);
  return { from: match[1], summary: body.slice(Math.max(0, start), end < 0 ? body.length : end).trim() };
}

/**
 * Why a chat can't move to `backend` (named `label`) now, or null. A harness
 * the machine's last sign-in check found signed out would start an agent
 * that fails at once. `credentials` is that check (home.mjs
 * checkCredentials): before its first answer nothing is known, and the move
 * goes ahead. The app's model sheet refuses the same way (model-picker.js).
 */
export function handoffRefusal(credentials, backend, label) {
  const cred = credentials?.[backend];
  return cred && !cred.ok ? `${label} isn't signed in on your cloud machine.` : null;
}

function stopped(messages, working, cwd) {
  if (!working) {
    return "It had finished its last turn and was waiting for the user.";
  }
  const last = messages.findLast((m) => m.role === "tool");
  const step = last ? ` Its last step: ${toolLine(last.tool, cwd)}.` : "";
  return `It was in the middle of a turn when the chat moved, and it was stopped there.${step}`;
}

// The conversation, newest kept first until the budget is spent, then in
// order. Tool calls between two things said fold into one line.
function recent(messages, fromHarness, cwd) {
  const blocks = [];
  let tools = [];
  const flush = () => {
    if (!tools.length) return;
    const more = tools.length > TOOLS_PER_STEP ? `, and ${tools.length - TOOLS_PER_STEP} more` : "";
    blocks.push(`(${fromHarness} ran: ${tools.slice(0, TOOLS_PER_STEP).map((tool) => toolLine(tool, cwd)).join("; ")}${more})`);
    tools = [];
  };
  for (const m of messages) {
    if (m.role === "tool") {
      tools.push(m.tool || {});
      continue;
    }
    flush();
    if (m.kind === "compact") {
      if (m.compact?.summary) {
        blocks.push(`(Earlier context, as ${fromHarness} summarized it when its context filled up:)\n${clip(m.compact.summary, SUMMARY_MAX)}`);
      }
    } else if (m.kind === "handoff") {
      // A chat that moved before: what that harness was handed.
      // Its headings one level down, under this handoff's own.
      const handed = clip(m.handoff.summary, SUMMARY_MAX).replace(/^## /gm, "### ");
      blocks.push(`(Earlier, ${fromHarness} took this chat over from ${m.handoff.from}, which handed it this:)\n${handed}`);
    } else if (m.kind === "peer") {
      // What another agent (a teammate, a subagent) sent it.
      blocks.push(`From ${m.peer.from}, another agent: ${clip(m.text, MESSAGE_MAX)}`);
    } else if (m.role === "user") {
      if (CODEX_INJECTED.test(m.text || "")) continue;
      const files = m.attachments?.length ? `\n(Attached: ${m.attachments.join(", ")})` : "";
      blocks.push(`User: ${clip(m.text, MESSAGE_MAX)}${files}`);
    } else {
      blocks.push(`${fromHarness}: ${clip(m.text, MESSAGE_MAX)}`);
    }
  }
  flush();
  const kept = [];
  let used = 0;
  for (let i = blocks.length - 1; i >= 0; i -= 1) {
    if (used + blocks[i].length > CONVERSATION_BUDGET && kept.length) break;
    kept.push(blocks[i]);
    used += blocks[i].length + 2;
  }
  const cut = kept.length < blocks.length ? "(Earlier messages left out.)\n\n" : "";
  return cut + kept.reverse().join("\n\n");
}

function toolLine(tool, cwd) {
  const raw = String(tool?.input || "").replace(/\s+/g, " ");
  const input = clip(cwd ? raw.split(`${cwd}/`).join("") : raw, TOOL_MAX);
  return input ? `${tool?.name || "tool"} ${input}` : tool?.name || "tool";
}

// The files the session's edit calls wrote, newest first: Claude's Edit,
// Write, MultiEdit and NotebookEdit, Codex's patches, and OpenCode's edit,
// write and multiedit calls and patches. Only lines that can hold one are
// parsed.
const MAY_CHANGE = {
  claude: (line) => line.includes('"tool_use"'),
  codex: (line) => line.includes(" File: "),
  opencode: (line) => line.includes('"tool"'),
};

function changedFiles(lines, backend, cwd) {
  const mayChange = MAY_CHANGE[backend];
  if (!mayChange) {
    throw new Error(`no handoff reads ${backend} sessions`);
  }
  const seen = new Set();
  const files = [];
  const add = (file) => {
    const value = String(file || "").trim();
    if (!value) return;
    const shown = cwd && path.isAbsolute(value) && value.startsWith(`${cwd}/`) ? value.slice(cwd.length + 1) : value;
    if (!seen.has(shown)) {
      seen.add(shown);
      files.push(shown);
    }
  };
  for (let i = lines.length - 1; i >= 0 && files.length < FILES_MAX; i -= 1) {
    const line = lines[i];
    if (!mayChange(line)) continue;
    const entry = parseLine(line);
    if (backend === "opencode") {
      const input = entry?.type === "tool" ? entry.state?.input : null;
      if (OPENCODE_EDIT_TOOLS.has(entry?.tool)) {
        add(input?.filePath);
      } else if (typeof input?.patchText === "string") {
        [...input.patchText.matchAll(PATCH_FILE)].map((match) => match[1]).reverse().forEach(add);
      }
    } else if (backend === "codex") {
      const payload = entry?.payload;
      if (payload?.type !== "custom_tool_call" && payload?.type !== "function_call") continue;
      const text = String(payload.input ?? payload.arguments ?? "");
      [...text.matchAll(PATCH_FILE)].map((match) => match[1]).reverse().forEach(add);
    } else if (entry?.type === "assistant" && Array.isArray(entry.message?.content)) {
      entry.message.content
        .filter((block) => block?.type === "tool_use" && CLAUDE_EDIT_TOOLS.has(block.name))
        .reverse()
        .forEach((block) => add(block.input?.file_path ?? block.input?.notebook_path));
    }
  }
  return files.slice(0, FILES_MAX);
}

// A live session's newest line can be mid-write; it holds nothing yet.
function parseLine(line) {
  try {
    return JSON.parse(line);
  } catch {
    return null;
  }
}
