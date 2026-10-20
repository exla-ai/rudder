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
// The dashboard's effort list (models.rs effort_options_for): Claude adds max.
const EFFORTS = [
  { id: "auto", label: "Auto", detail: "the agent decides" },
  { id: "low", label: "Low", detail: "fastest" },
  { id: "medium", label: "Medium", detail: "balanced" },
  { id: "high", label: "High", detail: "deeper reasoning" },
  { id: "xhigh", label: "Extra high", detail: "extended reasoning" },
  { id: "max", label: "Max", detail: "maximum reasoning", only: "claude" },
];
const effortsFor = (backend) => EFFORTS.filter((e) => !e.only || e.only === backend);
// Mirrors model_supports_reasoning (native/src/models.rs): only models with
// an effort dial get the effort chips.
const reasons = (backend, model) => {
  const m = String(model || "").toLowerCase();
  if (backend === "claude") return /opus|sonnet|fable/.test(m) || (m.startsWith("claude-") && !m.includes("haiku") && !m.includes("3-"));
  if (backend === "codex") return /^gpt|codex|^o\d/.test(m);
  return false;
};

let modelsCache = null; // { at, data }
async function loadModels(force = false) {
  if (!force && modelsCache && Date.now() - modelsCache.at < 5 * 60_000) return modelsCache.data;
  const data = (await api("/api/app/models")) || {};
  modelsCache = { at: Date.now(), data };
  return data;
}

const pickKey = (ws) => `rudder.model.${ws}`;
/** What this device last chose for the project, or the dashboard's current model. */
function currentChoice(ws) {
  const mine = ls.get(pickKey(ws));
  if (mine?.backend && mine?.model) return mine;
  const cur = modelsCache?.data?.current;
  if (cur?.backend && cur?.model) {
    const opt = (modelsCache.data[cur.backend] || []).find((o) => o.value === cur.model);
    return { backend: cur.backend, model: cur.model, label: opt?.label || cur.model, effort: cur.effort || null };
  }
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
  if (!modelsCache) loadModels().then(refresh).catch(() => {});
  return { el, refresh };
}

export function openModelPicker(ws, { onPicked } = {}) {
  const dlg = h(`<dialog class="sheet add-sheet model-sheet" aria-label="Model for new agents"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3>Model for new agents</h3><button type="button" class="icon-btn" data-act="close" aria-label="Close">${icon("x")}</button></div>
    <div class="seg modes mp-providers" role="tablist" aria-label="Agent"></div>
    <div class="mp-account"></div>
    <div class="mp-models" role="radiogroup" aria-label="Model"><div class="repo-note">Loading models…</div></div>
    <div class="mp-effort-wrap">
      <div class="field-label" id="mp-effort-l">Effort</div>
      <div class="mp-efforts" role="radiogroup" aria-labelledby="mp-effort-l"></div>
    </div>
    <button type="button" class="btn accent block mp-use" disabled>Use this model</button>
    <div class="create-note">Applies to agents you start next. Running agents keep theirs.</div>
  </div></dialog>`);
  const provEl = dlg.querySelector(".mp-providers");
  const modelsEl = dlg.querySelector(".mp-models");
  const effortWrap = dlg.querySelector(".mp-effort-wrap");
  const effortsEl = dlg.querySelector(".mp-efforts");
  const useBtn = dlg.querySelector(".mp-use");
  let data = modelsCache?.data || null;
  const start = currentChoice(ws);
  let backend = start?.backend === "codex" ? "codex" : "claude";
  let model = start?.model || null;
  let effort = start?.effort || "auto";

  const options = () => (data?.[backend] || []).filter((o) => o && o.value);
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
        if (!effortsFor(backend).some((e) => e.id === effort)) effort = "auto";
        render();
      });
      return b;
    }));
    setText(dlg.querySelector(".mp-account"), accountLine(backend));
    if (!data) return;
    const opts = options();
    if (model && !opts.some((o) => o.value === model)) model = opts[0]?.value || null;
    if (!model) model = opts[0]?.value || null;
    modelsEl.replaceChildren();
    for (const o of opts) {
      const current = data.current?.backend === backend && data.current?.model === o.value;
      const b = h(`<button type="button" role="radio" class="mp-model"><span class="mp-mt"><span class="mp-name"></span><span class="mp-badge" hidden>Current</span></span><span class="mp-detail"></span><span class="mp-radio">${icon("check")}</span></button>`);
      setText(b.querySelector(".mp-name"), o.label || o.value);
      b.querySelector(".mp-badge").hidden = !current;
      const detail = [o.detail, o.value !== o.label ? o.value : null].filter(Boolean).join(" · ");
      setText(b.querySelector(".mp-detail"), detail);
      b.setAttribute("aria-checked", String(o.value === model));
      b.addEventListener("click", () => { model = o.value; render(); });
      modelsEl.appendChild(b);
    }
    if (!opts.length) {
      const note = h(`<div class="repo-note"></div>`);
      note.textContent = `No ${providerLabel(backend)} models found on the cloud machine.`;
      modelsEl.appendChild(note);
    }
    const dial = !!model && reasons(backend, model);
    effortWrap.hidden = !dial;
    effortsEl.replaceChildren(...effortsFor(backend).map((e) => {
      const b = h(`<button type="button" role="radio" class="mp-effort"><span></span></button>`);
      setText(b.querySelector("span"), e.label);
      b.title = e.detail;
      b.setAttribute("aria-checked", String(e.id === effort));
      b.addEventListener("click", () => { effort = e.id; render(); });
      return b;
    }));
    useBtn.disabled = !model;
    const label = opts.find((o) => o.value === model)?.label || model;
    setText(useBtn, model ? `Use ${providerLabel(backend)} · ${label}${dial && effort !== "auto" ? ` · ${effortLabel(effort).toLowerCase()}` : ""}` : "Use this model");
  }

  useBtn.addEventListener("click", async () => {
    if (!model) return;
    const dial = reasons(backend, model);
    const eff = dial ? effort : null;
    useBtn.disabled = true;
    try {
      // Exactly the dashboard grammar: /model <provider> <model> [effort].
      const text = `/model ${backend} ${model}${eff ? ` ${eff}` : ""}`;
      await api("/api/app/task", { method: "POST", body: { workspace: ws, text } });
      const label = options().find((o) => o.value === model)?.label || model;
      const choice = { backend, model, label, effort: eff };
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
  loadModels(true).then((res) => {
    data = res;
    if (!start) {
      const cur = data.current;
      if (cur?.backend === "claude" || cur?.backend === "codex") { backend = cur.backend; model = cur.model; }
    }
    render();
  }).catch((err) => {
    modelsEl.replaceChildren(h(`<div class="repo-note err"></div>`));
    modelsEl.firstChild.textContent = err.status === 401 ? "Signed out." : err.message;
  });
}
