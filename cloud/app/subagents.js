// Subagent cards. A Claude Code Agent call or a Codex spawn_agent call in the
// chat carries a `subagent` (worker/transcripts.mjs): what it was asked, how it
// is doing, how long it has run, how many tools it used and what it answered.
// The card opens to show the subagent's own messages and tool calls, and the
// subagents it started in turn.
//
// The top card's open state is the chat's (app.js toggles it from the
// `.tool-row` click and repaints through paintTool). Cards inside a card keep
// their own on the element, so they stay open across polls while their parent is.
// The subagent tray (subagent-tray.js) says the same of each (subagentFacts)
// and shows the same activity for the one it has open (paintSubagentActivity).

import { icon, toolIcon, h, setText, reconcile, renderMarkdown, markdownReady } from "/app/render.js";

export const SUBAGENT_STATUS = {
  running: { label: "Running", cls: "s-working", live: true },
  done: { label: "Done", cls: "s-done" },
  failed: { label: "Failed", cls: "s-failed" },
  stopped: { label: "Stopped", cls: "s-idle" },
};

/** Paint a chat tool box (`.tool`) whose message started a subagent. */
export function paintSubagent(box, m, open) {
  if (!box.classList.contains("subagent")) build(box, true);
  paint(box, m, open);
}

function build(box, top) {
  box.classList.add("subagent");
  // A call shown as a plain tool row until its subagent's log turned up.
  box.querySelector(":scope > .tool-out")?.remove();
  const row = box.querySelector(".sa-row") || box.querySelector(".tool-row");
  // The top card's row stays the chat's `.tool-row` so the chat opens it.
  row.className = top ? "tool-row sa-row" : "sa-row";
  row.innerHTML = `<span class="t-icon">${icon("agents")}</span>
    <span class="sa-main">
      <span class="sa-title"></span>
      <span class="sa-meta"><span class="sa-state"><span class="dot"></span><span class="sa-label"></span></span><span class="sa-facts"></span></span>
      <span class="sa-line"></span>
    </span>${icon("chevron").replace("<svg", '<svg class="t-caret"')}`;
  const body = h(BODY);
  body.hidden = true;
  box.appendChild(body);
  // Cards nested in this one open and close here; app.js never sees them.
  if (top) box.addEventListener("click", toggleNested);
}

const BODY = `<div class="sa-body">
    <details class="sa-prompt" hidden><summary>${icon("chevron")}Prompt</summary><div class="md"></div></details>
    <div class="sa-earlier" hidden></div>
    <div class="sa-msgs"></div>
    <div class="sa-empty" hidden></div>
  </div>`;

function toggleNested(e) {
  const inner = e.target.closest(".sa-row:not(.tool-row)");
  if (!inner) return;
  const card = inner.closest(".subagent");
  card._open = !card._open;
  paint(card, card._msg, card._open);
}

/** Paint a subagent's activity (its prompt, messages, tool calls and the subagents it started) into `container`. */
export function paintSubagentActivity(container, s) {
  let body = container.querySelector(":scope > .sa-body");
  if (!body) {
    body = h(BODY);
    container.appendChild(body);
  }
  if (!container._saNested) {
    container._saNested = true;
    container.addEventListener("click", toggleNested);
  }
  const sig = activitySig(s);
  if (body._saSig === sig) return;
  body._saSig = sig;
  paintBody(body, s);
}

// What the activity shows changed. Subagents of this one change without its
// own messages changing.
function activitySig(s) {
  const msgs = s.messages || [];
  const last = msgs[msgs.length - 1];
  const nested = msgs.filter((x) => x.subagent).map((x) => `${x.subagent.status}.${x.subagent.toolCalls}.${x.subagent.messages?.length}`).join(",");
  return [s.status, s.toolCalls, msgs.length, last?.id, last?.tool?.output?.length, last?.text?.length, s.result?.length, s.prompt?.length, s.earlier, nested, markdownReady()].join("|");
}

/**
 * What a subagent's card says of it, in the chat and in the tray alike: its
 * status, title, kind and name (`who`), how long it has run or ran, and how
 * many tools it used.
 */
export function subagentFacts(s, now) {
  const st = SUBAGENT_STATUS[s.status] || SUBAGENT_STATUS.stopped;
  const elapsed = s.startedAt == null ? null
    : s.status === "running" ? now - s.startedAt
    : s.endedAt != null ? s.endedAt - s.startedAt : null;
  const title = s.description || s.name || s.kind || "Subagent";
  const who = [s.kind, s.name].filter((x, i, all) => x && x !== title && all.indexOf(x) === i).join(" · ");
  const calls = s.toolCalls || s.status === "running" ? `${s.toolCalls} tool ${s.toolCalls === 1 ? "call" : "calls"}` : "";
  return { st, title, who, time: elapsed == null ? "" : duration(elapsed), calls };
}

function paint(box, m, open) {
  const s = m.subagent;
  box._msg = m;
  const { st, title, who, time, calls } = subagentFacts(s, Date.now());
  const facts = [who, time, calls].filter(Boolean);
  const msgs = s.messages || [];
  const last = msgs[msgs.length - 1];
  const sig = [activitySig(s), time, open].join("|");
  if (box._saSig === sig) return;
  box._saSig = sig;

  box.classList.toggle("open", open);
  for (const c of Object.values(SUBAGENT_STATUS)) box.classList.remove(c.cls);
  box.classList.add(st.cls);
  const row = box.querySelector(".sa-row");
  row.setAttribute("aria-expanded", String(open));
  row.setAttribute("aria-label", `Subagent: ${title}. ${st.label}. ${facts.join(", ")}. ${open ? "Hide" : "Show"} its work`);
  setText(row.querySelector(".sa-title"), title);
  row.querySelector(".dot").classList.toggle("live", !!st.live);
  setText(row.querySelector(".sa-label"), st.label);
  // On a phone the facts wrap between items, never inside one.
  setText(row.querySelector(".sa-facts"), facts.map((f) => f.replace(/ /g, "\u00a0")).join(" · "));
  // Closed, the card says what it is doing now, or what it came back with.
  const line = row.querySelector(".sa-line");
  const text = open ? "" : s.status === "running" ? activity(last) : oneLine(s.result || "");
  setText(line, text);
  line.hidden = !text;
  line.classList.toggle("sa-result", !open && s.status !== "running");

  const body = box.querySelector(":scope > .sa-body");
  body.hidden = !open;
  if (open) paintBody(body, s);
}

function paintBody(body, s) {
  const msgs = s.messages || [];
  const prompt = body.querySelector(":scope > .sa-prompt");
  prompt.hidden = !s.prompt;
  if (s.prompt) {
    const md = prompt.querySelector(".md");
    const psig = `${s.prompt}|${markdownReady()}`;
    if (md.dataset.sig !== psig) { renderMarkdown(md, s.prompt); md.dataset.sig = psig; }
  }
  const earlier = body.querySelector(":scope > .sa-earlier");
  earlier.hidden = !s.earlier;
  setText(earlier, s.earlier ? `${s.earlier} earlier ${s.earlier === 1 ? "step" : "steps"} not shown` : "");
  const empty = body.querySelector(":scope > .sa-empty");
  empty.hidden = msgs.length > 0;
  setText(empty, msgs.length ? "" : s.status === "running" ? "Starting" : "Nothing recorded");
  // A call becomes a card once its subagent's log turns up: a new node.
  reconcile(body.querySelector(":scope > .sa-msgs"), msgs, (x) => (x.subagent ? `sa:${x.id}` : x.id), createInner, updateInner);
}

function createInner(x) {
  if (x.role === "tool" && x.subagent) {
    const card = h(`<div class="tool subagent sa-nested"><button type="button" class="sa-row"></button></div>`);
    build(card, false);
    return card;
  }
  if (x.role === "tool") {
    return h(`<details class="sa-tool"><summary><span class="t-icon"></span><span class="t-name"></span><span class="t-input"></span>${icon("chevron").replace("<svg", '<svg class="t-caret"')}</summary><pre></pre></details>`);
  }
  if (x.kind === "compact") return h(`<div class="sa-compact"></div>`);
  if (x.role === "user") return h(`<div class="sa-user"><div class="sa-label">Message</div><div class="md"></div></div>`);
  if (x.role === "system") return h(`<div class="sa-note md"></div>`);
  return h(`<div class="sa-text md"></div>`);
}

function updateInner(node, x) {
  if (x.role === "tool" && x.subagent) return paint(node, x, !!node._open);
  if (x.role === "tool") {
    const tool = x.tool || {};
    const sig = `${tool.name}|${tool.input}|${tool.output?.length}|${tool.isError}`;
    if (node.dataset.sig === sig) return;
    node.dataset.sig = sig;
    node.classList.toggle("error", !!tool.isError);
    node.querySelector(".t-icon").innerHTML = icon(tool.isError ? "alert" : toolIcon(tool.name));
    setText(node.querySelector(".t-name"), tool.name || "Tool");
    setText(node.querySelector(".t-input"), (tool.input || "").replace(/\s+/g, " ").trim());
    const pre = node.querySelector("pre");
    pre.textContent = tool.output || "No output";
    pre.classList.toggle("sa-none", !tool.output);
    return;
  }
  if (x.kind === "compact") return setText(node, "Context compacted");
  const md = node.classList.contains("md") ? node : node.querySelector(".md");
  const sig = `${x.text}|${markdownReady()}`;
  if (md.dataset.sig !== sig) { renderMarkdown(md, x.text || ""); md.dataset.sig = sig; }
}

// What a running subagent is doing: its newest tool call, or what it last said.
function activity(m) {
  if (!m) return "Starting";
  if (m.role === "tool") return `${m.tool?.name || "Tool"} ${(m.tool?.input || "").replace(/\s+/g, " ").trim()}`.trim();
  return oneLine(m.text || "");
}

function oneLine(text) {
  return String(text).replace(/[#*_`>]+/g, "").replace(/\s+/g, " ").trim();
}

/** `42s`, `3m 05s`, `2h 10m`: how long a subagent has run, or ran. */
export function duration(ms) {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, "0")}s`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
}
