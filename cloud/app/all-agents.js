// "All agents": one chat that talks to every agent in every project, opened
// from the top of the sidebar (desktop) or the home screen's top bar (phone).
// Behind it is an ordinary agent in a hidden project the server opens on
// first use (`state.coordinator`, POST /api/app/coordinator), with tools over
// all the others (cloud/worker/coordinator.mjs). So once it exists its chat
// is the usual agent screen at /app/all/<ws>/<run>. This module is the button and
// the page before the first message: what it does, the model it starts on,
// and a composer that starts it with `/main <message>`.
import { h, icon, setText } from "/app/render.js";
import { api, coarse, track } from "/app/core.js";
import { modelChip } from "/app/model-picker.js";

export const ALL_PATH = "/app/all";
export const allAgentPath = (ws, run) => `${ALL_PATH}/${encodeURIComponent(ws)}/${encodeURIComponent(run)}`;

const coordinatorAgents = (state) => (state?.coordinator?.agents || []).filter((a) => a && !a.pending);

/** The newest "All agents" chat, or null before the first one. */
export function latestCoordinatorRun(state) {
  const newest = coordinatorAgents(state).sort((a, b) => (b.createdAt ?? 0) - (a.createdAt ?? 0))[0];
  return newest?.id || null;
}

/**
 * The button. `paint(state, on)` keeps its dot (working, or waiting on you)
 * and its highlight in step.
 */
export function allAgentsButton({ variant }) {
  const el = h(`<a class="all-btn ${variant}" href="${ALL_PATH}" data-link title="One chat with every agent in every project">${icon("agents")}<span class="all-l">All agents</span><span class="all-dot" hidden><span class="dot"></span></span></a>`);
  const dotWrap = el.querySelector(".all-dot");
  return {
    el,
    paint(state, on) {
      el.classList.toggle("on", !!on);
      if (on) el.setAttribute("aria-current", "page"); else el.removeAttribute("aria-current");
      const latest = coordinatorAgents(state).find((a) => a.id === latestCoordinatorRun(state));
      const needs = !!latest?.asks?.length || latest?.status === "waiting";
      const working = latest?.status === "working" || latest?.status === "starting";
      dotWrap.hidden = !needs && !working;
      dotWrap.className = `all-dot ${needs ? "s-needs" : "s-working"}`;
      dotWrap.querySelector(".dot").classList.toggle("live", working && !needs);
      el.setAttribute("aria-label", needs ? "All agents, needs you" : working ? "All agents, working" : "All agents");
    },
  };
}

// The chat's own tools (cloud/worker/coordinator.mjs) as people read them.
// Claude and Codex both name them mcp__all_agents__<tool>.
const TOOL_LABELS = {
  list_agents: "List agents",
  read_agent: "Read agent",
  send_message: "Send message",
  interrupt_agent: "Interrupt agent",
  start_agent: "Start agent",
  answer_prompt: "Answer prompt",
  wait_for_events: "Wait for events",
};
const oneLine = (text) => String(text ?? "").replace(/\s+/g, " ").trim();

/** A tool call of the "All agents" chat as its card shows it, or null for any other tool. */
export function coordinatorTool(tool) {
  // Codex in code mode calls them from a script in its `exec` tool, which the
  // transcript names by the tool it calls (worker/transcripts.mjs), without arguments.
  const scripted = tool?.name === "exec" ? /^mcp__all_agents__([a-z_]+)$/.exec(String(tool.input || "").trim())?.[1] : null;
  if (scripted) return Object.hasOwn(TOOL_LABELS, scripted) ? { name: TOOL_LABELS[scripted], input: "" } : null;
  const which = /^mcp__all_agents__([a-z_]+)$/.exec(String(tool?.name || ""))?.[1];
  if (!which || !Object.hasOwn(TOOL_LABELS, which)) return null;
  // The transcript keeps the arguments as JSON, or one picked field (a start's prompt).
  let args = null;
  try { args = JSON.parse(tool.input || ""); } catch { args = null; }
  if (!args || typeof args !== "object") return { name: TOOL_LABELS[which], input: oneLine(tool.input) };
  const where = (a) => [a.project, a.agent].filter(Boolean).join(" / ");
  const input = {
    list_agents: () => args.project || "every project",
    read_agent: () => where(args),
    send_message: () => `${where(args)}: ${args.text ?? ""}`,
    interrupt_agent: () => where(args),
    start_agent: () => `${args.project ?? ""}: ${args.prompt ?? ""}`,
    answer_prompt: () => where(args),
    wait_for_events: () => (Array.isArray(args.agents) && args.agents.length ? args.agents.join(", ") : args.project || "every agent"),
  }[which]();
  return { name: TOOL_LABELS[which], input: oneLine(input) };
}

const EXAMPLES = [
  "What is every agent working on?",
  "Which agents need me right now?",
  "Tell me when the agents working now finish.",
];

// How long sending waits on each step before it says so with Try again: a
// request to the cloud machine, the hidden project's dashboard starting (a
// sleeping machine wakes first), and the new chat showing up.
const OPEN_MS = 60_000;
const READY_MS = 90_000;
const CHAT_MS = 60_000;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** `promise`, or an error saying `late` when `ms` pass first. */
function within(promise, ms, late) {
  let timer;
  const timeout = new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(late)), ms); });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/**
 * /app/all: the newest chat when there is one (replaces itself with it), or
 * the start page. /app/all/new (`fresh`) always shows the start page.
 */
export function AllAgentsScreen({ fresh, store, createComposer, navigate, goBack, refreshState }) {
  const el = h(`<section class="screen all-screen">
    <header class="topbar">
      <a class="icon-btn back" href="/app" aria-label="Back to projects">${icon("back")}</a>
      <div class="titles"><div class="title">All agents</div><div class="subtitle"><span class="st">Every agent, every project</span></div></div>
    </header>
    <div class="scroller has-composer"><div class="all-intro">
      <div class="all-mark">${icon("agents")}</div>
      <h2>Talk to all your agents at once</h2>
      <p>Ask what they are doing, send any of them a message, start new work, and hear when they finish or need you. It reads and steers your agents. It does not edit your projects itself.</p>
      <div class="all-examples" role="list"></div>
      <div class="all-status" role="status" hidden></div>
    </div></div></section>`);
  const bar = el.querySelector(".topbar");
  const statusEl = el.querySelector(".all-status");
  el.querySelector(".back").addEventListener("click", (e) => { e.preventDefault(); goBack("/app"); });

  // The hidden project, opened on first use; every send waits for it.
  let ensured = null;
  const ensure = () => {
    if (store.state?.coordinator?.workspaceId) return Promise.resolve(store.state.coordinator.workspaceId);
    ensured ??= api("/api/app/coordinator", { method: "POST", body: {} })
      .then(async (res) => { await refreshState(); return res.workspace; })
      .catch((err) => { ensured = null; throw err; });
    return ensured;
  };

  const composer = createComposer({
    placeholder: "Message all your agents",
    draftKey: "msg:all-agents",
    hint: coarse ? "" : "Enter to send · Shift+Enter for a new line",
    onSubmit: (text) => start(text),
  });
  el.appendChild(composer.el);
  const tools = h(`<div class="c-tools"></div>`);
  composer.el.querySelector("form").before(tools);
  let chip = null;
  const paintChip = (ws) => {
    if (chip || !ws) return;
    // The model the chat starts on: the hidden project's model for new agents.
    chip = modelChip(ws);
    tools.appendChild(chip.el);
  };

  const examples = el.querySelector(".all-examples");
  for (const text of EXAMPLES) {
    const b = h(`<button type="button" class="all-ex" role="listitem"></button>`);
    setText(b, text);
    b.addEventListener("click", () => {
      composer.textarea.value = text;
      composer.textarea.dispatchEvent(new Event("input"));
      composer.textarea.focus();
    });
    examples.appendChild(b);
  }

  // One line under the intro: what sending is doing, or what went wrong with
  // Try again (which sends the composer's text again).
  const say = (text, failed = false) => {
    statusEl.hidden = !text;
    statusEl.classList.toggle("err", failed);
    statusEl.replaceChildren(text || "");
    if (failed) {
      const again = h(`<button type="button" class="all-retry">Try again</button>`);
      again.addEventListener("click", () => composer.el.querySelector("form").requestSubmit());
      statusEl.append(" ", again);
    }
    // On a phone the intro runs past the composer.
    if (text) statusEl.scrollIntoView({ block: "nearest" });
  };
  let waitingFor = null; // ids that existed before our first message
  let gone = false;
  let autoOpened = false;

  // Refreshes the state, then once a second, until `done()` is true, for at
  // most `ms`; a state request that hangs does not hold it past that. The
  // first refresh means Try again never acts on the state that failed.
  async function until(done, ms, late) {
    const deadline = Date.now() + ms;
    for (;;) {
      await Promise.race([refreshState(), sleep(5000)]);
      if (done()) return;
      if (gone) throw new Error("left the page");
      if (Date.now() >= deadline) throw new Error(late);
      await sleep(1000);
    }
  }

  const projectReady = () => {
    const project = store.state?.coordinator;
    if (project?.dashboard === "failed") throw new Error(project.failureReason || "The All agents chat could not start on the cloud machine.");
    return project?.dashboard === "running";
  };

  async function start(text) {
    if (!text) return false;
    let sent = false;
    try {
      say("Starting the chat…");
      const ws = await within(ensure(), OPEN_MS, "The cloud machine did not open the All agents chat.");
      paintChip(ws);
      await until(projectReady, READY_MS, "The cloud machine did not finish setting up the All agents chat.");
      waitingFor = new Set(coordinatorAgents(store.state).map((a) => a.id));
      // `/main`: one agent in the hidden project's own checkout, on its model.
      await within(api("/api/app/task", { method: "POST", body: { workspace: ws, text: `/main ${text}` } }), OPEN_MS, "The cloud machine did not take the message.");
      sent = true;
      track("all_agents_started", { examples: EXAMPLES.includes(text) });
      // update() opens the new chat (and marks the page gone) once the state shows it.
      await until(() => gone, CHAT_MS, "The chat did not open on the cloud machine.");
      return true;
    } catch (err) {
      waitingFor = null;
      // Left the page: the draft stays unless the message went in.
      if (gone) return sent;
      if (err.status === 401) say("");
      else say(err.message, true);
      return false;
    }
  }

  function update() {
    paintChip(store.state?.coordinator?.workspaceId);
    // The page opens the project, so the model can be picked before the first message.
    if (store.state && !store.state.coordinator && !autoOpened) {
      autoOpened = true;
      ensure().catch(() => {});
    }
    const started = waitingFor && coordinatorAgents(store.state).find((a) => !waitingFor.has(a.id));
    const target = started?.id || (!fresh && !waitingFor ? latestCoordinatorRun(store.state) : null);
    if (target && !gone) {
      gone = true;
      // Not from inside a repaint: the replacement mounts a new screen.
      setTimeout(() => navigate(allAgentPath(store.state.coordinator.workspaceId, target), { replace: true }), 0);
    }
  }

  return {
    el,
    update,
    destroy() { gone = true; },
  };
}
