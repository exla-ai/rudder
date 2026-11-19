// Rudder's OpenCode plugin. The dashboard loads it into every OpenCode agent
// it runs, through the run's OPENCODE_CONFIG (signals.rs), and says what for
// in the environment:
//
//   RUDDER_OPENCODE_SIGNAL        the run's signal file (signals.rs): done when
//                                 the agent's own session goes idle, error,
//                                 permission and input while it waits on the
//                                 user, working when a turn starts or a wait
//                                 is answered
//   RUDDER_OPENCODE_SESSION_FILE  where it writes the session the agent is on
//   RUDDER_OPENCODE_ASKS          on the cloud machine, where a permission or
//                                 a question waits for the phone's answer, as
//                                 Claude's permission_hook.mjs does
//   RUDDER_OPENCODE_PROMPT        a message to send when OpenCode starts: into
//                                 RUDDER_OPENCODE_SESSION, the session it
//                                 opens with --session (its TUI ignores
//                                 --prompt then), or without one into the
//                                 first session created, a --fork
//   RUDDER_OPENCODE_MODEL, RUDDER_OPENCODE_AGENT, RUDDER_OPENCODE_VARIANT
//                                 what that message runs on
//
// It only writes files: anything it printed would land on OpenCode's screen.
// Measured on OpenCode 1.19.5: a subagent's session goes idle before its
// parent's turn ends; a permission is answered with POST
// /permission/<id>/reply and a question with /question/<id>/reply.

import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const env = process.env;
const POLL_MS = 400;

function writeAtomic(file, body) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const temp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(temp, body);
  fs.renameSync(temp, file);
}

function report(state, detail) {
  if (!env.RUDDER_OPENCODE_SIGNAL) return;
  try {
    writeAtomic(env.RUDDER_OPENCODE_SIGNAL, JSON.stringify(detail ? { state, detail } : { state }));
  } catch {
    // The dashboard falls back to process exit; never break the agent over this.
  }
}

// A permission as the phone's card shows Claude's: Bash with its command,
// Edit with the lines it takes out and puts in, anything else as its fields.
function permissionAsk(request) {
  const meta = request.metadata && typeof request.metadata === "object" ? request.metadata : {};
  const patterns = Array.isArray(request.patterns) ? request.patterns : [];
  const always = Array.isArray(request.always) ? request.always.filter((value) => typeof value === "string") : [];
  if (request.permission === "bash") {
    return { tool: "Bash", input: { command: String(meta.command ?? patterns.join("\n")), description: meta.description ?? undefined }, suggestions: always };
  }
  if (request.permission === "edit" && typeof meta.filepath === "string") {
    const old = [];
    const added = [];
    for (const line of String(meta.diff ?? "").split("\n")) {
      if (line.startsWith("---") || line.startsWith("+++")) continue;
      if (line.startsWith("-")) old.push(line.slice(1));
      else if (line.startsWith("+")) added.push(line.slice(1));
    }
    return { tool: "Edit", input: { file_path: meta.filepath, old_string: old.join("\n"), new_string: added.join("\n") }, suggestions: always };
  }
  const name = String(request.permission || "tool");
  return { tool: name.charAt(0).toUpperCase() + name.slice(1), input: { patterns, ...meta }, suggestions: always };
}

// A question as Claude's AskUserQuestion, which the phone's card answers.
function questionAsk(request) {
  const questions = (Array.isArray(request.questions) ? request.questions : []).map((q) => ({
    question: String(q?.question ?? ""),
    header: String(q?.header ?? ""),
    options: (Array.isArray(q?.options) ? q.options : []).map((o) => ({ label: String(o?.label ?? ""), description: String(o?.description ?? "") })),
    multiSelect: q?.multiple === true,
  }));
  return { tool: "AskUserQuestion", input: { questions }, suggestions: [] };
}

// The phone's answer (cloud/worker/asks.mjs askDecision, Claude's hook
// output) as OpenCode's reply: [url, body].
function replyFor(kind, requestId, ask, answer) {
  const decision = JSON.parse(answer)?.hookSpecificOutput?.decision ?? {};
  if (kind === "question") {
    const given = decision.updatedInput?.answers ?? {};
    const answers = ask.input.questions.map((q) => {
      const value = String(given[q.question] ?? "").trim();
      if (!value) return [];
      return q.multiSelect ? value.split(", ").filter(Boolean) : [value];
    });
    return [`/question/${requestId}/reply`, { answers }];
  }
  if (decision.behavior === "allow") {
    return [`/permission/${requestId}/reply`, { reply: Array.isArray(decision.updatedPermissions) ? "always" : "once" }];
  }
  const body = { reply: "reject" };
  if (typeof decision.message === "string" && decision.message) body.message = decision.message;
  return [`/permission/${requestId}/reply`, body];
}

export const RudderAgent = async ({ client }) => {
  // OpenCode can load a plugin twice in one process; one is enough.
  if (globalThis.rudderAgentLoaded) return {};
  globalThis.rudderAgentLoaded = true;

  const post = (url, body) => client._client.post({ url, body, headers: { "Content-Type": "application/json" } });
  const parents = new Map(); // session -> whether it is a subagent's
  const busy = new Set(); // sessions mid-turn
  const waiting = new Map(); // OpenCode request id -> { file, answer, timer }
  let session = null;

  async function isRoot(sessionID) {
    if (!sessionID) return true;
    if (!parents.has(sessionID)) {
      let child = false;
      try {
        const result = await client.session.get({ path: { id: sessionID } });
        child = Boolean((result?.data ?? result)?.parentID);
      } catch {
        child = false; // Report rather than go silent.
      }
      parents.set(sessionID, child);
    }
    return !parents.get(sessionID);
  }

  function onSession(sessionID) {
    if (!sessionID || sessionID === session || !env.RUDDER_OPENCODE_SESSION_FILE) return;
    session = sessionID;
    try {
      writeAtomic(env.RUDDER_OPENCODE_SESSION_FILE, sessionID);
    } catch {
      // The run keeps the session it has.
    }
  }

  let prompted = !env.RUDDER_OPENCODE_PROMPT;
  async function sendPrompt(sessionID) {
    if (prompted || !sessionID) return;
    prompted = true;
    const model = env.RUDDER_OPENCODE_MODEL || "";
    const slash = model.indexOf("/");
    const body = { parts: [{ type: "text", text: env.RUDDER_OPENCODE_PROMPT }], agent: env.RUDDER_OPENCODE_AGENT || "build" };
    if (slash > 0) body.model = { providerID: model.slice(0, slash), modelID: model.slice(slash + 1) };
    if (env.RUDDER_OPENCODE_VARIANT) body.variant = env.RUDDER_OPENCODE_VARIANT;
    try {
      await post(`/session/${sessionID}/prompt_async`, body);
    } catch {
      report("error", "could not send the first message");
    }
  }
  if (env.RUDDER_OPENCODE_SESSION) {
    // After the plugin loads: OpenCode's server answers once it has.
    setTimeout(() => void sendPrompt(env.RUDDER_OPENCODE_SESSION), 0);
  }

  function ask(kind, request) {
    const dir = env.RUDDER_OPENCODE_ASKS;
    if (!dir || !request?.id || waiting.has(request.id)) return;
    const shown = kind === "question" ? questionAsk(request) : permissionAsk(request);
    const hash = createHash("sha256").update(String(request.id)).digest("hex").slice(0, 8);
    const id = `${Date.now()}-${hash}`;
    const file = path.join(dir, `${id}.json`);
    const answerFile = path.join(dir, `${id}.answer.json`);
    try {
      writeAtomic(file, JSON.stringify({ id, ts: Date.now(), pid: process.pid, ...shown }));
    } catch {
      return;
    }
    const entry = { file, answerFile, timer: null };
    entry.timer = setInterval(() => {
      let answer;
      try {
        answer = fs.readFileSync(answerFile, "utf8");
      } catch {
        return;
      }
      settle(request.id);
      try {
        const [url, body] = replyFor(kind, request.id, shown, answer);
        void post(url, body).catch(() => report("error", "could not pass on the answer"));
      } catch {
        report("error", "could not read the answer");
      }
    }, POLL_MS);
    waiting.set(request.id, entry);
  }

  // Answered here or in the terminal: the card goes.
  function settle(requestId) {
    const entry = waiting.get(requestId);
    if (!entry) return;
    waiting.delete(requestId);
    clearInterval(entry.timer);
    for (const file of [entry.file, entry.answerFile]) {
      try {
        fs.rmSync(file, { force: true });
      } catch {
        // already gone
      }
    }
  }

  return {
    event: async ({ event }) => {
      const type = event?.type;
      const props = event?.properties ?? {};
      const sessionID = props.sessionID;
      if (type === "session.created") {
        const parent = props.info?.parentID;
        parents.set(props.info?.id, Boolean(parent));
        if (!parent && !env.RUDDER_OPENCODE_SESSION) await sendPrompt(props.info?.id);
      } else if (type === "session.status") {
        const state = props.status?.type;
        if (!(await isRoot(sessionID))) return;
        onSession(sessionID);
        // A turn starts once; OpenCode says busy several times during one.
        if (state === "busy" && !busy.has(sessionID)) {
          busy.add(sessionID);
          report("working");
        } else if (state === "idle") {
          busy.delete(sessionID);
        }
      } else if (type === "session.idle") {
        if (await isRoot(sessionID)) {
          onSession(sessionID);
          report("done");
        }
      } else if (type === "session.error") {
        const error = props.error;
        // An interrupt (Esc Esc) ends the turn; the idle that follows says so.
        if (error?.name === "MessageAbortedError") return;
        if (await isRoot(sessionID)) {
          report("error", String(error?.name ?? error?.data?.message ?? "error").slice(0, 80));
        }
      } else if (type === "permission.asked") {
        // A subagent's ask holds up the whole turn too.
        report("permission");
        ask("permission", props);
      } else if (type === "question.asked") {
        report("input");
        ask("question", props);
      } else if (type === "permission.replied" || type === "question.replied" || type === "question.rejected") {
        settle(props.requestID);
        report("working");
      }
    },
  };
};
