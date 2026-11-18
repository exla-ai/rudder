// "All agents": one chat that talks to every agent in every project, opened
// from the top of the sidebar (desktop) or the home screen's top bar (phone).
// Behind it is an ordinary agent in a hidden project the server opens on
// first use (`state.coordinator`, POST /api/app/coordinator), with tools over
// all the others (cloud/worker/coordinator.mjs). So once it exists its chat
// is the usual agent screen at /app/all/<ws>/<run>. This module is the button and
// the page before the first message: what it does, the model it starts on,
// and a composer that starts it with `/main <message>`.
import { h, icon, setText } from "/app/render.js";
import { api, coarse, toast, track } from "/app/core.js";
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

  const say = (text) => { statusEl.hidden = !text; setText(statusEl, text || ""); };
  let waitingFor = null; // ids that existed before our first message
  let gone = false;
  let autoOpened = false;

  async function dashboardReady() {
    for (let i = 0; i < 45 && !gone; i += 1) {
      const dash = store.state?.coordinator?.dashboard;
      if (dash === "running") return;
      if (dash === "failed") throw new Error(store.state.coordinator.failureReason || "The All agents chat could not start on the cloud machine");
      await new Promise((resolve) => setTimeout(resolve, 1000));
      await refreshState();
    }
    if (!gone) throw new Error("The cloud machine is still setting up the All agents chat. Try again in a moment.");
  }

  async function start(text) {
    if (!text) return false;
    try {
      say("Starting the chat…");
      const ws = await ensure();
      paintChip(ws);
      await dashboardReady();
      waitingFor = new Set(coordinatorAgents(store.state).map((a) => a.id));
      // `/main`: one agent in the hidden project's own checkout, on its model.
      await api("/api/app/task", { method: "POST", body: { workspace: ws, text: `/main ${text}` } });
      track("all_agents_started", { examples: EXAMPLES.includes(text) });
      await refreshState();
      update();
      return true;
    } catch (err) {
      waitingFor = null;
      say("");
      if (err.status !== 401) toast(err.message, { error: true });
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
