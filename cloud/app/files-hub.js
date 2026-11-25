// The Files page: setup every agent on your cloud machine reads (instructions,
// skills, Claude's rules, subagents and commands), one view for Claude Code and
// Codex, then each project's instruction files and its checkout. Desktop shows
// the list beside the open file; phones show the list, then the file.
//
// Only the machine's own account reaches this (the Cloud routes check it).
import { api, app, confirmDialog, openSheet, showError, toast } from "/app/core.js";
import { h, icon } from "/app/render.js";
import { FileTree, FileView, nameOf } from "/app/file-view.js";
import { projectFileSource, projectFileScopes } from "/app/files-panel.js";

const SETUP = "agent-setup";
const NAME = { claude: "Claude Code", codex: "Codex" };
const SHORT = { claude: "Claude", codex: "Codex" };
const CHECKOUT = "checkout";
const SCOPE_RENEW_MS = 40000;

/** One agent setup root (from the overview) as a file source. */
function setupSource(id) {
  const scope = { projectId: SETUP, workspaceId: id, attemptId: id, ownerEpoch: 0 };
  const query = (params) => api(`/api/app/agent-setup/files?${new URLSearchParams({ version: "1", ...params, projectId: SETUP, workspaceId: id, attemptId: id, ownerEpoch: "0" })}`);
  return {
    key: `setup:${id}`,
    list: (path) => query({ operation: "list", path }),
    read: (path, knownRevision) => query({ operation: "read", path, ...(knownRevision ? { knownRevision } : {}) }),
    save: (path, baseRevision, text) => api("/api/app/agent-setup/files", { method: "POST", body: { version: 1, operation: "write", scope, path, baseRevision, text } }),
  };
}

const chips = (agents) => `<span class="fh-chips">${agents.map((a) => `<span class="fh-chip fh-${a}">${SHORT[a]}</span>`).join("")}</span>`;
const textEl = (tag, className, text) => { const n = document.createElement(tag); n.className = className; n.textContent = text; return n; };

/** A part of the page that could not load, inside that part: what failed, the
 * reason, and a retry. `line` is the one-line form inside a project. */
function sectionError(e, title, retry, { line = false } = {}) {
  const box = h(`<div class="fh-error${line ? " line" : ""}" role="alert">${icon("alert")}<div class="fh-error-text"></div></div>`);
  const body = box.querySelector(".fh-error-text");
  // Plain spaces between the parts, so a wrapped line starts flush.
  body.append(textEl("span", "fh-error-title", line ? `${title}:` : title), " ", textEl("span", "fh-error-detail", e.message));
  const again = textEl("button", line ? "fh-retry" : "btn fh-retry", "Try again");
  again.type = "button";
  again.addEventListener("click", retry);
  if (line) body.append(" ", again); else box.appendChild(again);
  return box;
}

export function FilesHubScreen({ projects }) {
  const el = h(`<section class="screen fh-screen">
    <header class="topbar">
      <a class="icon-btn back" href="/app" aria-label="Back">${icon("back")}</a>
      <div class="titles"><div class="title">Files</div><div class="subtitle">Agent setup and project files</div></div>
      <button type="button" class="icon-btn fh-refresh" aria-label="Refresh" title="Refresh">${icon("refresh")}</button>
    </header>
    <div class="fh">
      <div class="fh-side">
        <section class="fh-sec">
          <h2 class="fh-h">Everywhere</h2>
          <p class="fh-sub">What every agent on your cloud machine reads, in any project.</p>
          <div class="fh-instr"><div class="fh-loading">Loading</div></div>
          <div class="fh-setup" hidden>
            <div class="fh-subh"><h3>Skills</h3><button type="button" class="btn fh-new">${icon("plus")}<span>New skill</span></button></div>
            <div class="fh-skills"></div>
            <div class="fh-folders"></div>
          </div>
        </section>
        <section class="fh-sec">
          <h2 class="fh-h">Projects</h2>
          <div class="fh-projects"></div>
        </section>
      </div>
      <div class="fh-main">
        <div class="fh-empty">${icon("folder")}<p>Choose a file to view or edit it.</p></div>
        <div class="fh-compare" hidden></div>
      </div>
    </div>
  </section>`);
  const $ = (s) => el.querySelector(s);
  const fh = $(".fh"), main = $(".fh-main"), emptyEl = $(".fh-empty"), compareEl = $(".fh-compare");
  const instrEl = $(".fh-instr"), setupEl = $(".fh-setup"), skillsEl = $(".fh-skills"), foldersEl = $(".fh-folders"), projectsEl = $(".fh-projects");
  const back = $(".back");
  back.addEventListener("click", (e) => { e.preventDefault(); app.goBack?.("/app"); });

  let overview = null;
  const sources = new Map();          // setup root id -> source (stable, so trees can tell what is open)
  const items = new Map();            // row key -> { tree, open }
  // ws -> { open, loading, loaded, generation, guide, guideError, filesError, scopesAt, source, tree, paint }
  const projectState = new Map();
  let paintedProjects = "";

  const view = FileView({
    onBack: () => fh.classList.remove("show-file"),
    onLink: (path) => { const source = view.file?.source; if (source) void openFile(source, path); },
    onChange: () => {
      fh.classList.toggle("show-file", !!view.file || !compareEl.hidden);
      emptyEl.hidden = !!view.file || !compareEl.hidden;
      for (const item of items.values()) item.tree?.paint();
      for (const p of projectState.values()) p.tree?.paint();
    },
  });
  main.appendChild(view.el);

  const sourceFor = (id) => { if (!sources.has(id)) sources.set(id, setupSource(id)); return sources.get(id); };
  const openPath = (source) => (view.file?.source === source ? view.file.path : null);

  async function openFile(source, path) {
    if (view.file?.source === source && view.file.path === path) { fh.classList.add("show-file"); return; }
    if (!(await view.discardOk())) return;
    compareEl.hidden = true;
    await view.open(source, path);
  }
  /** The first file of a root that shows one file (instructions). */
  async function openRoot(id) {
    try {
      const listing = await sourceFor(id).list("");
      const file = listing.entries.find((e) => e.kind === "file");
      if (file) await openFile(sourceFor(id), file.path);
    } catch (e) { showError(e); }
  }

  // ---------- Everywhere ----------

  let setupLoads = 0;
  /** The overview. A failure shows inside Everywhere only; projects load on their own. */
  async function load() {
    const mine = ++setupLoads;
    if (!overview) instrEl.replaceChildren(textEl("div", "fh-loading", "Loading"));
    let next;
    try { next = await api("/api/app/agent-setup"); }
    catch (e) {
      if (mine !== setupLoads) return;
      overview = null;
      setupEl.hidden = true;
      instrEl.replaceChildren(sectionError(e, "Could not load the agent setup", () => { void load(); }));
      return;
    }
    if (mine !== setupLoads) return;
    overview = next;
    setupEl.hidden = false;
    paintSetup();
  }

  async function act(body, done) {
    try { overview = await api("/api/app/agent-setup/actions", { method: "POST", body: { version: 1, ...body } }); paintSetup(); toast(done); return true; }
    catch (e) { showError(e); return false; }
  }

  function paintSetup() {
    paintInstructions();
    paintSkills();
    paintFolders();
  }

  function paintInstructions() {
    const { claude, codex, shared } = overview.instructions;
    const card = h(`<div class="fh-card"><div class="fh-card-h">${icon("file")}<span>Instructions for all agents</span></div><div class="fh-paths"></div><p class="fh-note"></p><div class="fh-actions"></div></div>`);
    const paths = card.querySelector(".fh-paths"), noteEl = card.querySelector(".fh-note"), actions = card.querySelector(".fh-actions");
    for (const s of [claude, codex]) {
      const line = h(`<div class="fh-path-line">${chips([s.agent])}<code></code><span class="fh-state"></span></div>`);
      line.querySelector("code").textContent = s.path;
      line.querySelector(".fh-state").textContent = s.state === "missing" ? "does not exist" : s.state === "link" ? `links to ${s.linksTo}` : "";
      paths.appendChild(line);
    }
    const button = (label, run, primary = false) => { const b = h(`<button type="button" class="btn${primary ? " accent" : ""}"></button>`); b.textContent = label; b.addEventListener("click", run); actions.appendChild(b); };
    const has = (s) => s.state !== "missing" && !!s.root;
    if (shared) {
      noteEl.textContent = "Both agents read this one file, so a change here reaches Claude Code and Codex alike.";
      button("Open", () => openRoot(claude.root), true);
    } else if (has(claude) && has(codex)) {
      noteEl.textContent = "Claude Code and Codex read two separate files, so they can drift apart. Compare them and keep one for both.";
      button("Compare", () => { void showCompare(); }, true);
      button("Open Claude's", () => openRoot(claude.root));
      button("Open Codex's", () => openRoot(codex.root));
    } else if (has(claude) || has(codex)) {
      const [own, missing] = has(claude) ? [claude, codex] : [codex, claude];
      const present = overview.agents[missing.agent].present;
      noteEl.textContent = present
        ? `Only ${NAME[own.agent]} has instructions for all projects. ${NAME[missing.agent]} looks for ${missing.path}, which does not exist.`
        : `Only ${NAME[own.agent]} has instructions for all projects. ${NAME[missing.agent]} is not set up on this machine.`;
      button("Open", () => openRoot(own.root), true);
      if (present) button(`Let ${NAME[missing.agent]} read it too`, () => { void linkInstructions(own.agent, null); });
    } else {
      noteEl.textContent = "Neither agent has instructions for all projects yet.";
    }
    instrEl.replaceChildren(card);
  }

  async function linkInstructions(keep, replacedRevision) {
    const { claude, codex } = overview.instructions;
    const [kept, linked] = keep === "claude" ? [claude, codex] : [codex, claude];
    const ok = await confirmDialog({
      title: `Use ${NAME[keep]}'s instructions for both agents?`,
      body: replacedRevision
        ? `${linked.path} becomes a link to ${kept.path}. Its current text is kept beside it as ${nameOf(linked.path)}.replaced-<time>.`
        : `${linked.path} will be a link to ${kept.path}, so ${NAME[linked.agent]} reads the same file.`,
      confirm: "Use for both",
    });
    if (!ok) return;
    if (await act({ operation: "link-instructions", keep, ...(replacedRevision ? { replacedRevision } : {}) }, "Both agents now read one instructions file")) {
      compareEl.hidden = true;
      view.close();
    }
  }

  /** Both instruction files side by side, to choose the one to keep. */
  async function showCompare() {
    if (!(await view.discardOk())) return;
    view.close();
    compareEl.hidden = false;
    const head = h(`<div class="fh-compare-head"><button type="button" class="icon-btn fh-compare-back" aria-label="Back" title="Back">${icon("back")}</button><span>Two separate instruction files</span></div>`);
    head.querySelector("button").addEventListener("click", () => { compareEl.hidden = true; fh.classList.remove("show-file"); emptyEl.hidden = false; });
    compareEl.replaceChildren(head, textEl("div", "fh-loading fh-compare-msg", "Loading"));
    fh.classList.add("show-file");
    emptyEl.hidden = true;
    try {
      const sides = await Promise.all(["claude", "codex"].map(async (agent) => {
        const state = overview.instructions[agent];
        const source = sourceFor(state.root);
        const file = (await source.list("")).entries.find((e) => e.kind === "file");
        return { agent, state, doc: await source.read(file.path) };
      }));
      const cols = h(`<div class="fh-compare-cols"></div>`);
      for (const side of sides) {
        const col = h(`<div class="fh-compare-col"><div class="fh-compare-title">${chips([side.agent])}<code></code></div><pre class="fh-pre"></pre><div class="fh-actions"></div></div>`);
        col.querySelector("code").textContent = side.state.path;
        col.querySelector("pre").textContent = side.doc.text ?? "This file is not text.";
        const other = sides.find((s) => s !== side);
        const keep = h(`<button type="button" class="btn primary"></button>`);
        keep.textContent = `Use this one for both`;
        keep.addEventListener("click", () => { void linkInstructions(side.agent, other.doc.revision); });
        col.querySelector(".fh-actions").appendChild(keep);
        cols.appendChild(col);
      }
      compareEl.replaceChildren(head, cols);
    } catch (e) {
      const box = textEl("div", "fh-compare-msg", "");
      box.appendChild(sectionError(e, "Could not read the instruction files", () => { void showCompare(); }));
      compareEl.replaceChildren(head, box);
    }
  }

  /** A row that expands to the tree of its root. */
  function rootRow(key, { title, agents = null, description = "", note = "", root, extra = null, glyph = "folder" }) {
    const item = items.get(key) ?? { tree: null, open: false };
    items.set(key, item);
    const row = h(`<div class="fh-item"><button type="button" class="fh-row"><span class="fp-chev">${icon("chevron")}</span>${icon(glyph)}<span class="fh-name"></span>${agents ? chips(agents) : ""}</button></div>`);
    const toggle = row.querySelector(".fh-row");
    toggle.querySelector(".fh-name").textContent = title;
    toggle.disabled = !root;
    if (description) row.appendChild(textEl("p", "fh-desc", description));
    if (note) row.appendChild(textEl("p", "fh-note", note));
    if (extra) row.appendChild(extra);
    const paint = () => {
      toggle.classList.toggle("open", item.open);
      toggle.setAttribute("aria-expanded", String(item.open));
      if (item.open && root) {
        if (!item.tree || item.tree.source !== sourceFor(root)) {
          item.tree?.destroy();
          const source = sourceFor(root);
          item.tree = FileTree({ source, label: title, current: () => openPath(source), onOpen: (path) => { void openFile(source, path); } });
          void item.tree.refresh();
        }
        row.appendChild(item.tree.el);
      } else item.tree?.el.remove();
    };
    toggle.addEventListener("click", () => { item.open = !item.open; paint(); });
    paint();
    return row;
  }

  function paintSkills() {
    const names = new Map();
    for (const s of overview.skills) names.set(s.name, (names.get(s.name) ?? 0) + 1);
    const rows = overview.skills.map((s) => {
      let extra = null;
      if (s.agents.length === 1 && s.root && names.get(s.name) === 1) {
        const other = s.agents[0] === "claude" ? "codex" : "claude";
        if (overview.agents[other].present) {
          extra = h(`<div class="fh-actions"><button type="button" class="btn">Use with both agents</button></div>`);
          extra.querySelector("button").addEventListener("click", async () => {
            const ok = await confirmDialog({ title: `Use ${s.name} with ${NAME[other]} too?`, body: `${NAME[other]}'s skills folder gets a link to ${s.location}, so both agents load the same skill.`, confirm: "Use with both" });
            // The home finds the skill by its folder (the root's last part), which
            // can differ from the name in its SKILL.md.
            if (ok) await act({ operation: "share-skill", name: s.root.split(":")[2], from: s.agents[0] }, `${NAME[other]} can use ${s.name} now`);
          });
        }
      }
      return rootRow(`skill:${s.location}`, { title: s.name, agents: s.agents, description: s.description, note: s.note, root: s.root, extra, glyph: "file" });
    });
    skillsEl.replaceChildren(...(rows.length ? rows : [textEl("p", "fh-note", "No skills yet. A skill is a folder with a SKILL.md that tells agents when and how to do something.")]));
  }

  function paintFolders() {
    foldersEl.replaceChildren(...overview.folders.map((f) => rootRow(`folder:${f.id}`, { title: f.label, agents: ["claude"], root: f.root })));
  }

  $(".fh-new").addEventListener("click", () => {
    const { body, close } = openSheet({ title: "New skill", className: "fh-sheet" });
    body.append(h(`<form class="fh-form" novalidate>
      <label class="field-label" for="fh-skill-name">Name</label>
      <input id="fh-skill-name" class="input" name="name" placeholder="release-notes" maxlength="64" autocapitalize="off" autocorrect="off" spellcheck="false">
      <label class="field-label" for="fh-skill-desc">When agents should use it</label>
      <input id="fh-skill-desc" class="input" name="description" placeholder="Use when writing release notes for a version" maxlength="1024">
      <p class="hint">It goes in ~/.agents/skills, which Codex reads, with a link from Claude's skills folder, so both agents use it.</p>
      <button type="submit" class="btn accent block" disabled>Create</button>
    </form>`));
    const form = body.querySelector("form");
    const submit = form.querySelector("button");
    const valid = () => /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/.test(form.name.value.trim()) && !!form.description.value.trim();
    form.addEventListener("input", () => { submit.disabled = !valid(); });
    form.addEventListener("submit", async (e) => {
      e.preventDefault();
      if (!valid()) return;
      submit.disabled = true;
      const name = form.name.value.trim();
      if (await act({ operation: "new-skill", name, description: form.description.value.trim() }, `Created ${name}`)) {
        close();
        const row = overview.skills.find((s) => s.name === name);
        if (row?.root) await openFile(sourceFor(row.root), "SKILL.md");
      } else submit.disabled = false;
    });
    setTimeout(() => form.name.focus(), 50);
  });

  // ---------- Projects ----------

  function paintProjects() {
    const list = projects();
    const key = JSON.stringify(list.map((p) => [p.workspaceId, p.name]));
    if (key === paintedProjects && projectsEl.childElementCount) return;
    paintedProjects = key;
    if (!list.length) { projectsEl.replaceChildren(textEl("p", "fh-note", "No projects on your cloud machine yet.")); return; }
    projectsEl.replaceChildren(...list.map((p) => projectRow(p)));
  }

  function projectRow(p) {
    const ws = p.workspaceId;
    const st = projectState.get(ws) ?? { open: false };
    projectState.set(ws, st);
    const row = h(`<div class="fh-item fh-project"><button type="button" class="fh-row"><span class="fp-chev">${icon("chevron")}</span>${icon("folder")}<span class="fh-name"></span></button><div class="fh-project-body"></div></div>`);
    const toggle = row.querySelector(".fh-row"), bodyEl = row.querySelector(".fh-project-body");
    toggle.querySelector(".fh-name").textContent = p.name || ws;
    // The instruction files and the file tree load and fail on their own. When
    // both fail for the same reason, one line says so.
    const paint = () => {
      toggle.classList.toggle("open", st.open);
      toggle.setAttribute("aria-expanded", String(st.open));
      bodyEl.hidden = !st.open;
      if (!st.open) return;
      if (!st.loaded) { bodyEl.replaceChildren(textEl("div", "fh-loading", "Loading")); return; }
      const retry = () => { void loadProject(ws, st); };
      const same = st.guideError && st.filesError && st.guideError.message === st.filesError.message;
      const parts = [];
      if (same) parts.push(sectionError(st.guideError, "Could not open this project", retry, { line: true }));
      else if (st.guideError) parts.push(sectionError(st.guideError, "Could not read its instruction files", retry, { line: true }));
      else parts.push(guideView(st));
      if (st.filesError) { if (!same) parts.push(sectionError(st.filesError, "Could not list its files", retry, { line: true })); }
      else if (st.tree) parts.push(textEl("h4", "fh-tree-h", "All files"), st.tree.el);
      else parts.push(textEl("p", "fh-note", "The project's engine is not running on your cloud machine, so its files cannot be opened right now."));
      bodyEl.replaceChildren(...parts);
    };
    st.paint = paint;
    toggle.addEventListener("click", () => {
      st.open = !st.open;
      paint();
      if (st.open && !st.loaded && !st.loading) void loadProject(ws, st);
    });
    paint();
    return row;
  }

  async function loadProject(ws, st) {
    const mine = st.generation = (st.generation ?? 0) + 1;
    st.tree?.destroy();
    Object.assign(st, { loading: true, loaded: false, guide: null, guideError: null, filesError: null, source: null, tree: null });
    st.paint?.();
    const [guide, scopes] = await Promise.allSettled([
      api(`/api/app/project-guide?${new URLSearchParams({ workspaceId: ws })}`),
      projectFileScopes(ws),
    ]);
    if (mine !== st.generation) return;
    if (guide.status === "fulfilled") st.guide = guide.value; else st.guideError = guide.reason;
    if (scopes.status === "rejected") st.filesError = scopes.reason;
    else {
      st.scopesAt = Date.now();
      const checkout = scopes.value.find((s) => s.workspaceId === CHECKOUT);
      if (checkout) {
        const renew = async () => { if (Date.now() - st.scopesAt >= SCOPE_RENEW_MS) { await projectFileScopes(ws); st.scopesAt = Date.now(); } };
        st.source = projectFileSource(checkout, { renew, denied: () => { st.scopesAt = 0; } });
        st.tree = FileTree({ source: st.source, label: "Project files", current: () => openPath(st.source), onOpen: (path) => { void openFile(st.source, path); } });
        void st.tree.refresh();
      }
    }
    Object.assign(st, { loading: false, loaded: true });
    st.paint?.();
  }

  function guideView(st) {
    const box = h(`<div class="fh-guide"></div>`);
    const { files, shared } = st.guide;
    if (!files.length) { box.appendChild(textEl("p", "fh-note", "This project has no AGENTS.md, CLAUDE.md, RUDDER.md or README.md.")); return box; }
    for (const f of files) {
      // Name and link in one ellipsized label: a narrow row cuts the link's end, not the name.
      const b = h(`<button type="button" class="fh-guide-file"><span class="fp-chev"></span>${icon("file")}<span class="fh-guide-label"><span class="fh-guide-name"></span></span>${chips(f.readBy)}</button>`);
      b.querySelector(".fh-guide-name").textContent = f.path;
      const link = f.linksTo ? `links to ${f.linksTo}` : f.imports.length ? `includes ${f.imports.join(", ")}` : "";
      if (link) b.querySelector(".fh-guide-label").append(" ", textEl("span", "fh-link", link));
      b.disabled = !st.source;
      b.addEventListener("click", () => { if (st.source) void openFile(st.source, f.linksTo ?? f.path); });
      box.appendChild(b);
    }
    const claude = files.some((f) => f.readBy.includes("claude")), codex = files.some((f) => f.readBy.includes("codex"));
    const note = shared ? "Both agents read the same instructions here: CLAUDE.md leads Claude Code to AGENTS.md, which Codex reads."
      : claude && codex ? "Claude Code reads CLAUDE.md and Codex reads AGENTS.md. They are separate files, so keep them in step or have CLAUDE.md link to AGENTS.md."
      : claude ? "Only Claude Code has instructions here; Codex reads AGENTS.md, which this project does not have."
      : codex ? "Only Codex has instructions here; Claude Code reads CLAUDE.md, which this project does not have."
      : "";
    if (note) box.appendChild(textEl("p", "fh-note", note));
    return box;
  }

  $(".fh-refresh").addEventListener("click", () => {
    void load();
    for (const item of items.values()) if (item.open) void item.tree?.refresh();
    paintProjects();
    // Open projects reload now; closed ones when next opened.
    for (const [ws, st] of projectState) {
      if (st.open) void loadProject(ws, st);
      else if (!st.loading) st.loaded = false;
    }
    void view.refresh();
  });

  void load();
  paintProjects();
  return {
    el,
    update() { paintProjects(); },
    destroy() {
      view.destroy();
      for (const item of items.values()) item.tree?.destroy();
      for (const st of projectState.values()) st.tree?.destroy();
    },
  };
}
