// Prompts the phone answers for an agent on the cloud machine (see
// native/src/permission_hook.mjs, which the dashboard wires into each Claude
// agent's settings there). Each pending prompt is <signals>/<run>.asks/<id>.json,
// held open by a hook process; the phone's answer becomes <id>.answer.json,
// which the hook prints as Claude's decision.

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
export function askDecision(request, answer) {
  const kind = askKind(String(request.tool || ""));
  const decide = (decision) => JSON.stringify({ hookSpecificOutput: { hookEventName: "PermissionRequest", decision } });
  const message = (text, fallback) => String(text || "").trim().slice(0, 4000) || fallback;
  if (kind === "plan") {
    if (answer?.approve === true) {
      return { keys: "1" };
    }
    return { output: decide({ behavior: "deny", message: message(answer?.message, "Revise the plan.") }) };
  }
  if (kind === "question") {
    const questions = Array.isArray(request.input?.questions) ? request.input.questions : [];
    const given = answer?.answers && typeof answer.answers === "object" ? answer.answers : null;
    if (!given) {
      throw new Error("answers are required");
    }
    const answers = {};
    for (const question of questions) {
      const text = String(question?.question || "");
      const value = given[text];
      const picked = Array.isArray(value) ? value.map((v) => String(v).trim()).filter(Boolean).join(", ") : String(value ?? "").trim();
      if (!picked) {
        throw new Error(`no answer for "${text}"`);
      }
      answers[text] = picked.slice(0, 4000);
    }
    return { output: decide({ behavior: "allow", updatedInput: { ...request.input, answers } }) };
  }
  if (answer?.behavior === "allow") {
    const always = answer.always === true && Array.isArray(request.suggestions) && request.suggestions.length > 0;
    return { output: decide(always ? { behavior: "allow", updatedPermissions: request.suggestions } : { behavior: "allow" }) };
  }
  if (answer?.behavior === "deny") {
    return { output: decide({ behavior: "deny", message: message(answer.message, "The user denied this from the Rudder app.") }) };
  }
  throw new Error("answer must allow or deny");
}
