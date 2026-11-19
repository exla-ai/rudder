// Which agent, model and effort new agents in a project start on. Choosing
// sends the dashboard `/model <claude|codex> <model> <effort>`, exactly what
// typing it in the dashboard's task bar does (effort "auto" lets the agent
// decide; native/src/main.rs handle_command).
import { h, icon, setText } from "/app/render.js";
import { app, api, ls, showError, toast, track } from "/app/core.js";

const PROVIDERS = [
  { id: "claude", label: "Claude", account: "Claude Code" },
  { id: "codex", label: "Codex", account: "ChatGPT" },
];
const PLAN_LABEL = { max: "Max", pro: "Pro", plus: "Plus", team: "Team", business: "Business", enterprise: "Enterprise", edu: "Edu", free: "Free", API: "API key" };
// Effort names. Which ones a model takes comes with the list: each row's
// `efforts` (src/models.ts modelEfforts), plus Auto when it takes any.
const EFFORTS = [
  { id: "auto", label: "Auto", detail: "the agent decides" },
  { id: "low", label: "Low", detail: "fastest" },
  { id: "medium", label: "Medium", detail: "balanced" },
  { id: "high", label: "High", detail: "deeper reasoning" },
  { id: "xhigh", label: "Extra high", detail: "extended reasoning" },
  { id: "max", label: "Max", detail: "maximum reasoning" },
];
/** The effort choices of a list row: Auto and the levels it takes, or none. */
const effortsOf = (row) => (row?.efforts?.length ? EFFORTS.filter((e) => e.id === "auto" || row.efforts.includes(e.id)) : []);
/** `effort` when `row` takes it, else Auto. */
const effortFor = (row, effort) => (effortsOf(row).some((e) => e.id === effort) ? effort : "auto");

// The list (`rudder __models` on the cloud machine, built ahead of time) is
// kept on this device, so the sheet opens on the last one at once and swaps
// in the fresh one when it arrives.
const MODELS_KEY = "rudder.models";
let modelsCache = ls.get(MODELS_KEY); // { current, claude, codex } or null
let modelsFresh = false; // fetched since this page loaded
let modelsFetch = null;
function loadModels() {
  modelsFetch ??= api("/api/app/models").then((data) => {
    modelsCache = data || {};
    modelsFresh = true;
    ls.set(MODELS_KEY, modelsCache);
    return modelsCache;
  }).finally(() => { modelsFetch = null; });
  return modelsFetch;
}
const rowsOf = (data, backend) => (data?.[backend] || []).filter((o) => o && o.value);
const rowFor = (backend, model) => rowsOf(modelsCache, backend).find((o) => o.value === model) || null;
/** The readable name of a model id ("Opus 5.5"), or the id when the list does not have it. */
const modelName = (backend, model) => rowFor(backend, model)?.label || model;

const pickKey = (ws) => `rudder.model.${ws}`;
/** What this device last chose for the project, or the dashboard's current model. */
function currentChoice(ws) {
  const mine = ls.get(pickKey(ws));
  if (mine?.backend && mine?.model) return { ...mine, label: rowFor(mine.backend, mine.model)?.label || mine.label || mine.model };
  const cur = modelsCache?.current;
  if (cur?.backend && cur?.model) return { backend: cur.backend, model: cur.model, label: modelName(cur.backend, cur.model), effort: cur.effort || null };
  return null;
}
const providerLabel = (b) => PROVIDERS.find((p) => p.id === b)?.label || b;
const effortLabel = (e) => EFFORTS.find((x) => x.id === e)?.label || "";
export function choiceLabel(c) {
  if (!c) return "Choose model";
  return [providerLabel(c.backend), c.label || c.model, c.effort && c.effort !== "auto" ? effortLabel(c.effort).toLowerCase() : null].filter(Boolean).join(" · ");
}

function accountLine(backend) {
  const cred = app.store?.state?.home?.credentials?.[backend];
  const p = PROVIDERS.find((x) => x.id === backend);
  if (!cred) return `Runs on your own ${p.account} subscription`;
  if (cred.ok) return [cred.account, cred.plan ? `${PLAN_LABEL[cred.plan] || cred.plan} plan` : null].filter(Boolean).join(" · ") || "Signed in";
  return cred.account ? `${cred.account} · sign in again on the cloud machine` : `Not signed in on the cloud machine`;
}

/**
 * The model rows of `opts` under their generation headings, `model` checked
 * and `current` badged. Keeps the list's scroll; `reveal` scrolls the checked
 * row into view.
 */
function paintModels(modelsEl, opts, { model, current, backend, onPick, reveal }) {
  const top = modelsEl.scrollTop;
  modelsEl.replaceChildren();
  let group;
  for (const o of opts) {
    if (o.group && o.group !== group) {
      group = o.group;
      const head = h(`<div class="mp-group" role="presentation"></div>`);
      head.textContent = group;
      modelsEl.appendChild(head);
    }
    const b = h(`<button type="button" role="radio" class="mp-model"><span class="mp-mt"><span class="mp-name"></span><span class="mp-badge" hidden>Current</span></span><span class="mp-detail"><span class="mp-id"></span><span class="mp-desc"></span></span><span class="mp-radio">${icon("check")}</span></button>`);
    setText(b.querySelector(".mp-name"), o.label || o.value);
    b.querySelector(".mp-badge").hidden = o.value !== current;
    setText(b.querySelector(".mp-id"), o.value);
    setText(b.querySelector(".mp-desc"), o.detail || "");
    b.title = [o.value, o.detail].filter(Boolean).join(" · ");
    b.setAttribute("aria-checked", String(o.value === model));
    b.addEventListener("click", () => onPick(o.value));
    modelsEl.appendChild(b);
  }
  if (!opts.length) {
    const note = h(`<div class="repo-note"></div>`);
    note.textContent = `No ${providerLabel(backend)} models found on the cloud machine.`;
    modelsEl.appendChild(note);
  }
  modelsEl.scrollTop = top;
  if (reveal) modelsEl.querySelector('[aria-checked="true"]')?.scrollIntoView({ block: "nearest" });
}

/** The effort chips of `row`, `effort` checked. Returns whether it takes an effort. */
function paintEfforts(effortWrap, effortsEl, { row, effort, onPick }) {
  const choices = effortsOf(row);
  effortWrap.hidden = !choices.length;
  effortsEl.replaceChildren(...choices.map((e) => {
    const b = h(`<button type="button" role="radio" class="mp-effort"><span></span></button>`);
    setText(b.querySelector("span"), e.label);
    b.title = e.detail;
    b.setAttribute("aria-checked", String(e.id === effort));
    b.addEventListener("click", () => onPick(e.id));
    return b;
  }));
  return choices.length > 0;
}

/** The list's error under the account line; the list shown stays usable. */
function showListError(errEl, modelsEl, err, hasList) {
  const message = err.status === 401 ? "Signed out." : err.message;
  if (hasList) {
    errEl.hidden = false;
    errEl.textContent = `Could not refresh the list: ${message}`;
  } else {
    modelsEl.replaceChildren(h(`<div class="repo-note err"></div>`));
    modelsEl.firstChild.textContent = message;
  }
}

/**
 * A chip that names the model new agents start on and opens the picker.
 * Returns { el, refresh() }.
 */
export function modelChip(ws, { compact = false } = {}) {
  const el = h(`<button type="button" class="model-chip${compact ? " compact" : ""}" aria-haspopup="dialog">${icon("spark")}<span class="mc-l"></span>${icon("chevron").replace("<svg", '<svg class="mc-caret"')}</button>`);
  const refresh = () => {
    const c = currentChoice(ws);
    setText(el.querySelector(".mc-l"), compact && c ? (c.label || c.model) : choiceLabel(c));
    el.setAttribute("aria-label", `Model for new agents: ${choiceLabel(c)}. Change`);
    el.title = `New agents start on ${choiceLabel(c)}`;
  };
  el.addEventListener("click", () => openModelPicker(ws, { onPicked: refresh }));
  refresh();
  // The label reads the list; a failed fetch shows when the sheet opens.
  if (!modelsFresh) loadModels().then(refresh).catch(() => {});
  return { el, refresh };
}

export function openModelPicker(ws, { onPicked } = {}) {
  const dlg = h(`<dialog class="sheet add-sheet model-sheet" aria-label="Model for new agents"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3>Model for new agents</h3><button type="button" class="icon-btn" data-act="close" aria-label="Close">${icon("x")}</button></div>
    <div class="seg modes mp-providers" role="tablist" aria-label="Agent"></div>
    <div class="mp-account"></div>
    <div class="repo-note err mp-error" hidden></div>
    <div class="mp-models" role="radiogroup" aria-label="Model"><div class="repo-note">Loading models…</div></div>
    <div class="mp-effort-wrap">
      <div class="field-label" id="mp-effort-l">Effort</div>
      <div class="mp-efforts" role="radiogroup" aria-labelledby="mp-effort-l"></div>
    </div>
    <button type="button" class="btn accent block mp-use" disabled>Use this model</button>
    <div class="create-note">Applies to agents you start next. Running agents keep theirs.</div>
  </div></dialog>`);
  const provEl = dlg.querySelector(".mp-providers");
  const errEl = dlg.querySelector(".mp-error");
  const modelsEl = dlg.querySelector(".mp-models");
  const effortWrap = dlg.querySelector(".mp-effort-wrap");
  const effortsEl = dlg.querySelector(".mp-efforts");
  const useBtn = dlg.querySelector(".mp-use");
  let data = modelsCache;
  const start = currentChoice(ws);
  let backend = start?.backend === "codex" ? "codex" : "claude";
  let model = start?.model || null;
  let effort = start?.effort || "auto";
  let revealed = false;

  const options = () => rowsOf(data, backend);
  function render() {
    provEl.replaceChildren(...PROVIDERS.map((p) => {
      const b = h(`<button type="button" role="tab"><span></span></button>`);
      setText(b.querySelector("span"), p.label);
      b.setAttribute("aria-selected", String(p.id === backend));
      b.addEventListener("click", () => {
        if (backend === p.id) return;
        backend = p.id;
        const cur = data?.current;
        model = cur?.backend === backend ? cur.model : options()[0]?.value || null;
        render();
      });
      return b;
    }));
    setText(dlg.querySelector(".mp-account"), accountLine(backend));
    if (!data) return;
    const opts = options();
    if (!model || !opts.some((o) => o.value === model)) model = opts[0]?.value || null;
    const cur = data.current?.backend === backend ? data.current.model : null;
    paintModels(modelsEl, opts, { model, current: cur, backend, onPick: (v) => { model = v; render(); }, reveal: !revealed });
    revealed = true;
    const row = opts.find((o) => o.value === model);
    const eff = effortFor(row, effort);
    const dial = paintEfforts(effortWrap, effortsEl, { row, effort: eff, onPick: (v) => { effort = v; render(); } });
    useBtn.disabled = !model;
    setText(useBtn, model ? `Use ${providerLabel(backend)} · ${row?.label || model}${dial && eff !== "auto" ? ` · ${effortLabel(eff).toLowerCase()}` : ""}` : "Use this model");
  }

  useBtn.addEventListener("click", async () => {
    if (!model) return;
    const row = options().find((o) => o.value === model);
    const eff = effortsOf(row).length ? effortFor(row, effort) : null;
    useBtn.disabled = true;
    try {
      // Exactly the dashboard grammar: /model <provider> <model> [effort].
      const text = `/model ${backend} ${model}${eff ? ` ${eff}` : ""}`;
      await api("/api/app/task", { method: "POST", body: { workspace: ws, text } });
      const choice = { backend, model, label: row?.label || model, effort: eff };
      ls.set(pickKey(ws), choice);
      track("model_picked", { backend, model, effort: eff || "none" });
      dlg.close();
      toast(`New agents start on ${choiceLabel(choice)}`);
      onPicked?.(choice);
    } catch (err) { showError(err); useBtn.disabled = false; }
  });

  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest('[data-act="close"]')) dlg.close();
  });
  dlg.addEventListener("close", () => dlg.remove());
  document.body.appendChild(dlg);
  dlg.showModal();
  render();
  loadModels().then((res) => {
    data = res;
    errEl.hidden = true;
    if (!start) {
      const cur = data.current;
      if (cur?.backend === "claude" || cur?.backend === "codex") { backend = cur.backend; model = cur.model; effort = cur.effort || effort; }
    }
    render();
  }).catch((err) => showListError(errEl, modelsEl, err, !!data));
}

/* The model control in an agent's chat --------------------------------- */

const chatLabel = (a) => [modelName(a.backend, a.model), a.effort ? effortLabel(a.effort).toLowerCase() : null].filter(Boolean).join(" · ");

/**
 * The model and effort one agent's chat runs on, next to its composer, and a
 * sheet to switch them for that chat only. The dashboard reopens the same
 * conversation on the new ones (POST /api/app/agent-model, native
 * deliver_model_request), so the agent's next turn uses them. It stays on its
 * provider, and it does not switch in the middle of a turn.
 * `agent()` returns the agent's current row. Call refresh() when it changes.
 */
export function mountChatModelControl(container, { workspace, run, agent }) {
  const el = h(`<button type="button" class="model-chip compact" aria-haspopup="dialog">${icon("spark")}<span class="mc-l"></span>${icon("chevron").replace("<svg", '<svg class="mc-caret"')}</button>`);
  const refresh = () => {
    const a = agent();
    el.hidden = !a?.model || !PROVIDERS.some((p) => p.id === a.backend);
    if (el.hidden) return;
    setText(el.querySelector(".mc-l"), chatLabel(a));
    el.setAttribute("aria-label", `This chat runs on ${providerLabel(a.backend)} ${chatLabel(a)}. Change`);
    el.title = `This chat runs on ${providerLabel(a.backend)} · ${chatLabel(a)} (${a.model})`;
  };
  el.addEventListener("click", () => openChatModelPicker({ workspace, run, agent, onSwitched: refresh }));
  container.appendChild(el);
  refresh();
  // The label reads the list; a failed fetch shows when the sheet opens.
  if (!modelsFresh) loadModels().then(refresh).catch(() => {});
  return { el, refresh };
}

function openChatModelPicker({ workspace, run, agent, onSwitched }) {
  const start = agent();
  const backend = start.backend;
  const dlg = h(`<dialog class="sheet add-sheet model-sheet" aria-label="Model for this chat"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3>Model for this chat</h3><button type="button" class="icon-btn" data-act="close" aria-label="Close">${icon("x")}</button></div>
    <div class="mp-account"></div>
    <div class="repo-note err mp-error" hidden></div>
    <div class="mp-models" role="radiogroup" aria-label="Model"><div class="repo-note">Loading models…</div></div>
    <div class="mp-effort-wrap">
      <div class="field-label" id="cm-effort-l">Effort</div>
      <div class="mp-efforts" role="radiogroup" aria-labelledby="cm-effort-l"></div>
    </div>
    <button type="button" class="btn accent block mp-use" disabled>Switch</button>
    <div class="create-note"></div>
  </div></dialog>`);
  const errEl = dlg.querySelector(".mp-error");
  const modelsEl = dlg.querySelector(".mp-models");
  const effortWrap = dlg.querySelector(".mp-effort-wrap");
  const effortsEl = dlg.querySelector(".mp-efforts");
  const useBtn = dlg.querySelector(".mp-use");
  const note = dlg.querySelector(".create-note");
  let data = modelsCache;
  let model = null; // the picked row's value; until a pick, the row the agent runs on
  let effort = start.effort || "auto";
  let revealed = false;

  // This backend's list, led by the model the agent runs on when the list
  // does not have it (an id it was started with by hand).
  const options = () => {
    const opts = rowsOf(data, backend);
    return opts.some((o) => o.value === start.model) ? opts : [{ value: start.model, label: start.model }, ...opts];
  };
  const rowOf = (opts, m) => (opts.some((o) => o.value === m) ? m : null);
  function render() {
    const a = agent() || start;
    const busy = a.status === "working";
    setText(dlg.querySelector(".mp-account"), `${providerLabel(backend)} · ${accountLine(backend)}`);
    setText(note, busy
      ? "It is working right now. Switch when this turn ends."
      : "The conversation carries on. Its next reply uses the new model.");
    if (!data) return;
    const opts = options();
    const current = rowOf(opts, a.model);
    model ??= rowOf(opts, start.model);
    paintModels(modelsEl, opts, { model, current, backend, onPick: (v) => { model = v; render(); }, reveal: !revealed });
    revealed = true;
    const row = opts.find((o) => o.value === model);
    const eff = effortFor(row, effort);
    const dial = paintEfforts(effortWrap, effortsEl, { row, effort: eff, onPick: (v) => { effort = v; render(); } });
    const same = model === current && (!dial || eff === (a.effort || "auto"));
    useBtn.disabled = busy || same;
    setText(useBtn, same ? "Current model" : `Switch to ${row?.label || model}${eff !== "auto" ? ` · ${effortLabel(eff).toLowerCase()}` : ""}`);
  }

  useBtn.addEventListener("click", async () => {
    const row = options().find((o) => o.value === model);
    const eff = effortFor(row, effort);
    useBtn.disabled = true;
    try {
      const res = await api("/api/app/agent-model", { method: "POST", body: { workspace, run, model, effort: eff } });
      track("chat_model_switched", { backend, model, effort: eff });
      // The dashboard took it (or will within seconds): show it now, the next
      // state refresh carries the real row.
      const a = agent();
      if (a) { a.model = model; a.effort = eff === "auto" ? null : eff; }
      dlg.close();
      toast(res?.status === "queued" ? "Switching when the cloud machine picks it up" : `This chat now runs on ${row?.label || model}`);
      onSwitched();
      app.refreshState();
    } catch (err) { showError(err); render(); }
  });

  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest('[data-act="close"]')) dlg.close();
  });
  dlg.addEventListener("close", () => dlg.remove());
  document.body.appendChild(dlg);
  dlg.showModal();
  render();
  loadModels().then((res) => { data = res; errEl.hidden = true; render(); })
    .catch((err) => showListError(errEl, modelsEl, err, !!data));
}
