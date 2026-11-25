// The chat's Running strip: what the agent started on purpose and still has
// running, above the composer (app.js AgentScreen). It reads the newest
// transcript page, where the worker marks a call `running`
// (worker/transcripts.mjs markRunning): a call with no result yet in the turn
// under way, a Codex command that outlived its call, or a background task (a
// Claude Code background Bash or Monitor, an Antigravity command or timer) that
// has not ended. Calls that only wait on or read another (`tool.poll`) are
// left out. Collapsed, it says how many run and the newest one's purpose and
// time. Tapped, it lists a row each, newest first: the purpose, how long, and
// the command itself in mono, whole once its row is tapped. Finished ones drop
// out, and the strip hides when nothing runs.

import { h, icon, reconcile, setText, toolIcon } from "/app/render.js";
import { duration } from "/app/subagents.js";

export function mountRunningStrip() {
  const el = h(`<section class="run-strip" hidden aria-label="Running now">
    <button type="button" class="rs-head" aria-expanded="false">
      <span class="spin rs-spin" aria-hidden="true"></span>
      <span class="rs-count"></span>
      <span class="rs-desc"></span>
      <span class="rs-time"></span>
      ${icon("chevron").replace("<svg", '<svg class="rs-caret" aria-hidden="true"')}
    </button>
    <div class="rs-list" hidden></div>
  </section>`);
  const head = el.querySelector(".rs-head");
  const list = el.querySelector(".rs-list");
  let open = false;
  let paused = false;
  let items = [];
  // Rows showing their whole command, by call id.
  const whole = new Set();
  let timer = null;

  head.addEventListener("click", () => {
    open = !open;
    paint();
  });
  list.addEventListener("click", (e) => {
    const row = e.target.closest(".rs-row");
    if (!row) return;
    const id = row.dataset.key;
    if (whole.has(id)) whole.delete(id);
    else whole.add(id);
    paint();
  });
  el.addEventListener("keydown", (e) => {
    if (e.key !== "Escape" || !open || e.defaultPrevented) return;
    e.preventDefault();
    open = false;
    paint();
    head.focus();
  });

  function paint() {
    const now = Date.now();
    el.hidden = paused || !items.length;
    if (el.hidden) {
      open = false;
      stopTimer();
      return;
    }
    const newest = items[0];
    el.classList.toggle("open", open);
    head.setAttribute("aria-expanded", String(open));
    head.setAttribute("aria-label", `${items.length} running. Newest: ${newest.desc}. ${open ? "Hide the list" : "Show the list"}`);
    setText(head.querySelector(".rs-count"), `${items.length} running`);
    setText(head.querySelector(".rs-desc"), newest.desc);
    const time = head.querySelector(".rs-time");
    time.dataset.since = String(newest.since);
    setText(time, elapsed(newest.since, now));
    list.hidden = !open;
    if (open) {
      for (const id of whole) if (!items.some((x) => x.id === id)) whole.delete(id);
      reconcile(list, items, (x) => x.id, createRow, (node, x) => paintRow(node, x, now));
    } else list.replaceChildren();
    startTimer();
  }

  // One timer keeps every elapsed time current while the strip shows.
  function startTimer() {
    timer ??= setInterval(() => {
      const now = Date.now();
      for (const t of el.querySelectorAll("[data-since]")) setText(t, elapsed(Number(t.dataset.since), now));
    }, 1000);
  }
  function stopTimer() {
    clearInterval(timer);
    timer = null;
  }

  return {
    el,
    // The newest transcript page's messages (null before the first read).
    update(messages) {
      items = runningItems(messages || []);
      paint();
    },
    // A closed chat (AgentScreen): nothing shows until it reopens.
    pause(closed) {
      paused = closed;
      paint();
    },
    destroy() {
      stopTimer();
    },
  };

  function createRow() {
    return h(`<button type="button" class="rs-row">
      <span class="rs-r-top"><span class="rs-r-desc"></span><span class="rs-r-bg" hidden>Background</span><span class="rs-r-time"></span></span>
      <code class="rs-r-cmd"></code>
    </button>`);
  }

  function paintRow(node, x, now) {
    const full = whole.has(x.id);
    node.classList.toggle("whole", full);
    node.setAttribute("aria-expanded", String(full));
    node.setAttribute("aria-label", `${x.desc}${x.background ? ", in the background" : ""}. ${full ? "Shorten the command" : "Show the whole command"}`);
    setText(node.querySelector(".rs-r-desc"), x.desc);
    node.querySelector(".rs-r-bg").hidden = !x.background;
    const time = node.querySelector(".rs-r-time");
    time.dataset.since = String(x.since);
    setText(time, elapsed(x.since, now));
    const cmd = node.querySelector(".rs-r-cmd");
    // A short command is its own description.
    cmd.hidden = !x.command || x.command === x.desc;
    setText(cmd, x.command);
  }
}

// The calls running on the page, newest first. A question the agent asked
// (tool.ask) waits on you, and a subagent has its own tray.
function runningItems(messages) {
  return messages
    .filter((m) => m.role === "tool" && m.tool?.running && !m.tool.poll && !m.tool.ask && !m.subagent)
    .map((m) => {
      const tool = m.tool;
      const command = (tool.input || "").replace(/\s+/g, " ").trim();
      return {
        id: String(m.id),
        since: m.ts,
        background: !!tool.background,
        // Claude Code's `description`, Antigravity's `toolSummary` (worker/transcripts.mjs).
        desc: tool.description || (toolIcon(tool.name) === "terminal" ? shortCommand(command) : tool.name) || command,
        command,
      };
    })
    .sort((a, b) => b.since - a.since);
}

// A command with no purpose line of its own (Codex records none, and a
// Claude Bash call may have no description), as the strip names it: the first program it runs, past the steps
// that only set up the shell and variable assignments in front ("cd cloud &&
// FOO=1 npm test | tail" is "npm test"). The row's second line has the whole
// command.
function shortCommand(command) {
  const steps = shellSteps(command);
  const main = steps.find((step) => !/^(?:cd|export|unset|source|\.)\s/.test(step)) || steps[0] || command;
  return main.replace(/^(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)+/, "");
}

// A command line's steps: split at &&, ||, | and ; outside quotes and
// $(...), so `unset $(env | grep X) && npm test` is two steps.
function shellSteps(command) {
  const steps = [];
  let step = "";
  let quote = "";
  let depth = 0;
  for (let i = 0; i < command.length; i += 1) {
    const c = command[i];
    if (c === "\\" && quote !== "'") {
      step += command.slice(i, i + 2);
      i += 1;
    } else if (quote) {
      step += c;
      if (c === quote) quote = "";
    } else if (c === "'" || c === '"' || c === "`") {
      quote = c;
      step += c;
    } else if (depth === 0 && /^(?:&&|\|\|)$/.test(command.slice(i, i + 2))) {
      steps.push(step);
      step = "";
      i += 1;
    } else if (depth === 0 && (c === "|" || c === ";")) {
      steps.push(step);
      step = "";
    } else {
      if (c === "(") depth += 1;
      if (c === ")") depth = Math.max(0, depth - 1);
      step += c;
    }
  }
  steps.push(step);
  return steps.map((s) => s.trim()).filter(Boolean);
}

function elapsed(since, now) {
  return Number.isFinite(since) ? duration(now - since) : "";
}
