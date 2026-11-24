// Which harness, model and effort new agents in a project start on. Choosing
// sends the dashboard `/model <harness> <model> <effort>`, exactly what
// typing it in the dashboard's task bar does (effort "auto" lets the agent
// decide; native/src/main.rs handle_command).
import { h, icon, setText } from "/app/render.js";
import { app, api, coarse, ls, showError, toast, track, accountCanRun, accountUsageLabel } from "/app/core.js";
import { harnessLogo } from "/app/harness-logos.js";
import { UpdateNow } from "/app/home-update.js";
import { antigravityPoolOf, groupAntigravityEfforts, matchesModel, variantEffort, modelRequest } from "./model-effort.js";

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
/** Antigravity offers only the concrete variants returned by its CLI. */
const effortsOf = (row) => (row?.efforts?.length ? EFFORTS.filter((e) => (e.id === "auto" && !row.variants) || row.efforts.includes(e.id)) : []);
/** `effort` when `row` takes it, else Auto. */
function effortFor(row, effort, model) {
  if (effortsOf(row).some((e) => e.id === effort)) return effort;
  if (!row?.variants) return "auto";
  return variantEffort(row, model) ?? (row.variants.medium ? "medium" : row.efforts[0]);
}
const effortLabel = (e) => EFFORTS.find((x) => x.id === e)?.label || "";
/** How an effort reads after a model's name ("high"); Auto and none read as nothing. */
/** The effort as the chat's effort chip names it ("Low", "Extra high"); none for Auto. */
const effortWord = (e) => (e && e !== "auto" ? effortLabel(e) : null);

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
let modelsRefreshFetch = null;
let modelsRequestVersion = 0;
function storeModels(data) {
  const changed = JSON.stringify(data) !== JSON.stringify(modelsCache);
  modelsCache = data;
  modelsFresh = true;
  ls.set(MODELS_KEY, modelsCache);
  // Agent rows on screen read their model names from the list.
  if (changed) app.updateScreen();
  return modelsCache;
}
function loadModels({ refresh = false } = {}) {
  modelsAsked = true;
  if (refresh) {
    modelsRefreshFetch ??= (() => {
      const version = ++modelsRequestVersion;
      return api("/api/app/models?refresh=true").then((data) => {
        if (version === modelsRequestVersion) storeModels(data);
        return data;
      }).finally(() => { modelsRefreshFetch = null; });
    })();
    return modelsRefreshFetch;
  }
  if (modelsRefreshFetch) return modelsRefreshFetch;
  modelsFetch ??= (() => {
    const version = ++modelsRequestVersion;
    return api("/api/app/models").then((data) => {
      if (version === modelsRequestVersion) storeModels(data);
      return data;
    }).finally(() => { modelsFetch = null; });
  })();
  return modelsFetch;
}
const harnessesOf = (data) => (data?.harnesses || []).map((harness) => harness.id === "antigravity"
  ? { ...harness, models: groupAntigravityEfforts(harness.models) }
  : harness);
const harnessOf = (data, id) => harnessesOf(data).find((x) => x.id === id) || null;
const rowsOf = (data, backend) => harnessOf(data, backend)?.models || [];
const rowFor = (backend, model) => rowsOf(modelsCache, backend).find((o) => matchesModel(o, model)) || null;

/**
 * A harness's name ("Claude"), from the list; one the list does not have
 * reads as its id. Before this device has a list, none: the first
 * call fetches it, and the screen repaints when it lands.
 */
function harnessLabel(backend) {
  if (!modelsCache && !modelsAsked) loadModels().catch(() => {});
  return modelsCache ? harnessOf(modelsCache, backend)?.label ?? backend : null;
}

/** A model in words, harness first: "Claude · Opus 5.5 · High". Any part may be missing. */
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
 * effort: [logo] Opus 5.5 · High. The logo says which harness it is, so the
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
    const row = rowFor(mine.backend, mine.model);
    const effort = variantEffort(row, mine.model);
    return { backend: mine.backend, model: mine.model, effort: mine.effort || null, name: effort ? row.variants[effort].label : row?.label ?? mine.name };
  }
  const cur = modelsCache?.current;
  if (cur?.backend && cur?.model && cur?.name) return { backend: cur.backend, model: cur.model, effort: cur.effort || null, name: cur.name };
  return null;
}
/** "Codex · Sol 6.1 · High" */
function choiceLabel(c) {
  return c ? modelText(c.backend, c.name, c.effort) : "Choose model";
}

// A harness's accounts on the cloud machine, as its last sign-in check found
// them: "main" (the CLI's own) first. Claude Code and Codex can hold several
// (src/accounts.ts), each its own sign-in; an agent runs on one of them.
const MAIN = "main";
/** The machine's accounts of a harness, or [] before its first check. */
const accountsOf = (backend) => app.store?.state?.home?.credentials?.[backend] ?? [];
const accountOf = (backend, id) => accountsOf(backend).find((a) => a.id === id) ?? null;
/** The account new agents on a harness start on. */
const defaultAccount = (backend) => accountsOf(backend).find((a) => a.default)?.id ?? MAIN;
/** Why the account cannot run an agent, on `model` when one is named; "" when it can. */
function accountRefusal(backend, id, model = null) {
  const account = accountOf(backend, id);
  if (!account) return "";
  if (accountCanRun(account)) return model ? poolRefusal(account, model) : "";
  return accountUsageLabel(account) || (account.signedIn
    ? account.detail || "Could not check usage for this account"
    : "Sign in to this account from Settings");
}

// Antigravity's models draw on separate pools (the account's `modelPools`,
// cloud/worker/home.mjs checkAntigravity): a spent pool stops its own models
// and leaves the rest of the account working.
function poolRefusal(account, model) {
  const pool = antigravityPoolOf(model, account.modelPools ?? []);
  if (!pool?.spent || (pool.resetsAt !== null && pool.resetsAt <= Date.now())) return "";
  const said = /\d/.test(pool.name) ? `${pool.name} is used up` : `${pool.name} models are used up`;
  if (pool.resetsAt === null) return said;
  const when = new Intl.DateTimeFormat(undefined, {
    month: "short", day: "numeric", hour: "numeric", minute: "2-digit", timeZoneName: "short",
  }).format(pool.resetsAt);
  return `${said} · Resets ${when}`;
}

// Use the preferred account again after its allowance resets.
function availableDefault(backend) {
  const preferred = defaultAccount(backend);
  if (!accountUsageLabel(accountOf(backend, preferred))) return preferred;
  return accountsOf(backend).find(accountCanRun)?.id ?? preferred;
}
/** An account in words: its email, else which one it is. */
const accountName = (backend, id) => {
  const list = accountsOf(backend);
  const account = list.find((a) => a.id === id);
  return account?.account || (id === MAIN ? "Main account" : `Account ${list.indexOf(account) + 1}`);
};

function accountLine(harness, id, model = null) {
  const cred = accountOf(harness.id, id);
  if (!cred) return `Runs on your own ${harness.account} subscription`;
  if (cred.ok || cred.signedIn) return [
    cred.account,
    cred.plan ? `${PLAN_LABEL[cred.plan] || cred.plan} plan` : null,
    accountRefusal(harness.id, id, model),
  ].filter(Boolean).join(" · ") || "Signed in";
  return cred.account ? `${cred.account} · sign in again on the cloud machine` : `Not signed in on the cloud machine`;
}

/**
 * Under the harness tabs: the account its agents run on. One account is a
 * line; several are a choice, `selected` checked, the signed-out ones off.
 */
function paintAccounts(el, harness, { selected, onPick, model = null }) {
  const list = accountsOf(harness.id);
  const addable = ADDABLE.has(harness.id);
  el.hidden = false;
  if (list.length < 2 && !(addable && list.length)) {
    el.classList.remove("mp-accounts");
    el.removeAttribute("role");
    el.removeAttribute("aria-label");
    setText(el, accountLine(harness, selected, model));
    return;
  }
  el.classList.add("mp-accounts");
  el.setAttribute("role", "radiogroup");
  el.setAttribute("aria-label", `${harness.label} account`);
  el.replaceChildren(...list.map((a) => {
    const b = h(`<button type="button" role="radio" class="mp-acct"><span class="mp-acct-n"></span><span class="mp-acct-p"></span></button>`);
    setText(b.querySelector(".mp-acct-n"), accountName(harness.id, a.id));
    const refusal = accountRefusal(harness.id, a.id);
    const label = accountUsageLabel(a) ? "At limit" : refusal ? (a.signedIn ? "Check failed" : "Signed out") : PLAN_LABEL[a.plan] || a.plan || "";
    setText(b.querySelector(".mp-acct-p"), label);
    b.setAttribute("aria-checked", String(a.id === selected));
    b.disabled = Boolean(refusal);
    b.title = accountLine(harness, a.id);
    b.addEventListener("click", () => onPick(a.id));
    return b;
  }), ...(addable ? [addAccountChip(harness)] : []));
}

// Claude Code and Codex hold several accounts, each its own sign-in
// (src/accounts.ts); the "+" after the chips adds one (app.js openAgentSignIn).
const ADDABLE = new Set(["claude", "codex"]);
function addAccountChip(harness) {
  const b = h(`<button type="button" class="mp-acct mp-acct-add">${icon("plus")}<span>Add account</span></button>`);
  b.setAttribute("aria-label", `Add another ${harness.label} account`);
  b.title = `Sign in to another ${harness.label} account`;
  b.addEventListener("click", () => app.openAgentSignIn(harness.id, "new"));
  return b;
}

/**
 * The harness tabs over a model sheet's list, the Add project sheet's
 * segmented control (`.seg.modes`): each harness's logo and name, `tab`
 * selected. The buttons stay put while the list and labels do, so a tab keeps
 * focus when it is picked from the keyboard.
 */
function paintTabs(tabsEl, harnesses, { tab, onTab }) {
  const key = harnesses.map((x) => `${x.id}:${x.label}`).join(" ");
  if (tabsEl.dataset.key !== key) {
    tabsEl.dataset.key = key;
    tabsEl.replaceChildren(...harnesses.map((x) => {
      const b = h(`<button type="button" role="tab">${harnessLogo(x.id)}<span></span></button>`);
      setText(b.querySelector("span"), x.label);
      b.dataset.harness = x.id;
      b.addEventListener("click", () => onTab(x.id));
      return b;
    }));
  }
  for (const b of tabsEl.children) b.setAttribute("aria-selected", String(b.dataset.harness === tab));
}

/**
 * One harness's models, the aliases last under "Always the newest".
 * `selected` is checked and `current` badged
 * ({ backend, model } each), whichever tab they are on. Keeps the list's
 * scroll while the tab stays; a new tab starts at the top. `reveal` scrolls
 * the checked row into view.
 */
function paintModels(modelsEl, section, { selected, current, onPick, reveal, query = "" }) {
  const { harness, models } = section;
  const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  const shown = words.length ? models.filter((o) => words.every((w) => `${o.label} ${o.value} ${o.detail || ""}`.toLowerCase().includes(w))) : models;
  // A new search starts at the top, like a new tab.
  const top = modelsEl.dataset.harness === harness.id && modelsEl.dataset.query === query ? modelsEl.scrollTop : 0;
  modelsEl.dataset.harness = harness.id;
  modelsEl.dataset.query = query;
  modelsEl.setAttribute("aria-label", `${harness.label} models`);
  modelsEl.replaceChildren();
  let aliases = false;
  for (const o of shown) {
    if (o.alias && !aliases) {
      aliases = true;
      modelsEl.appendChild(h(`<div class="mp-group" role="presentation">Always the newest</div>`));
    }
    const b = h(`<button type="button" role="radio" class="mp-model"><span class="mp-mt"><span class="mp-name"></span><span class="mp-badge" hidden>Current</span></span><span class="mp-detail"><span class="mp-id"></span><span class="mp-desc"></span></span><span class="mp-radio">${icon("check")}</span></button>`);
    setText(b.querySelector(".mp-name"), o.label);
    b.querySelector(".mp-badge").hidden = !(current?.backend === harness.id && current.model === o.value);
    setText(b.querySelector(".mp-id"), o.family ?? o.value);
    setText(b.querySelector(".mp-desc"), o.detail || "");
    b.title = [o.family ?? o.value, o.detail].filter(Boolean).join(" · ");
    b.setAttribute("aria-label", `${harness.label} ${o.label}`);
    b.setAttribute("aria-checked", String(selected?.backend === harness.id && selected.model === o.value));
    b.addEventListener("click", () => onPick(harness.id, o.value));
    modelsEl.appendChild(b);
  }
  if (!shown.length) {
    const note = h(`<div class="repo-note"></div>`);
    note.textContent = models.length ? `No ${harness.label} model matches "${query.trim()}".` : `No ${harness.label} models found on the cloud machine.`;
    modelsEl.appendChild(note);
  }
  modelsEl.scrollTop = top;
  if (reveal && !words.length) modelsEl.querySelector('[aria-checked="true"]')?.scrollIntoView({ block: "nearest" });
}

// OpenCode lists every provider's models (over 400 on a cloud machine), so
// its tab has a search over them.
const SEARCHED = new Set(["opencode"]);

/**
 * The search between a sheet's accounts and its model list, on a SEARCHED
 * harness's tab. Typing filters the list by name, provider/model id and
 * detail; Escape clears it before it would close the sheet; Enter picks the
 * first match. A new tab clears it. While it has text, the list keeps the
 * height it had with every model, so the sheet does not shrink and move the
 * field under the cursor.
 */
function modelSearch(dlg, modelsEl, onInput) {
  const wrap = dlg.querySelector(".mp-search");
  const input = wrap.querySelector("input");
  const hold = () => {
    if (!input.value) modelsEl.style.minHeight = "";
    else if (!modelsEl.style.minHeight) modelsEl.style.minHeight = `${modelsEl.getBoundingClientRect().height}px`;
  };
  input.addEventListener("input", () => { hold(); onInput(); });
  input.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && input.value) {
      e.preventDefault();
      input.value = "";
      hold();
      onInput();
    } else if (e.key === "Enter") {
      e.preventDefault();
      modelsEl.querySelector(".mp-model")?.click();
    }
  });
  return {
    query: () => (wrap.hidden ? "" : input.value),
    paint(harness, count) {
      wrap.hidden = !SEARCHED.has(harness.id);
      input.placeholder = `Search ${count} ${harness.label} models`;
      input.setAttribute("aria-label", `Search ${harness.label} models`);
    },
    // A tab picked: an empty search, focused on a SEARCHED one (not on a
    // phone, where it would raise the keyboard over the list).
    tab(id) {
      input.value = "";
      hold();
      if (SEARCHED.has(id) && !coarse) requestAnimationFrame(() => input.focus());
    },
  };
}

/** The list with no harness in it: no tabs, a note. */
function paintNoModels(tabsEl, accountEl, modelsEl) {
  tabsEl.replaceChildren();
  tabsEl.dataset.key = "";
  accountEl.hidden = true;
  delete modelsEl.dataset.harness;
  modelsEl.replaceChildren(h(`<div class="repo-note">No models found on the cloud machine.</div>`));
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
    const row = c && rowFor(c.backend, c.model);
    paintChip(el, c ? { backend: c.backend, name: row?.label ?? c.name, effort: compact ? null : variantEffort(row, c.model) ?? c.effort } : { backend: null, name: "Choose model" });
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
    <div class="seg modes mp-tabs" role="tablist" aria-label="Harness"></div>
    <div class="mp-account" hidden></div>
    <label class="search-wrap mp-search" hidden>${icon("search")}<input class="input repo-search" type="search" autocapitalize="off" autocorrect="off" spellcheck="false" autocomplete="off"></label>
    <div class="mp-models" role="radiogroup" aria-label="Model"><div class="repo-note">Loading models…</div></div>
    <div class="mp-effort-wrap">
      <div class="field-label" id="mp-effort-l">Effort</div>
      <div class="mp-efforts" role="radiogroup" aria-labelledby="mp-effort-l"></div>
    </div>
    <button type="button" class="btn accent block mp-use" disabled>Use this model</button>
    <div class="create-note">Applies to agents you start next. Running agents keep theirs.</div>
  </div></dialog>`);
  const errEl = dlg.querySelector(".mp-error");
  const tabsEl = dlg.querySelector(".mp-tabs");
  const accountEl = dlg.querySelector(".mp-account");
  const modelsEl = dlg.querySelector(".mp-models");
  const effortWrap = dlg.querySelector(".mp-effort-wrap");
  const effortsEl = dlg.querySelector(".mp-efforts");
  const useBtn = dlg.querySelector(".mp-use");
  let data = modelsCache;
  const start = currentChoice(ws);
  let pick = start && { backend: start.backend, model: start.model };
  let effort = start?.effort || null;
  // The account picked per harness; until a pick, the harness's default.
  const picked = {};
  const accountFor = (backend) => picked[backend] ?? availableDefault(backend);
  // The harness tab shown: the picked model's until another is chosen.
  let tab = pick?.backend ?? null;
  let revealed = false;
  const search = modelSearch(dlg, modelsEl, () => render());
  const showTab = (id) => { tab = id; revealed = false; search.tab(id); render(); };

  const pickedRow = () => (pick ? rowsOf(data, pick.backend).find((o) => matchesModel(o, pick.model)) || null : null);
  function render() {
    if (!data) return;
    // A model the list does not have starts on its harness's first model.
    if (!pickedRow()) {
      const harness = harnessOf(data, pick?.backend) || harnessesOf(data)[0];
      pick = harness?.models.length ? { backend: harness.id, model: harness.models[0].value } : null;
    }
    const current = data.current?.backend && data.current?.model ? {
      ...data.current, model: rowsOf(data, data.current.backend).find((row) => matchesModel(row, data.current.model))?.value ?? data.current.model,
    } : null;
    const harnesses = harnessesOf(data);
    // A tab the list no longer has opens on the picked model's harness.
    if (!harnessOf(data, tab)) tab = pick?.backend ?? harnesses[0]?.id ?? null;
    if (harnesses.length) {
      paintTabs(tabsEl, harnesses, { tab, onTab: showTab });
      const harness = harnessOf(data, tab);
      paintAccounts(accountEl, harness, { selected: accountFor(harness.id), model: pick?.backend === harness.id ? pick.model : null, onPick: (id) => { picked[harness.id] = id; render(); } });
      search.paint(harness, harness.models.length);
      paintModels(modelsEl, { harness, models: harness.models }, { selected: pick && { ...pick, model: pickedRow()?.value }, current, onPick: (backend, model) => {
        if (backend !== pick?.backend || model !== pickedRow()?.value) {
          const row = rowsOf(data, backend).find((entry) => entry.value === model);
          pick = { backend, model: row?.variants?.medium?.value ?? model };
        }
        render();
      }, reveal: !revealed, query: search.query() });
    } else paintNoModels(tabsEl, accountEl, modelsEl);
    revealed = true;
    const row = pickedRow();
    const eff = effortFor(row, effort, pick?.model);
    const dial = paintEfforts(effortWrap, effortsEl, { row, effort: eff, onPick: (v) => { effort = v; render(); } });
    useBtn.disabled = !pick || Boolean(accountRefusal(pick.backend, accountFor(pick.backend), pick.model));
    setText(useBtn, pick ? `Use ${modelText(pick.backend, row.label, dial ? eff : null)}` : "Use this model");
  }

  useBtn.addEventListener("click", async () => {
    if (!pick) return;
    const { backend } = pick;
    const row = pickedRow();
    const eff = effortsOf(row).length ? effortFor(row, effort, pick.model) : null;
    useBtn.disabled = true;
    try {
      // New agents on a harness start on its default account (native reads
      // it at launch), so picking another account here makes it the default.
      const account = accountFor(backend);
      if (picked[backend] && account !== defaultAccount(backend)) {
        await api("/api/app/agent-account", { method: "POST", body: { provider: backend, account, action: "default" } });
      }
      // Exactly the dashboard grammar: /model <harness> <model> [effort].
      const choice = { backend, ...modelRequest(row, eff) };
      const text = `/model ${backend} ${choice.model}${choice.effort ? ` ${choice.effort}` : ""}`;
      await api("/api/app/task", { method: "POST", body: { workspace: ws, text } });
      ls.set(pickKey(ws), choice);
      track("model_picked", { backend, model: choice.model, effort: eff || "none" });
      dlg.close();
      const as = accountsOf(backend).length > 1 ? ` as ${accountName(backend, account)}` : "";
      toast(`New agents start on ${choiceLabel(choice)}${as}`);
      app.refreshState();
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
  loadModels({ refresh: true }).then((res) => {
    data = res;
    errEl.hidden = true;
    if (!start && data.current?.backend && data.current?.model) {
      pick = { backend: data.current.backend, model: data.current.model };
      effort = data.current.effort || effort;
      tab = pick.backend;
      revealed = false;
    }
    render();
  }).catch((err) => showListError(errEl, modelsEl, err, !!data));
}

/* The model control in an agent's chat --------------------------------- */

/** "Codex · Sol 6.1 · High" */
const chatLabel = (a) => modelText(a.backend, a.modelName, a.effort);

/**
 * The harness, model and effort one agent's chat runs on, next to its
 * composer, and a sheet to switch the model and effort for that chat only.
 * A model of its own harness: the dashboard reopens the same conversation on
 * it (POST /api/app/agent-model, native deliver_model_request), so the
 * agent's next turn uses it, and not in the middle of a turn. A model of
 * another harness moves the chat there (openHandoffDialog). `agent()`
 * returns the agent's current row. Call refresh() when it changes.
 */
export function mountChatModelControl(container, { workspace, run, agent }) {
  const el = h(CHIP);
  const effortEl = h(EFFORT_CHIP);
  const refresh = () => {
    const a = agent();
    // The sheet switches among its harness's models: none for a harness the
    // list lacks. An agent started without a model shows what it runs.
    el.hidden = !a || !harnessOf(modelsCache, a.backend);
    // The effort has a chip of its own: one for a model that takes any.
    const row = el.hidden ? null : rowFor(a.backend, a.model);
    effortEl.hidden = !effortsOf(row).length;
    if (el.hidden) return;
    paintChip(el, { backend: a.backend, name: row?.label ?? a.modelName, effort: null });
    el.setAttribute("aria-label", `This chat runs on ${chatLabel(a)}. Change`);
    el.title = a.model ? `This chat runs on ${chatLabel(a)} (${a.model})` : `This chat runs on ${chatLabel(a)}`;
    // As the model sheet reads it: an effort the model does not list is Auto.
    if (!effortEl.hidden) paintEffortChip(effortEl, effortFor(row, a.effort, a.model));
  };
  el.addEventListener("click", () => openChatModelPicker({ workspace, run, agent, onSwitched: refresh }));
  effortEl.addEventListener("click", () => {
    if (closeEffortMenu) { closeEffortMenu(); return; }
    openEffortMenu(effortEl, { workspace, run, agent, onSwitched: refresh });
  });
  container.append(el, effortEl);
  refresh();
  // The label reads the list; a failed fetch shows when the sheet opens.
  if (!modelsFresh) loadModels().then(refresh).catch(() => {});
  return { el, refresh };
}

/**
 * Switch an agent's chat to another model of its own harness, or the same
 * model at another effort: the dashboard reopens the conversation on it
 * (POST /api/app/agent-model). The model sheet's Switch and the effort chip
 * both land here. Throws what the request threw.
 */
async function switchChatModel({ workspace, run, agent, backend, row, effort, account }) {
  const request = modelRequest(row, effort);
  const { model } = request;
  const res = await api("/api/app/agent-model", { method: "POST", body: { workspace, run, model, effort: request.effort ?? "auto", account } });
  track("chat_model_switched", { backend, model, effort });
  // The dashboard took it (or will within seconds): show it now, the next
  // state refresh carries the real row.
  const a = agent();
  const moved = a && (a.account ?? MAIN) !== account;
  if (a) { a.model = model; a.modelName = request.name; a.effort = request.effort === "auto" ? null : request.effort; a.account = account; }
  const as = moved ? ` as ${accountName(backend, account)}` : "";
  toast(res?.status === "queued" ? "Switching when the cloud machine picks it up" : `This chat now runs on ${modelText(backend, row.label, effort)}${as}`);
  app.refreshState();
}

/* The effort chip beside the model chip -------------------------------- */

// A dial whose needle leans further right the harder the agent thinks; Auto
// points straight up.
const NEEDLE = { auto: 0, low: -60, medium: -25, high: 25, xhigh: 50, max: 70 };
const GAUGE = `<svg class="ec-gauge" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3.5 17a8.5 8.5 0 0 1 17 0"/><path class="ec-needle" d="M12 17V10"/><circle cx="12" cy="17" r="1.2" fill="currentColor" stroke="none"/></svg>`;
const EFFORT_CHIP = `<button type="button" class="model-chip effort-chip" aria-haspopup="menu" aria-expanded="false">${GAUGE}<span class="mc-n"></span>${icon("chevron").replace("<svg", '<svg class="mc-caret"')}</button>`;

function paintEffortChip(el, effort) {
  const label = effortLabel(effort);
  setText(el.querySelector(".mc-n"), label);
  el.querySelector(".ec-needle").setAttribute("transform", `rotate(${NEEDLE[effort]} 12 17)`);
  el.setAttribute("aria-label", `Effort: ${label}. Change`);
  el.title = `Effort: ${label}`;
}

let closeEffortMenu = null;

/**
 * The effort levels of the chat's model, as a menu above the effort chip.
 * Up and Down move, Enter picks, Esc or a click elsewhere closes. A pick
 * switches the chat the way the model sheet does (switchChatModel). It lives
 * in the composer, like the attach menu, so it rides up with the keyboard.
 */
function openEffortMenu(chip, { workspace, run, agent, onSwitched }) {
  const a = agent();
  const row = a && rowFor(a.backend, a.model);
  const choices = effortsOf(row);
  if (!choices.length) return;
  const current = effortFor(row, a.effort, a.model);
  const busy = a.status === "working";
  const composer = chip.closest(".composer");
  const menu = h(`<div class="menu effort-menu" role="menu" aria-label="Effort"></div>`);
  if (busy) {
    const note = h(`<div class="em-note"></div>`);
    setText(note, "It is working right now. Change the effort when this turn ends.");
    menu.appendChild(note);
  }
  for (const e of choices) {
    const b = h(`<button type="button" role="menuitemradio"><span class="m-label"><span></span><small></small></span>${icon("check").replace("<svg", '<svg class="em-ck"')}</button>`);
    setText(b.querySelector(".m-label > span"), e.label);
    setText(b.querySelector("small"), e.detail);
    b.setAttribute("aria-checked", String(e.id === current));
    b.disabled = busy;
    b.addEventListener("click", async () => {
      close();
      if (e.id === current) return;
      chip.disabled = true;
      try {
        await switchChatModel({ workspace, run, agent, backend: a.backend, row, effort: e.id, account: a.account ?? MAIN });
        onSwitched();
      } catch (err) { showError(err); }
      finally { chip.disabled = false; }
    });
    menu.appendChild(b);
  }
  const buttons = () => [...menu.querySelectorAll("button:not([disabled])")];
  function close(refocus = false) {
    if (closeEffortMenu !== close) return;
    closeEffortMenu = null;
    menu.remove();
    chip.setAttribute("aria-expanded", "false");
    document.removeEventListener("pointerdown", outside, true);
    document.removeEventListener("keydown", onKey, true);
    removeEventListener("resize", onAway);
    if (refocus) chip.focus();
  }
  const onAway = () => close();
  const outside = (e) => { if (!menu.contains(e.target) && !chip.contains(e.target)) close(); };
  const onKey = (e) => {
    if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); close(true); return; }
    if (e.key === "Tab") { close(); return; }
    const list = buttons();
    if (!list.length) return;
    const at = list.indexOf(document.activeElement);
    const to = { ArrowDown: at + 1, ArrowUp: at - 1, Home: 0, End: list.length - 1 }[e.key];
    if (to === undefined) return;
    e.preventDefault();
    list[(to + list.length) % list.length].focus();
  };
  closeEffortMenu?.();
  composer.appendChild(menu);
  // Above the chip, its left edge on the chip's (kept inside the composer).
  const c = composer.getBoundingClientRect();
  const r = chip.getBoundingClientRect();
  menu.style.left = `${Math.max(8, Math.min(r.left - c.left, c.width - menu.offsetWidth - 8))}px`;
  menu.style.bottom = `${c.bottom - r.top + 6}px`;
  closeEffortMenu = close;
  chip.setAttribute("aria-expanded", "true");
  document.addEventListener("pointerdown", outside, true);
  document.addEventListener("keydown", onKey, true);
  addEventListener("resize", onAway);
  (menu.querySelector('[aria-checked="true"]:not([disabled])') || buttons()[0])?.focus();
}

function openChatModelPicker({ workspace, run, agent, onSwitched }) {
  const start = agent();
  const backend = start.backend;
  const dlg = h(`<dialog class="sheet add-sheet model-sheet" aria-label="Model for this chat"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3>Model for this chat</h3><button type="button" class="icon-btn" data-act="close" aria-label="Close">${icon("x")}</button></div>
    <div class="repo-note err mp-error" hidden></div>
    <div class="seg modes mp-tabs" role="tablist" aria-label="Harness"></div>
    <div class="mp-account" hidden></div>
    <label class="search-wrap mp-search" hidden>${icon("search")}<input class="input repo-search" type="search" autocapitalize="off" autocorrect="off" spellcheck="false" autocomplete="off"></label>
    <div class="mp-models" role="radiogroup" aria-label="Model"><div class="repo-note">Loading models…</div></div>
    <div class="mp-effort-wrap">
      <div class="field-label" id="cm-effort-l">Effort</div>
      <div class="mp-efforts" role="radiogroup" aria-labelledby="cm-effort-l"></div>
    </div>
    <button type="button" class="btn accent block mp-use" disabled>Switch</button>
    <div class="create-note"></div>
  </div></dialog>`);
  const errEl = dlg.querySelector(".mp-error");
  const tabsEl = dlg.querySelector(".mp-tabs");
  const accountEl = dlg.querySelector(".mp-account");
  const modelsEl = dlg.querySelector(".mp-models");
  const effortWrap = dlg.querySelector(".mp-effort-wrap");
  const effortsEl = dlg.querySelector(".mp-efforts");
  const useBtn = dlg.querySelector(".mp-use");
  const note = dlg.querySelector(".create-note");
  let data = modelsCache;
  // The picked row; until a pick, the row the agent runs on.
  let pick = { backend, model: start.model };
  let effort = start.effort || null;
  // The account picked per harness: the chat's own for its harness, the
  // default for the others, until a pick.
  const own = start.account ?? MAIN;
  const picked = { [backend]: own };
  const accountFor = (b) => picked[b] ?? availableDefault(b);
  // The harness tab shown: the chat's own until another is chosen.
  let tab = backend;
  let revealed = false;
  const search = modelSearch(dlg, modelsEl, () => render());
  const showTab = (id) => { tab = id; revealed = false; search.tab(id); render(); };

  // Every harness in the list's order. Its own is led by the model the agent
  // runs on when the list does not have it (an id it was started with by
  // hand, or one its CLI no longer lists), under the name the cloud machine
  // gave it. Picking a model of another harness moves the chat.
  const sections = () => harnessesOf(data).map((harness) => {
    if (harness.id !== backend || !start.model || harness.models.some((o) => matchesModel(o, start.model))) return { harness, models: harness.models };
    return { harness, models: [{ value: start.model, label: start.modelName, detail: "", efforts: [] }, ...harness.models] };
  });
  const rowOf = (p) => sections().find((x) => x.harness.id === p.backend)?.models.find((o) => matchesModel(o, p.model)) || null;
  function render() {
    if (!data) return;
    const a = agent() || start;
    const busy = a.status === "working";
    const moving = pick.backend !== backend;
    const account = accountFor(pick.backend);
    const reaccount = !moving && account !== (a.account ?? MAIN);
    const row = rowOf(pick);
    const eff = effortFor(row, effort, pick.model);
    const all = sections();
    // A tab the list no longer has opens on the first harness it does.
    const section = all.find((x) => x.harness.id === tab) || all[0];
    if (section) {
      tab = section.harness.id;
      paintTabs(tabsEl, all.map((x) => x.harness), { tab, onTab: showTab });
      paintAccounts(accountEl, section.harness, { selected: accountFor(tab), model: pick?.backend === section.harness.id ? pick.model : null, onPick: (id) => { picked[section.harness.id] = id; render(); } });
      search.paint(section.harness, section.models.length);
      paintModels(modelsEl, section, {
        selected: { ...pick, model: row?.value }, current: { backend, model: rowOf({ backend, model: a.model })?.value ?? a.model },
        onPick: (backendId, model) => {
          if (backendId !== pick.backend || model !== row?.value) {
            const destination = rowOf({ backend: backendId, model });
            pick = { backend: backendId, model: destination?.variants?.medium?.value ?? model };
          }
          render();
        }, reveal: !revealed, query: search.query(),
      });
    } else paintNoModels(tabsEl, accountEl, modelsEl);
    revealed = true;
    const dial = paintEfforts(effortWrap, effortsEl, { row, effort: eff, onPick: (v) => { effort = v; render(); } });
    const to = harnessLabel(pick.backend);
    // An account the machine is not signed in to can't take the chat: its
    // agent would fail at once. The home refuses it too.
    // A model whose pool is used up fails at once too, on any account.
    const asked = row ? modelRequest(row, eff).model : null;
    const acct = accountOf(pick.backend, account);
    const blocked = ((moving || reaccount) && accountRefusal(pick.backend, account, asked))
      || (acct && asked ? poolRefusal(acct, asked) : "");
    setText(note, blocked
      ? blocked
      : moving ? `Moves this chat to ${to}, which starts in the same files with a summary of this conversation.`
      : busy ? "It is working right now. Switch when this turn ends."
      : reaccount ? `The conversation carries on as ${accountName(pick.backend, account)}. Its next reply uses that account.`
      : "The conversation carries on. Its next reply uses the new model.");
    note.classList.toggle("mp-blocked", Boolean(blocked));
    const request = row && modelRequest(row, eff);
    const sameModel = request?.model === a.model;
    const same = !moving && !reaccount && sameModel && (!dial || (request.effort ?? "auto") === (a.effort || "auto"));
    const name = row ? [row.label, dial ? effortWord(eff) : null].filter(Boolean).join(" · ") : "";
    useBtn.disabled = !row || same || (busy && !moving) || Boolean(blocked);
    setText(useBtn, !row || same ? "Current model" : moving ? `Move to ${to} · ${name}` : reaccount && sameModel ? `Switch to ${accountName(pick.backend, account)}` : `Switch to ${name}`);
  }

  useBtn.addEventListener("click", async () => {
    const row = rowOf(pick);
    if (!row) return;
    const eff = effortFor(row, effort, pick.model);
    if (pick.backend !== backend) {
      const request = modelRequest(row, eff);
      openHandoffDialog({
        workspace, run, agent,
        to: { backend: pick.backend, ...request, effort: request.effort === "auto" ? null : request.effort, account: accountFor(pick.backend) },
        onMoved: () => { dlg.close(); onSwitched(); },
      });
      return;
    }
    useBtn.disabled = true;
    try {
      await switchChatModel({ workspace, run, agent, backend, row, effort: eff, account: accountFor(backend) });
      dlg.close();
      onSwitched();
    } catch (err) { showError(err); render(); }
  });

  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest('[data-act="close"]')) dlg.close();
  });
  dlg.addEventListener("close", () => dlg.remove());
  document.body.appendChild(dlg);
  dlg.showModal();
  render();
  loadModels({ refresh: true }).then((res) => { data = res; errEl.hidden = true; render(); })
    .catch((err) => showListError(errEl, modelsEl, err, !!data));
}

/* Moving a chat to another harness ------------------------------------- */

/** How the popup's Switch is pressed from the keyboard; nothing on a touch screen. */
const SUBMIT_KEYS = matchMedia("(pointer: coarse)").matches ? "" : /Mac|iP(hone|ad|od)/.test(navigator.platform || navigator.userAgent) ? "⌘ Enter" : "Ctrl Enter";

/**
 * The popup that moves a chat to another harness: where it goes, optional
 * instructions for the summary the new harness starts from, then Switch or
 * Cancel. The cloud machine builds the summary from the conversation and the
 * dashboard starts the new harness in the same agent and workspace (POST
 * /api/app/agent-harness; cloud/worker/handoff.mjs, native agent_model.rs).
 * It opens over the model sheet, so Cancel and Esc go back to it. Enter is a
 * new line; Cmd or Ctrl+Enter switches. `onMoved()` runs once it moved.
 */
function openHandoffDialog({ workspace, run, agent, to, onMoved }) {
  const a = agent();
  const toText = modelText(to.backend, to.name, to.effort);
  const fromHarness = harnessLabel(a.backend);
  const toHarness = harnessLabel(to.backend);
  const dlg = h(`<dialog class="handoff-dialog" aria-labelledby="ho-t"><form class="dialog">
    <div class="ho-route" aria-hidden="true"><span class="ho-end"></span>${icon("chevron").replace("<svg", '<svg class="ho-arrow"')}<span class="ho-end"></span></div>
    <h3 id="ho-t"></h3>
    <p class="ho-sub"></p>
    <label class="field-label" for="ho-ask">Summary instructions <span class="opt">optional</span></label>
    <textarea id="ho-ask" class="ask-other-text" rows="3" placeholder="Focus on the open bug and the files touched"></textarea>
    <p class="ho-hint">Empty gives the default summary: the goal, decisions, where it stopped, files touched and next steps.</p>
    <p class="repo-note err ho-err" role="alert" hidden></p>
    <div class="ho-update" hidden></div>
    <div class="d-actions"><button type="button" class="btn" data-act="cancel">Cancel</button><button type="submit" class="btn accent ho-go"><span class="ho-go-t">Switch</span><kbd class="ho-keys"></kbd></button></div>
  </form></dialog>`);
  const [fromEl, toEl] = dlg.querySelectorAll(".ho-end");
  const end = (el, backend, name) => {
    const logo = harnessLogo(backend);
    el.innerHTML = logo;
    el.append(logo ? name : modelText(backend, name));
  };
  end(fromEl, a.backend, [a.modelName || fromHarness, effortWord(a.effort)].filter(Boolean).join(" · "));
  end(toEl, to.backend, [to.name, effortWord(to.effort)].filter(Boolean).join(" · "));
  setText(dlg.querySelector("h3"), `Move this chat to ${toText}`);
  setText(dlg.querySelector(".ho-sub"), a.status === "working"
    ? `${fromHarness} stops this turn, and ${toHarness} carries on from a summary of the chat, in the same files.`
    : `${fromHarness} stops, and ${toHarness} picks up from a summary of the chat, in the same files.`);
  const form = dlg.querySelector("form");
  const ask = dlg.querySelector("textarea");
  const err = dlg.querySelector(".ho-err");
  const go = dlg.querySelector(".ho-go");
  const goText = dlg.querySelector(".ho-go-t");
  const keys = dlg.querySelector(".ho-keys");
  setText(keys, SUBMIT_KEYS);
  keys.hidden = !SUBMIT_KEYS;
  let updater = null;
  let sending = false;
  ask.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); form.requestSubmit(); }
  });
  form.querySelector('[data-act="cancel"]').addEventListener("click", () => dlg.close());
  dlg.addEventListener("click", (e) => { if (e.target === dlg && !sending) dlg.close(); });
  // Esc cancels, except while it switches: the answer is on its way.
  dlg.addEventListener("cancel", (e) => { if (sending) e.preventDefault(); });
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    if (sending) return;
    sending = true;
    go.disabled = true;
    setText(goText, "Switching…");
    err.hidden = true;
    const instructions = ask.value.trim();
    try {
      const res = await api("/api/app/agent-harness", {
        method: "POST",
        body: { workspace, run, backend: to.backend, model: to.model, effort: to.effort || "auto", account: to.account, instructions },
      });
      // The dashboard took it: show it now, the next state refresh carries the real row.
      const row = agent();
      if (row) Object.assign(row, { backend: to.backend, model: to.model, modelName: to.name, effort: to.effort, account: to.account, status: "working" });
      sending = false;
      dlg.close();
      toast(res?.status === "queued" ? `Moving to ${toText} when the cloud machine picks it up` : `This chat moved to ${toText}`);
      onMoved();
      app.refreshState();
    } catch (error) {
      sending = false;
      go.disabled = false;
      setText(goText, "Switch");
      if (error.needsUpdate) {
        // A cloud machine older than this: its Update now, here.
        if (!updater) {
          const box = dlg.querySelector(".ho-update");
          const why = h(`<p class="ho-why"></p>`);
          setText(why, error.message);
          updater = UpdateNow({ onDone: () => { updater.destroy(); updater = null; box.replaceChildren(); box.hidden = true; } });
          box.append(why, updater.el);
          box.hidden = false;
        }
        return;
      }
      err.textContent = error.status === 401 ? "Signed out." : error.message;
      err.hidden = false;
    }
  });
  dlg.addEventListener("close", () => { updater?.destroy(); dlg.remove(); });
  document.body.appendChild(dlg);
  dlg.showModal();
  ask.focus();
}
