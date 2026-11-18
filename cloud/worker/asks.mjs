// Prompts the phone answers for an agent on the cloud machine (see
// native/src/permission_hook.mjs, which the dashboard wires into each Claude
// agent's settings there). Each pending prompt is <signals>/<run>.asks/<id>.json,
// held open by a hook process; the phone's answer becomes <id>.answer.json,
// which the hook prints as Claude's decision. Codex has no such hook; its
// prompts are read from its rollout (codexAsks, below).

import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const ASK_ID = /^[0-9]+-[0-9a-f]{8}$/;

export function isAskId(id) {
  return ASK_ID.test(String(id));
}
const ASK_TEXT_LIMIT = 40_000;

function askKind(tool) {
  return tool === "AskUserQuestion" ? "question" : tool === "ExitPlanMode" ? "plan" : "permission";
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code === "EPERM";
  }
}

// Long strings (a Write's content, an Edit's old text) are cut for the phone;
// the answer never echoes the input back except for questions, whose input is
// small and is re-read from disk.
function clipInput(value, depth = 0) {
  if (typeof value === "string") {
    return value.length > ASK_TEXT_LIMIT ? `${value.slice(0, ASK_TEXT_LIMIT)}\n…` : value;
  }
  if (Array.isArray(value)) {
    return depth > 6 ? [] : value.slice(0, 50).map((item) => clipInput(item, depth + 1));
  }
  if (value && typeof value === "object") {
    return depth > 6 ? {} : Object.fromEntries(Object.entries(value).slice(0, 50).map(([k, v]) => [k, clipInput(v, depth + 1)]));
  }
  return value;
}

export function readAsks(dir) {
  let names = [];
  try {
    names = fs.readdirSync(dir);
  } catch {
    return [];
  }
  const asks = [];
  for (const name of names) {
    const id = name.endsWith(".json") && !name.endsWith(".answer.json") ? name.slice(0, -5) : null;
    if (!id || !ASK_ID.test(id) || fs.existsSync(path.join(dir, `${id}.answer.json`))) {
      continue;
    }
    let request;
    try {
      request = JSON.parse(fs.readFileSync(path.join(dir, name), "utf8"));
    } catch {
      continue;
    }
    // A hook killed without cleaning up (Claude restarted) leaves its request:
    // nobody is waiting for that answer any more.
    if (!Number.isInteger(request?.pid) || !alive(request.pid)) {
      fs.rmSync(path.join(dir, name), { force: true });
      continue;
    }
    const tool = String(request.tool || "");
    asks.push({
      id,
      ts: Number(request.ts) || 0,
      tool,
      kind: askKind(tool),
      input: clipInput(request.input ?? {}),
      suggestions: Array.isArray(request.suggestions) ? request.suggestions.slice(0, 10) : [],
    });
  }
  return asks.sort((a, b) => a.ts - b.ts);
}

// The hook's stdout for the phone's answer, or { keys } when the answer is a
// key press in the terminal (Claude ignores a hook's "allow" for a plan, but
// does take a "deny" with a message as the user's requested changes).
//
// Claude gets a deny's message as a bare error result. Unattributed, it read a
// revision as an error it had not been asked to follow and went back to its
// plan (haiku, 2.1.327), so the message says it is the user's, in the words
// Claude uses when the terminal's "Tell Claude what to change" is picked.
export function askDecision(request, answer) {
  const kind = askKind(String(request.tool || ""));
  const decide = (decision) => JSON.stringify({ hookSpecificOutput: { hookEventName: "PermissionRequest", decision } });
  const said = (text) => String(text || "").trim().slice(0, 4000);
  if (kind === "plan") {
    if (answer?.approve === true) {
      return { keys: "1" };
    }
    const text = said(answer?.message);
    return { output: decide({ behavior: "deny", message: `The user reviewed this plan in the Rudder app and does not want to proceed with it yet. To tell you how to proceed, the user said:\n${text || "Revise the plan."}` }) };
  }
  if (kind === "question") {
    const questions = Array.isArray(request.input?.questions) ? request.input.questions : [];
    const given = answer?.answers && typeof answer.answers === "object" ? answer.answers : null;
    if (!given) {
      throw new Error("answers are required");
    }
    const answers = {};
    // As Claude's own dialog does: a picked option's preview goes back with
    // the answer, and Claude is told which preview was chosen.
    const annotations = {};
    for (const question of questions) {
      const text = String(question?.question || "");
      const value = given[text];
      const picked = Array.isArray(value) ? value.map((v) => String(v).trim()).filter(Boolean).join(", ") : String(value ?? "").trim();
      if (!picked) {
        throw new Error(`no answer for "${text}"`);
      }
      answers[text] = picked.slice(0, 4000);
      const option = Array.isArray(question.options) ? question.options.find((o) => o?.label === picked) : null;
      if (typeof option?.preview === "string" && option.preview) {
        annotations[text] = { preview: option.preview };
      }
    }
    return { output: decide({ behavior: "allow", updatedInput: { ...request.input, answers, annotations } }) };
  }
  if (answer?.behavior === "allow") {
    const always = answer.always === true && Array.isArray(request.suggestions) && request.suggestions.length > 0;
    return { output: decide(always ? { behavior: "allow", updatedPermissions: request.suggestions } : { behavior: "allow" }) };
  }
  if (answer?.behavior === "deny") {
    const text = said(answer.message);
    return { output: decide({ behavior: "deny", message: `The user denied this from the Rudder app.${text ? ` To tell you how to proceed, the user said:\n${text}` : ""}` }) };
  }
  throw new Error("answer must allow or deny");
}


// ---------------------------------------------------------------------------
// Codex. Workers run with approvals off, so Codex asks the user three ways
// (measured on 0.170): `request_user_input_async` in any mode (the turn goes on,
// the question waits in the pane's "Queued follow-up inputs"), and in Plan mode
// `request_user_input` (the pane waits on it) and, after a plan, the
// "Implement this plan?" menu. No hook covers any of them, so they are read from
// the rollout. Answered in the terminal, the rollout moves on and the card goes.
// The phone answers with keys in the pane.
// ---------------------------------------------------------------------------

const CODEX_TAIL_BYTES = 512 * 1024;
const CODEX_HEAD_BYTES = 16 * 1024;
const codexCache = new Map(); // rollout -> { size, mtimeMs, asks }
const codexHeads = new Map(); // rollout -> { startMs, cwd }, from its first line
const codexNewer = new Map(); // rollout -> { since, ruledOut, newer }

function codexAskId(ts, key) {
  return `${Math.max(0, Math.trunc(ts))}-${createHash("sha256").update(key).digest("hex").slice(0, 8)}`;
}

function parseJson(text) {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function readSlice(file, fromEnd, bytes) {
  const fd = fs.openSync(file, "r");
  try {
    const size = fs.fstatSync(fd).size;
    const start = fromEnd ? Math.max(0, size - bytes) : 0;
    const buffer = Buffer.alloc(Math.min(bytes, size - start));
    fs.readSync(fd, buffer, 0, buffer.length, start);
    return { text: buffer.toString("utf8"), cut: start > 0 };
  } finally {
    fs.closeSync(fd);
  }
}

function tailLines(file, bytes) {
  const { text, cut } = readSlice(file, true, bytes);
  const lines = text.split("\n");
  if (cut) lines.shift();
  return lines;
}

// The answers in a `request_user_input_async` reply, which Codex sends as a user
// message: `<send_user_message_question_reply>[{answer, question,
// questionItemId: "[\"request_user_input_async\",\"<call>\",<index>]"}]</…>`.
function asyncReplies(content) {
  const replies = [];
  for (const part of Array.isArray(content) ? content : []) {
    const match = /<send_user_message_question_reply>([\s\S]*?)<\/send_user_message_question_reply>/.exec(String(part?.text ?? ""));
    const items = match ? parseJson(match[1]) : null;
    for (const item of Array.isArray(items) ? items : []) {
      const id = parseJson(String(item?.questionItemId ?? ""));
      if (Array.isArray(id) && id[0] === "request_user_input_async" && Number.isInteger(id[2])) {
        replies.push({ callId: String(id[1]), index: id[2] });
      }
    }
  }
  return replies;
}

const optionsOf = (q, toOption) => (Array.isArray(q?.options) ? q.options : []).map(toOption);

/** The prompts a Codex rollout's lines leave waiting, in the card shape. */
export function pendingCodexAsks(lines) {
  let question = null;
  let plan = null;
  // This turn's async calls: callId -> { ts, questions, answered, accepted }.
  // Their pane goes when the turn ends, answered or not.
  const later = new Map();
  for (const line of lines) {
    if (!line.includes('"request_user_input') && !line.includes('"event_msg"') && !line.includes('"function_call_output"') && !line.includes("send_user_message_question_reply")) continue;
    const entry = parseJson(line);
    const payload = entry?.payload ?? {};
    const ts = Date.parse(entry?.timestamp) || 0;
    if (entry?.type === "response_item") {
      if (payload.type === "function_call" && (payload.name === "request_user_input" || payload.name === "request_user_input_async")) {
        const questions = parseJson(payload.arguments)?.questions;
        const call = Array.isArray(questions) && questions.length && payload.call_id ? { callId: String(payload.call_id), ts, questions } : null;
        if (payload.name === "request_user_input") {
          question = call;
        } else if (call) {
          later.set(call.callId, { ...call, answered: new Set(), accepted: false });
        }
      } else if (payload.type === "function_call_output") {
        if (payload.call_id === question?.callId) {
          question = null;
        }
        // `{"accepted":true}` once the pane shows it; anything else is an error
        // the model gets back (bad arguments), and nothing was asked.
        const call = later.get(payload.call_id);
        if (call && parseJson(payload.output)?.accepted === true) {
          call.accepted = true;
        } else if (call) {
          later.delete(payload.call_id);
        }
      } else if (payload.type === "message" && payload.role === "user") {
        for (const reply of asyncReplies(payload.content)) {
          later.get(reply.callId)?.answered.add(reply.index);
        }
      }
    } else if (entry?.type === "event_msg") {
      if (payload.type === "task_started" || payload.type === "turn_aborted") {
        question = null;
        plan = null;
        later.clear();
      } else if (payload.type === "task_complete") {
        question = null;
        later.clear();
        plan = plan && plan.turnId === payload.turn_id ? { ...plan, done: true } : null;
      } else if (payload.type === "item_completed" && payload.item?.type === "Plan" && typeof payload.item.text === "string") {
        plan = { turnId: payload.turn_id, ts, text: payload.item.text, done: false };
      }
    }
  }
  const asks = [];
  if (question) {
    asks.push({
      id: codexAskId(question.ts, question.callId),
      ts: question.ts,
      tool: "request_user_input",
      kind: "question",
      input: clipInput({
        questions: question.questions.map((q) => ({
          question: String(q?.question ?? ""),
          header: String(q?.header ?? ""),
          multiSelect: false,
          options: optionsOf(q, (o) => ({ label: String(o?.label ?? ""), description: String(o?.description ?? "") })),
        })),
      }),
      suggestions: [],
    });
  }
  // The pane lists every open async question in order, so only the oldest
  // call is a card: its keys start from the pane's first question.
  const open = [...later.entries()]
    .filter(([, call]) => call.accepted)
    .map(([callId, call]) => ({ callId, call, indexes: call.questions.map((_, index) => index).filter((index) => !call.answered.has(index)) }))
    .filter(({ indexes }) => indexes.length);
  if (open.length) {
    const { callId, call, indexes } = open[0];
    asks.push({
      // A question answered in the terminal changes the id, so the card is rebuilt.
      id: codexAskId(call.ts, `${callId}:${indexes.join(".")}`),
      ts: call.ts,
      tool: "request_user_input_async",
      kind: "question",
      input: clipInput({
        questions: indexes.map((index) => ({
          question: String(call.questions[index]?.title ?? ""),
          header: "",
          multiSelect: false,
          options: optionsOf(call.questions[index], (label) => ({ label: String(label ?? ""), description: "" })),
        })),
      }),
      suggestions: [],
      // Every open question in the pane, this call's and later ones'.
      paneQuestions: open.reduce((sum, { indexes: each }) => sum + each.length, 0),
    });
  }
  if (plan?.done && plan.text.trim()) {
    asks.push({ id: codexAskId(plan.ts, String(plan.turnId)), ts: plan.ts, tool: "Plan", kind: "plan", input: clipInput({ plan: plan.text }), suggestions: [] });
  }
  return asks;
}

// When a rollout's session began and in which folder, from its first line
// (`{"timestamp":…,"type":"session_meta","payload":{…,"cwd":…`). Never changes.
function codexHead(file) {
  if (codexHeads.has(file)) {
    return codexHeads.get(file);
  }
  let text = "";
  try {
    text = readSlice(file, false, CODEX_HEAD_BYTES).text;
  } catch {
    return null;
  }
  const startMs = Date.parse(/^\{"timestamp":"([^"]+)"/.exec(text)?.[1] ?? "");
  const cwd = parseJson(/"type":"session_meta","payload":\{.*?"cwd":("(?:[^"\\]|\\.)*")/.exec(text)?.[1] ?? "");
  if (!Number.isFinite(startMs) || typeof cwd !== "string") {
    return null; // not written yet: look again next time
  }
  codexHeads.set(file, { startMs, cwd });
  return codexHeads.get(file);
}

const pad2 = (n) => String(n).padStart(2, "0");

/**
 * Whether Codex moved on from `rollout` to a newer thread in the same folder
 * after `since`. "Yes, clear context and implement" (and /new) does that and
 * writes nothing more to the old thread, so a plan waiting there is over.
 * Rollouts sit under sessions/YYYY/MM/DD (local time).
 */
function newerCodexThread(rollout, since) {
  let state = codexNewer.get(rollout);
  if (!state || state.since !== since) {
    state = { since, ruledOut: new Set(), newer: false };
    codexNewer.set(rollout, state);
  }
  const own = codexHead(rollout);
  if (state.newer || !own) {
    return state.newer;
  }
  const root = path.resolve(path.dirname(rollout), "..", "..", "..");
  const today = new Date();
  for (const dir of new Set([path.dirname(rollout), path.join(root, String(today.getFullYear()), pad2(today.getMonth() + 1), pad2(today.getDate()))])) {
    let names = [];
    try {
      names = fs.readdirSync(dir);
    } catch {
      continue;
    }
    for (const name of names) {
      const file = path.join(dir, name);
      if (file === rollout || state.ruledOut.has(file) || !name.startsWith("rollout-") || !name.endsWith(".jsonl")) continue;
      let mtimeMs = 0;
      try {
        mtimeMs = fs.statSync(file).mtimeMs;
      } catch {
        continue;
      }
      const head = mtimeMs > since ? codexHead(file) : null;
      if (mtimeMs > since && !head) continue; // its first line is not written yet
      if (head && head.startMs > since && head.cwd === own.cwd) {
        state.newer = true;
        return true;
      }
      state.ruledOut.add(file);
    }
  }
  return false;
}

/**
 * A Codex run's waiting prompts, from its rollout. None once its process is
 * gone: a Codex killed mid-question leaves the call open in the rollout.
 */
export function codexAsks(record, rollout, { isAlive = alive } = {}) {
  const pid = record?.process?.pid;
  if (!rollout || !Number.isInteger(pid) || pid <= 0 || !isAlive(pid)) {
    return [];
  }
  let stat;
  try {
    stat = fs.statSync(rollout);
  } catch {
    return [];
  }
  let asks = null;
  const cached = codexCache.get(rollout);
  if (cached && cached.size === stat.size && cached.mtimeMs === stat.mtimeMs) {
    asks = cached.asks;
  } else {
    try {
      asks = pendingCodexAsks(tailLines(rollout, CODEX_TAIL_BYTES));
    } catch {
      return []; // the rollout went between stat and read
    }
    codexCache.set(rollout, { size: stat.size, mtimeMs: stat.mtimeMs, asks });
  }
  return asks.filter((ask) => ask.kind !== "plan" || !newerCodexThread(rollout, ask.ts));
}

// Text for the pane goes as a paste (native `remote_input_bytes`), hex so
// commas and newlines survive the comma-separated key list.
function pasteKey(text) {
  return `paste:${Buffer.from(text, "utf8").toString("hex")}`;
}

/**
 * The dashboard requests that answer a Codex prompt in its pane, in order: key
 * lists (`keys`, comma-separated, one write) or a message (`steer`).
 *
 * A question's options start on the first; Down moves, Enter answers. Text of
 * your own is a paste on the entry after the options ("None of the above" in
 * Plan mode, where it becomes the note; "Other" for async questions; the only
 * entry when an async question has no options). Plan mode's Enter moves to the
 * next question. Async questions sit behind the main prompt: Shift+Right walks
 * back from wherever the pane is to the prompt (a no-op there, a draft stays),
 * Shift+Left opens the first question, and each Enter sends that answer and
 * shows the next. Keys in the same write as that Shift+Left move the
 * highlight but Enter does nothing, so opening is a write of its own; each
 * answer wakes the model, so the answers go in one write.
 *
 * The plan menu's 1 is "Yes, implement this plan". To revise, Esc closes the
 * menu, then the change goes as a message: in one write, Esc and the paste that
 * follows read as Esc plus the literal text "[200~".
 */
export function codexAskSteps(ask, answer) {
  if (ask.kind === "plan") {
    if (answer?.approve === true) {
      return [{ kind: "keys", instruction: "1" }];
    }
    const text = String(answer?.message || "").trim().slice(0, 4000);
    if (!text) {
      throw new Error("say what should change in the plan");
    }
    return [{ kind: "keys", instruction: "esc" }, { kind: "steer", instruction: text }];
  }
  const given = answer?.answers && typeof answer.answers === "object" ? answer.answers : null;
  if (!given) {
    throw new Error("answers are required");
  }
  const keys = [];
  for (const question of ask.input.questions) {
    const value = given[question.question];
    const picked = (Array.isArray(value) ? value.map((v) => String(v).trim()).filter(Boolean).join(", ") : String(value ?? "")).trim();
    if (!picked) {
      throw new Error(`no answer for "${question.question}"`);
    }
    const index = question.options.findIndex((option) => option.label === picked);
    keys.push(...Array(index >= 0 ? index : question.options.length).fill("down"));
    if (index < 0) {
      keys.push(pasteKey(picked.slice(0, 4000)));
    }
    keys.push("enter");
  }
  const answers = { kind: "keys", instruction: keys.join(",") };
  if (ask.tool !== "request_user_input_async") {
    return [answers];
  }
  const open = [...Array(Math.max(1, ask.paneQuestions || 0)).fill("shift-right"), "shift-left"];
  return [{ kind: "keys", instruction: open.join(",") }, answers];
}
