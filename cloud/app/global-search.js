// Search everything: Cmd+Shift+F on a Mac (Ctrl+Shift+F elsewhere) from
// anywhere in the app, text fields included, and the Mac app's Edit >
// Search Everything (`window.rudderSearchEverything`). A box over the page
// with the Cmd+K palette's look and keys (command-palette.js): agents and
// projects by name, from the app's state, and what was said in every chat on
// the cloud machine (GET /api/app/search, cloud/worker/transcript-search.mjs),
// each with its agent, project and time. A message opens its chat at it,
// the words found (chat-find.js). The same keys close it; not while another
// dialog is open.
//
// A cloud machine too old to search its chats still finds names, and says
// what Update now would add.
import { escapeHtml, h, icon, setText } from "/app/render.js";
import { rankCommands } from "/app/command-rank.js";
import { allAgentPath } from "/app/all-agents.js";
import { api, rel, track } from "/app/core.js";

const MAC = /Mac|iPhone|iPad|iPod/.test(navigator.platform);
const AGENTS_SHOWN = 12;
const PROJECTS_SHOWN = 6;
// Messages are searched once typing pauses, from this many characters.
const TYPING_MS = 250;
const MIN_CHARS = 2;
const WAKE_RETRY_MS = 5000;

const isSearchKey = (e) => !e.repeat && !e.altKey && e.shiftKey && e.key?.toLowerCase() === "f"
  && (MAC ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey);

let ctx = null;
let open = null;

/**
 * `state()` is the app's /api/app/state (null signed out), `navigate(path)`
 * moves the app, `openChat(path)` moves it to a chat and gives back that
 * chat's find, `agentPath` and `projectPath` are the app's routes and
 * `agentStatus(agent)` its status dot.
 */
export function mountGlobalSearch(options) {
  ctx = options;
  document.addEventListener("keydown", (e) => {
    if (!isSearchKey(e)) return;
    if (open?.dlg.isConnected) { e.preventDefault(); open.close(); return; }
    if (document.querySelector("dialog[open]") || !ctx.state()) return;
    e.preventDefault();
    open = openSearch();
  }, { capture: true });
  window.rudderSearchEverything = () => {
    if (!ctx.state()) return false;
    if (!open?.dlg.isConnected) {
      document.querySelector("dialog[open]")?.close();
      open = openSearch();
    }
    open.input.focus();
    return true;
  };
}

/** Opens it from a Cmd+K row. */
export function openGlobalSearch() {
  if (!ctx?.state() || open?.dlg.isConnected) return;
  open = openSearch();
}

function openSearch() {
  const dlg = h(`<dialog class="cmdk gs" aria-label="Search everything"><div class="cmdk-box">
    <label class="cmdk-in">${icon("search")}<input type="text" role="combobox" aria-expanded="true" aria-controls="gs-list" aria-autocomplete="list" autocomplete="off" autocapitalize="off" spellcheck="false" enterkeyhint="search" placeholder="Search agents, projects and messages" aria-label="Search agents, projects and messages"></label>
    <div class="cmdk-list" id="gs-list" role="listbox" aria-label="Results"></div>
    <div class="cmdk-foot" aria-hidden="true"><span>↑ ↓ to move</span><span>Enter to open</span><span>Esc to close</span></div>
  </div></dialog>`);
  const input = dlg.querySelector("input");
  const list = dlg.querySelector(".cmdk-list");
  // What the cloud machine said for the query: null before it answers.
  let messages = { q: "", results: null, partial: false, error: null, update: false, waking: false };
  let inflight = null;
  let timer = 0;
  let options = [];
  let active = 0;
  let closed = false;

  const name = (p) => p.name || p.repo || "Project";
  const query = () => input.value.replace(/\s+/g, " ").trim();
  const projects = () => {
    const s = ctx.state();
    return [...(s?.projects || []), ...(s?.coordinator ? [{ ...s.coordinator, coordinator: true }] : [])];
  };
  const chatPath = (p, a) => (p.coordinator ? allAgentPath(p.workspaceId, a.id) : ctx.agentPath(p.workspaceId, a.id));

  function nameRows(q) {
    if (!q) return { agents: [], projects: [] };
    const all = projects();
    const agents = all.flatMap((p) => (p.agents || []).filter((a) => !a.pending).map((a) => ({ p, a })))
      .sort((x, y) => (y.a.updatedAt || 0) - (x.a.updatedAt || 0))
      .map(({ p, a }) => ({ label: a.title || a.task || "Untitled agent", keywords: [p.coordinator ? "All agents" : name(p)], p, a }));
    const named = all.filter((p) => !p.coordinator).map((p) => ({ label: name(p), keywords: p.repo ? [p.repo] : [], p }));
    return { agents: rankCommands(agents, q).slice(0, AGENTS_SHOWN), projects: rankCommands(named, q).slice(0, PROJECTS_SHOWN) };
  }

  // The words found, marked in a line of text.
  function marked(text, q) {
    if (!q) return escapeHtml(text);
    const re = new RegExp(q.replace(/[\\^$.*+?()[\]{}|]/g, "\\$&"), "giu");
    let out = "";
    let at = 0;
    for (const m of text.matchAll(re)) {
      out += `${escapeHtml(text.slice(at, m.index))}<mark>${escapeHtml(m[0])}</mark>`;
      at = m.index + m[0].length;
    }
    return out + escapeHtml(text.slice(at));
  }

  function option(html, run) {
    const el = h(`<div class="cmdk-opt" role="option">${html}</div>`);
    el.id = `gs-o${options.length}`;
    el.dataset.i = String(options.length);
    options.push({ el, run });
    return el;
  }
  const head = (label) => h(`<div class="gs-h" role="presentation">${escapeHtml(label)}</div>`);
  const note = (text, cls = "") => { const el = h(`<div class="gs-note ${cls}" role="presentation"></div>`); setText(el, text); return el; };

  function fill({ keep = false } = {}) {
    const q = query();
    const was = keep ? options[active]?.key : null;
    options = [];
    const rows = [];
    if (!q) {
      rows.push(note("Agent and project names, and what you and your agents said in every chat."));
    }
    const { agents, projects: named } = nameRows(q);
    if (agents.length) {
      rows.push(head("Agents"));
      for (const { p, a, label } of agents) {
        const st = ctx.agentStatus(a);
        const el = option(`<span class="cmdk-ic" aria-hidden="true"><span class="dot ${st.cls}${st.live ? " live" : ""}"></span></span><span class="cmdk-t"><span class="cmdk-l"></span><span class="cmdk-d"></span></span>`,
          () => ctx.navigate(chatPath(p, a)));
        el.querySelector(".cmdk-l").innerHTML = marked(label, q);
        setText(el.querySelector(".cmdk-d"), `${p.coordinator ? "All agents" : name(p)} · ${st.label}`);
        options[options.length - 1].key = `a|${p.workspaceId}|${a.id}`;
        rows.push(el);
      }
    }
    if (named.length) {
      rows.push(head("Projects"));
      for (const { p, label } of named) {
        const el = option(`<span class="cmdk-ic" aria-hidden="true">${icon("code")}</span><span class="cmdk-t"><span class="cmdk-l"></span><span class="cmdk-d"></span></span>`,
          () => ctx.navigate(ctx.projectPath(p.workspaceId)));
        el.querySelector(".cmdk-l").innerHTML = marked(label, q);
        setText(el.querySelector(".cmdk-d"), p.repo && p.repo !== label ? p.repo : "");
        options[options.length - 1].key = `p|${p.workspaceId}`;
        rows.push(el);
      }
    }
    if (q.length >= MIN_CHARS) {
      rows.push(head("Messages"));
      rows.push(...messageRows(q));
    }
    if (q && !options.length && q.length < MIN_CHARS) rows.push(note("No matches"));
    list.replaceChildren(...rows);
    const again = was ? options.findIndex((o) => o.key === was) : -1;
    active = again >= 0 ? again : 0;
    if (!keep) list.scrollTop = 0;
    paintActive();
  }

  function messageRows(q) {
    const m = messages;
    if (m.update) {
      const el = option(`<span class="cmdk-ic" aria-hidden="true">${icon("refresh")}</span><span class="cmdk-t"><span class="cmdk-l">Update your cloud machine to search messages</span><span class="cmdk-d"></span></span>`, () => {
        void import("/app/home-update.js").then((mod) => mod.openUpdateSheet(m.error));
      });
      setText(el.querySelector(".cmdk-d"), m.error);
      options[options.length - 1].key = "update";
      return [el];
    }
    if (m.q !== q || (!m.results && !m.error)) return [note(m.waking ? "Waking the cloud machine to search messages" : "Searching messages", "gs-wait")];
    if (m.error) return [note(m.error, "gs-err")];
    const rows = [];
    for (const r of m.results) {
      const p = projects().find((x) => x.workspaceId === r.workspaceId);
      const a = p?.agents?.find((x) => x.id === r.runId);
      // An agent the app does not list (its project closed since) cannot open.
      if (!a || a.pending) continue;
      const title = p.coordinator ? "All agents" : a.title || a.task || "Untitled agent";
      const el = option(`<span class="cmdk-ic" aria-hidden="true">${icon(r.role === "user" ? "reply" : "agents")}</span><span class="cmdk-t"><span class="cmdk-l gs-snip"></span><span class="cmdk-d"></span></span>`,
        () => ctx.openChat(chatPath(p, a))?.reveal({ messageId: r.messageId, query: q }));
      el.classList.add("gs-msg");
      el.querySelector(".gs-snip").innerHTML = marked(r.snippet, q);
      const where = p.coordinator ? title : `${title} · ${name(p)}`;
      setText(el.querySelector(".cmdk-d"), [r.role === "user" ? "You" : null, where, rel(r.ts)].filter(Boolean).join(" · "));
      el.setAttribute("aria-label", `${r.role === "user" ? "You" : title}: ${r.snippet}. ${where}`);
      options[options.length - 1].key = `m|${r.workspaceId}|${r.runId}|${r.messageId}`;
      rows.push(el);
    }
    if (!rows.length) rows.push(note("No messages say that"));
    if (m.partial) rows.push(note("The oldest chats were not searched in time. More words find it faster.", "gs-wait"));
    return rows;
  }

  function paintActive() {
    options.forEach((o, i) => o.el.setAttribute("aria-selected", String(i === active)));
    const opt = options[active]?.el;
    if (opt) {
      input.setAttribute("aria-activedescendant", opt.id);
      opt.scrollIntoView({ block: "nearest" });
    } else input.removeAttribute("aria-activedescendant");
  }

  async function searchMessages() {
    const q = query();
    inflight?.abort();
    inflight = null;
    clearTimeout(timer);
    if (q.length < MIN_CHARS) return;
    const controller = new AbortController();
    inflight = controller;
    try {
      const res = await api(`/api/app/search?q=${encodeURIComponent(q)}`, { signal: controller.signal });
      if (controller.signal.aborted || closed) return;
      messages = { q, results: res.results, partial: !!res.partial, error: null, update: false, waking: false };
    } catch (err) {
      if (controller.signal.aborted || closed || err.status === 401) return;
      if (err.waking) {
        messages = { q, results: null, partial: false, error: null, update: false, waking: true };
        timer = setTimeout(searchMessages, WAKE_RETRY_MS);
      } else messages = { q, results: null, partial: false, error: err.message, update: !!err.needsUpdate, waking: false };
    } finally {
      if (inflight === controller) inflight = null;
    }
    fill({ keep: true });
  }

  // Gone at once, as the palette: closing puts focus back where it was.
  const close = () => {
    closed = true;
    inflight?.abort();
    clearTimeout(timer);
    dlg.close();
    dlg.remove();
  };
  const run = (i) => {
    const o = options[i];
    if (!o) return;
    close();
    o.run();
  };

  input.addEventListener("input", () => {
    fill();
    clearTimeout(timer);
    inflight?.abort();
    // An old machine stays old while the box is open: names only, no more asking.
    if (messages.update) return;
    timer = setTimeout(searchMessages, TYPING_MS);
  });
  input.addEventListener("keydown", (e) => {
    if (e.isComposing) return;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!options.length) return;
      active = (active + (e.key === "ArrowDown" ? 1 : -1) + options.length) % options.length;
      paintActive();
    } else if (e.key === "Enter") {
      e.preventDefault();
      run(active);
    }
  });
  list.addEventListener("mousedown", (e) => e.preventDefault());
  list.addEventListener("pointermove", (e) => {
    const opt = e.target.closest(".cmdk-opt");
    if (opt && Number(opt.dataset.i) !== active) { active = Number(opt.dataset.i); paintActive(); }
  });
  list.addEventListener("click", (e) => {
    const opt = e.target.closest(".cmdk-opt");
    if (opt) run(Number(opt.dataset.i));
  });
  dlg.addEventListener("click", (e) => { if (e.target === dlg) close(); });
  dlg.addEventListener("cancel", (e) => { e.preventDefault(); close(); });

  fill();
  document.body.appendChild(dlg);
  dlg.showModal();
  input.focus();
  track("feature_used", { feature: "search_everything" });
  return { dlg, input, close };
}
