// Which harness, model and effort new agents in a project start on. Choosing
// sends the dashboard `/model <harness> <model> <effort>`, exactly what
// typing it in the dashboard's task bar does (effort "auto" lets the agent
// decide; native/src/main.rs handle_command).
import { h, icon, setText } from "/app/render.js";
import { app, api, ls, showError, toast, track } from "/app/core.js";
import { harnessLogo } from "/app/harness-logos.js";

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
const effortLabel = (e) => EFFORTS.find((x) => x.id === e)?.label || "";
/** How an effort reads after a model's name ("high"); Auto and none read as nothing. */
const effortWord = (e) => (e && e !== "auto" ? effortLabel(e).toLowerCase() : null);

// The list (`rudder __models` on the cloud machine, built ahead of time):
// { current, harnesses: [{ id, label, account, models }] }, one entry per
// harness (src/models.ts HARNESSES) in the order the pickers show them, each
// model named by its name and then its version ("Opus 5.5", "Sol 6.1"), and
// `current` named the same way. It is kept on this device, so the sheet opens
// on the last one at once and swaps in the fresh one when it arrives.
//
// The app never names a model from its id. The cloud machine names every one
// it sends by the same rules (src/models.ts modelName): the list's rows and
// `current`, and `modelName` on agents, plan nodes and digest runs.
const MODELS_KEY = "rudder.models";
let modelsCache = ls.get(MODELS_KEY);
let modelsFresh = false; // fetched since this page loaded
let modelsAsked = false; // a fetch started since this page loaded
let modelsFetch = null;
function loadModels() {
  modelsAsked = true;
  modelsFetch ??= api("/api/app/models").then((data) => {
    const changed = JSON.stringify(data) !== JSON.stringify(modelsCache);
    modelsCache = data;
    modelsFresh = true;
    ls.set(MODELS_KEY, modelsCache);
    // Agent rows on screen read their model names from the list.
    if (changed) app.updateScreen();
    return modelsCache;
  }).finally(() => { modelsFetch = null; });
  return modelsFetch;
}
const harnessesOf = (data) => data?.harnesses || [];
const harnessOf = (data, id) => harnessesOf(data).find((x) => x.id === id) || null;
const rowsOf = (data, backend) => harnessOf(data, backend)?.models || [];
const rowFor = (backend, model) => rowsOf(modelsCache, backend).find((o) => o.value === model) || null;

/**
 * A harness's name ("Claude"), from the list; one the list does not have
 * (opencode) reads as its id. Before this device has a list, none: the first
 * call fetches it, and the screen repaints when it lands.
 */
function harnessLabel(backend) {
  if (!modelsCache && !modelsAsked) loadModels().catch(() => {});
  return modelsCache ? harnessOf(modelsCache, backend)?.label ?? backend : null;
}

/** A model in words, harness first: "Claude · Opus 5.5 · high". Any part may be missing. */
function modelText(backend, name, effort) {
  return [harnessLabel(backend), name, effortWord(effort)].filter(Boolean).join(" · ");
}

/**
 * How an agent's harness and model read in text: "Codex · Sol 6.1". `row` is
 * an agent, a plan node or a digest run, each with `backend` and the
 * `modelName` the cloud machine gave its model.
 */
export const agentModelLabel = (row) => modelText(row.backend, row.modelName);

const CHIP = `<button type="button" class="model-chip" aria-haspopup="dialog"><span class="mc-logo"></span><span class="mc-l"><span class="mc-n"></span><span class="mc-e"></span></span>${icon("chevron").replace("<svg", '<svg class="mc-caret"')}</button>`;

/**
 * A model chip's face: the harness's logo, the model's name and, quieter, its
 * effort: [logo] Opus 5.5 · high. The logo says which harness it is, so the
 * name goes without "Claude ·"; a harness with no logo yet (harness-logos.js)
 * is named in words instead. With no name (a cloud machine too old to send
 * one, or a harness that does not say what it runs by default) the chip
 * names the harness: [logo] Claude. It is never just a logo.
 */
function paintChip(el, { backend, name, effort }) {
  const logo = harnessLogo(backend);
  const slot = el.querySelector(".mc-logo");
  if (slot.dataset.backend !== String(backend)) {
    slot.innerHTML = logo;
    slot.dataset.backend = String(backend);
  }
  slot.hidden = !logo;
  setText(el.querySelector(".mc-n"), logo ? name || harnessLabel(backend) : modelText(backend, name));
  const word = effortWord(effort);
  setText(el.querySelector(".mc-e"), word ? ` · ${word}` : "");
}

const pickKey = (ws) => `rudder.model.${ws}`;
/**
 * What this device last chose for the project, or else the dashboard's
 * current model, with its name: the list's while it lists the model (an
 * alias's name moves with it), else the name it was picked under.
 */
function currentChoice(ws) {
  const mine = ls.get(pickKey(ws));
  if (mine?.backend && mine?.model && mine?.name) {
    return { backend: mine.backend, model: mine.model, effort: mine.effort || null, name: rowFor(mine.backend, mine.model)?.label ?? mine.name };
  }
  const cur = modelsCache?.current;
  if (cur?.backend && cur?.model && cur?.name) return { backend: cur.backend, model: cur.model, effort: cur.effort || null, name: cur.name };
  return null;
}
/** "Codex · Sol 6.1 · high" */
function choiceLabel(c) {
  return c ? modelText(c.backend, c.name, c.effort) : "Choose model";
}

function accountLine(harness) {
  const cred = app.store?.state?.home?.credentials?.[harness.id];
  if (!cred) return `Runs on your own ${harness.account} subscription`;
  if (cred.ok) return [cred.account, cred.plan ? `${PLAN_LABEL[cred.plan] || cred.plan} plan` : null].filter(Boolean).join(" · ") || "Signed in";
  return cred.account ? `${cred.account} · sign in again on the cloud machine` : `Not signed in on the cloud machine`;
}

/**
 * One section per harness: its name and sign-in, then its models, the
 * aliases last under "Always the newest". `selected` is checked and `current`
 * badged ({ backend, model } each). Keeps the list's scroll; `reveal` scrolls
 * the checked row into view.
 */
function paintModels(modelsEl, sections, { selected, current, onPick, reveal }) {
  const top = modelsEl.scrollTop;
  modelsEl.replaceChildren();
  for (const { harness, models } of sections) {
    const head = h(`<div class="mp-harness" role="presentation"><span class="mp-hn"></span><span class="mp-ha"></span></div>`);
    setText(head.querySelector(".mp-hn"), harness.label);
    setText(head.querySelector(".mp-ha"), accountLine(harness));
    modelsEl.appendChild(head);
    let aliases = false;
    for (const o of models) {
      if (o.alias && !aliases) {
        aliases = true;
        modelsEl.appendChild(h(`<div class="mp-group" role="presentation">Always the newest</div>`));
      }
      const b = h(`<button type="button" role="radio" class="mp-model"><span class="mp-mt"><span class="mp-name"></span><span class="mp-badge" hidden>Current</span></span><span class="mp-detail"><span class="mp-id"></span><span class="mp-desc"></span></span><span class="mp-radio">${icon("check")}</span></button>`);
      setText(b.querySelector(".mp-name"), o.label);
      b.querySelector(".mp-badge").hidden = !(current?.backend === harness.id && current.model === o.value);
      setText(b.querySelector(".mp-id"), o.value);
      setText(b.querySelector(".mp-desc"), o.detail || "");
      b.title = [o.value, o.detail].filter(Boolean).join(" · ");
      b.setAttribute("aria-label", `${harness.label} ${o.label}`);
      b.setAttribute("aria-checked", String(selected?.backend === harness.id && selected.model === o.value));
      b.addEventListener("click", () => onPick(harness.id, o.value));
      modelsEl.appendChild(b);
    }
    if (!models.length) {
      const note = h(`<div class="repo-note"></div>`);
      note.textContent = `No ${harness.label} models found on the cloud machine.`;
      modelsEl.appendChild(note);
    }
  }
  if (!sections.length) modelsEl.appendChild(h(`<div class="repo-note">No models found on the cloud machine.</div>`));
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

/** The list's error above the list; the list shown stays usable. */
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
 * A chip that shows the harness and model new agents start on and opens the
 * picker; `compact` leaves the effort out. Returns { el, refresh() }.
 */
export function modelChip(ws, { compact = false } = {}) {
  const el = h(CHIP);
  if (compact) el.classList.add("compact");
  const refresh = () => {
    const c = currentChoice(ws);
    paintChip(el, c ? { backend: c.backend, name: c.name, effort: compact ? null : c.effort } : { backend: null, name: "Choose model" });
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
    <div class="repo-note err mp-error" hidden></div>
    <div class="mp-models" role="radiogroup" aria-label="Model"><div class="repo-note">Loading models…</div></div>
    <div class="mp-effort-wrap">
      <div class="field-label" id="mp-effort-l">Effort</div>
      <div class="mp-efforts" role="radiogroup" aria-labelledby="mp-effort-l"></div>
    </div>
    <button type="button" class="btn accent block mp-use" disabled>Use this model</button>
    <div class="create-note">Applies to agents you start next. Running agents keep theirs.</div>
  </div></dialog>`);
  const errEl = dlg.querySelector(".mp-error");
  const modelsEl = dlg.querySelector(".mp-models");
  const effortWrap = dlg.querySelector(".mp-effort-wrap");
  const effortsEl = dlg.querySelector(".mp-efforts");
  const useBtn = dlg.querySelector(".mp-use");
  let data = modelsCache;
  const start = currentChoice(ws);
  let pick = start && { backend: start.backend, model: start.model };
  let effort = start?.effort || "auto";
  let revealed = false;

  const pickedRow = () => (pick ? rowsOf(data, pick.backend).find((o) => o.value === pick.model) || null : null);
  function render() {
    if (!data) return;
    // A model the list does not have starts on its harness's first model.
    if (!pickedRow()) {
      const harness = harnessOf(data, pick?.backend) || harnessesOf(data)[0];
      pick = harness?.models.length ? { backend: harness.id, model: harness.models[0].value } : null;
    }
    const current = data.current?.backend && data.current?.model ? data.current : null;
    const sections = harnessesOf(data).map((harness) => ({ harness, models: harness.models }));
    paintModels(modelsEl, sections, { selected: pick, current, onPick: (backend, model) => { pick = { backend, model }; render(); }, reveal: !revealed });
    revealed = true;
    const row = pickedRow();
    const eff = effortFor(row, effort);
    const dial = paintEfforts(effortWrap, effortsEl, { row, effort: eff, onPick: (v) => { effort = v; render(); } });
    useBtn.disabled = !pick;
    setText(useBtn, pick ? `Use ${modelText(pick.backend, row.label, dial ? eff : null)}` : "Use this model");
  }

  useBtn.addEventListener("click", async () => {
    if (!pick) return;
    const { backend, model } = pick;
    const row = pickedRow();
    const eff = effortsOf(row).length ? effortFor(row, effort) : null;
    useBtn.disabled = true;
    try {
      // Exactly the dashboard grammar: /model <harness> <model> [effort].
      const text = `/model ${backend} ${model}${eff ? ` ${eff}` : ""}`;
      await api("/api/app/task", { method: "POST", body: { workspace: ws, text } });
      const choice = { backend, model, effort: eff, name: row.label };
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
    if (!start && data.current?.backend && data.current?.model) {
      pick = { backend: data.current.backend, model: data.current.model };
      effort = data.current.effort || effort;
    }
    render();
  }).catch((err) => showListError(errEl, modelsEl, err, !!data));
}

/* The model control in an agent's chat --------------------------------- */

/** "Codex · Sol 6.1 · high" */
const chatLabel = (a) => modelText(a.backend, a.modelName, a.effort);

/**
 * The harness, model and effort one agent's chat runs on, next to its
 * composer, and a sheet to switch the model and effort for that chat only.
 * The dashboard reopens the same conversation on the new ones (POST
 * /api/app/agent-model, native deliver_model_request), so the agent's next
 * turn uses them. It stays on its harness, and it does not switch in the
 * middle of a turn. `agent()` returns the agent's current row. Call refresh()
 * when it changes.
 */
export function mountChatModelControl(container, { workspace, run, agent }) {
  const el = h(CHIP);
  const refresh = () => {
    const a = agent();
    // The sheet switches among its harness's models: none for a harness the
    // list lacks. An agent started without a model shows what it runs.
    el.hidden = !a || !harnessOf(modelsCache, a.backend);
    if (el.hidden) return;
    paintChip(el, { backend: a.backend, name: a.modelName, effort: a.effort });
    el.setAttribute("aria-label", `This chat runs on ${chatLabel(a)}. Change`);
    el.title = a.model ? `This chat runs on ${chatLabel(a)} (${a.model})` : `This chat runs on ${chatLabel(a)}`;
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

  // Its harness's list, led by the model the agent runs on when the list
  // does not have it (an id it was started with by hand, or one its CLI
  // no longer lists), under the name the cloud machine gave it.
  const options = () => {
    const opts = rowsOf(data, backend);
    return !start.model || opts.some((o) => o.value === start.model) ? opts : [{ value: start.model, label: start.modelName, detail: "", efforts: [] }, ...opts];
  };
  const rowOf = (opts, m) => (opts.some((o) => o.value === m) ? m : null);
  function render() {
    const a = agent() || start;
    const busy = a.status === "working";
    setText(note, busy
      ? "It is working right now. Switch when this turn ends."
      : "The conversation carries on. Its next reply uses the new model.");
    const harness = harnessOf(data, backend);
    if (!harness) return;
    const opts = options();
    const current = rowOf(opts, a.model);
    model ??= rowOf(opts, start.model);
    paintModels(modelsEl, [{ harness, models: opts }], {
      selected: { backend, model }, current: { backend, model: current },
      onPick: (_, v) => { model = v; render(); }, reveal: !revealed,
    });
    revealed = true;
    const row = opts.find((o) => o.value === model);
    const eff = effortFor(row, effort);
    const dial = paintEfforts(effortWrap, effortsEl, { row, effort: eff, onPick: (v) => { effort = v; render(); } });
    const same = model === current && (!dial || eff === (a.effort || "auto"));
    useBtn.disabled = busy || same;
    setText(useBtn, same ? "Current model" : `Switch to ${[row.label, effortWord(eff)].filter(Boolean).join(" · ")}`);
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
      if (a) { a.model = model; a.modelName = row.label; a.effort = eff === "auto" ? null : eff; }
      dlg.close();
      toast(res?.status === "queued" ? "Switching when the cloud machine picks it up" : `This chat now runs on ${modelText(backend, row.label)}`);
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
