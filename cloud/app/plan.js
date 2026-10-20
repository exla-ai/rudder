// The orchestrator panel: a project's plan (the DAG the dashboard mirrors into
// .rudder/graph.json) as a list grouped by status. A node with an agent opens
// that agent; nothing here edits the plan.
import { h, icon, setText, reconcile } from "/app/render.js";
import { app, api, showError, toast, track } from "/app/core.js";

const NODE_STATUS = {
  running: { label: "Running", cls: "s-working", live: true },
  review: { label: "Review", cls: "s-done" },
  planned: { label: "Planned", cls: "s-idle" },
  ready: { label: "Ready", cls: "s-accent" },
  blocked: { label: "Blocked", cls: "s-needs" },
  merged: { label: "Merged", cls: "s-accent" },
  failed: { label: "Failed", cls: "s-failed" },
};
const PLAN_GROUPS = [
  { id: "running", label: "Running", has: ["running"] },
  { id: "review", label: "Review", has: ["review"] },
  { id: "next", label: "Up next", has: ["ready", "planned", "blocked"] },
  { id: "failed", label: "Failed", has: ["failed"] },
  { id: "merged", label: "Merged", has: ["merged"] },
];
const groupFor = (status) => PLAN_GROUPS.find((g) => g.has.includes(status))?.id || "next";
const BACKEND = { claude: "Claude", codex: "Codex", opencode: "opencode" };

/**
 * The plan a project carries, normalized: { nodes, awaitingApproval, planIds }
 * or null when it has none. Approval comes from the plan queue, which the
 * worker sends per plan (`plans: [{ id, awaitingApproval }]`), on the plan or
 * beside it on the project.
 */
export function planOf(p) {
  const plan = p?.plan;
  const nodes = Array.isArray(plan?.nodes) ? plan.nodes.filter((n) => n && n.id) : [];
  const plans = ([plan?.plans, p?.plans].find(Array.isArray) || []).filter(Boolean);
  const waiting = plans.filter((x) => x.awaitingApproval === true);
  const awaitingApproval = waiting.length > 0 || plan?.awaitingApproval === true;
  if (!nodes.length && !awaitingApproval) return null;
  // `request` is the goal the plan was made for: the panel's title.
  const requests = plans.map((x) => (typeof x.request === "string" ? x.request.trim() : "")).filter(Boolean);
  const title = (waiting.find((x) => x.request)?.request || (requests.length === 1 ? requests[0] : "")).trim();
  return { nodes, awaitingApproval, waitingIds: waiting.map((x) => x.id).filter(Boolean), title, planCount: plans.length, updatedAt: plan?.updatedAt || null };
}

function depsText(node) {
  const deps = Array.isArray(node.deps) ? node.deps.filter((d) => d && d.on) : [];
  const of = (type) => deps.filter((d) => (d.type || "hard") === type).map((d) => d.on);
  const bits = [];
  const hard = of("hard"), soft = of("soft"), judge = of("judge");
  if (hard.length) bits.push(`after ${hard.join(", ")}`);
  if (judge.length) bits.push(`judges ${judge.join(", ")}`);
  if (soft.length) bits.push(`uses ${soft.join(", ")}`);
  return bits.join(" · ");
}

/**
 * PlanView({ ws, hrefFor(runId), variant: "panel" | "card" }) -> { el, update(project) }.
 * "panel" is the desktop column beside the agent; "card" sits on the phone's
 * project screen, collapsible.
 */
export function PlanView({ ws, hrefFor, variant = "panel" }) {
  const el = h(`<section class="plan-view ${variant}" aria-label="Plan">
    <div class="pv-head">
      <button type="button" class="pv-toggle" aria-expanded="true">
        <span class="pv-title">Plan</span><span class="pv-count"></span>
        ${variant === "card" ? icon("chevron").replace("<svg", '<svg class="pv-caret"') : ""}
      </button>
    </div>
    <div class="pv-bar" aria-hidden="true"><span></span></div>
    <div class="pv-approve" hidden>
      <div class="pv-ap-text"><b>Waiting for your approval</b><span></span></div>
      <button type="button" class="btn small accent pv-ap-btn">${icon("check")}<span>Approve</span></button>
    </div>
    <div class="pv-groups"></div>
  </section>`);
  const toggle = el.querySelector(".pv-toggle");
  const approve = el.querySelector(".pv-approve");
  const approveBtn = el.querySelector(".pv-ap-btn");
  const groupsEl = el.querySelector(".pv-groups");
  const openKey = `rudder.planOpen.${ws}`;
  let open = variant === "panel" || (() => { try { return localStorage.getItem(openKey) === "1"; } catch { return false; } })();
  let project = null;
  let tracked = false;
  function setOpen(v, save = false) {
    open = v;
    toggle.setAttribute("aria-expanded", String(open));
    el.classList.toggle("collapsed", !open);
    groupsEl.hidden = !open;
    if (save) try { localStorage.setItem(openKey, open ? "1" : "0"); } catch { /* private mode */ }
  }
  if (variant === "card") toggle.addEventListener("click", () => setOpen(!open, true));
  else toggle.disabled = true;
  setOpen(open);

  approveBtn.addEventListener("click", async () => {
    const plan = planOf(project);
    if (!plan?.awaitingApproval) return;
    approveBtn.disabled = true;
    try {
      // The dashboard's own approval (the empty Enter in its task bar). An
      // empty plan id approves the one waiting plan.
      const res = await api("/api/app/action", { method: "POST", body: { workspace: ws, action: "approve", plan: plan.waitingIds[0] || "" } });
      toast(res?.status === "queued" ? "Approved. The plan launches when the dashboard picks it up." : "Approved. The plan is launching.");
      app.refreshState?.();
    } catch (err) { showError(err); }
    finally { approveBtn.disabled = false; }
  });

  function update(p) {
    project = p;
    const plan = planOf(p);
    el.hidden = !plan;
    if (!plan) return;
    if (!tracked) { tracked = true; track("plan_panel_opened", { variant, nodes: plan.nodes.length }); }
    const merged = plan.nodes.filter((n) => n.status === "merged").length;
    const title = el.querySelector(".pv-title");
    setText(title, plan.title || "Plan");
    title.title = plan.title || "";
    setText(el.querySelector(".pv-count"), plan.nodes.length ? `${merged} of ${plan.nodes.length} merged` : "");
    el.querySelector(".pv-bar span").style.width = plan.nodes.length ? `${Math.round((merged / plan.nodes.length) * 100)}%` : "0";
    el.querySelector(".pv-bar").hidden = !plan.nodes.length;

    approve.hidden = !plan.awaitingApproval;
    if (plan.awaitingApproval) {
      setText(approve.querySelector(".pv-ap-text span"), "Nothing launches until you approve. To change it first, message the orchestrator.");
    }

    const groups = PLAN_GROUPS.map((g) => ({
      ...g,
      nodes: plan.nodes.filter((n) => groupFor(n.status) === g.id),
    })).filter((g) => g.nodes.length);
    reconcile(groupsEl, groups, (g) => g.id,
      () => h(`<div class="pv-group"><div class="pv-gh"><span class="pv-gl"></span><span class="pv-gc"></span></div><div class="pv-nodes" role="list"></div></div>`),
      (gEl, g) => {
        setText(gEl.querySelector(".pv-gl"), g.label);
        setText(gEl.querySelector(".pv-gc"), String(g.nodes.length));
        reconcile(gEl.querySelector(".pv-nodes"), g.nodes, (n) => n.id,
          () => h(`<a class="pv-node" role="listitem"><span class="dot"></span>
            <span class="pv-nb"><span class="pv-nt"><span class="pv-id"></span><span class="pv-name"></span></span>
            <span class="pv-ns"><span class="pv-st"></span><span class="pv-deps"></span><span class="pv-model"></span></span></span>
            ${icon("chevron").replace("<svg", '<svg class="pv-go"')}</a>`),
          (a, n) => {
            const st = NODE_STATUS[n.status] || NODE_STATUS.planned;
            a.className = `pv-node ${st.cls}${n.runId ? "" : " no-run"}`;
            a.querySelector(".dot").classList.toggle("live", !!st.live);
            if (n.runId) { a.href = hrefFor(n.runId); a.setAttribute("data-link", ""); a.removeAttribute("aria-disabled"); }
            else { a.removeAttribute("href"); a.removeAttribute("data-link"); a.setAttribute("aria-disabled", "true"); }
            setText(a.querySelector(".pv-id"), n.id);
            setText(a.querySelector(".pv-name"), n.title || "Untitled task");
            setText(a.querySelector(".pv-st"), st.label);
            const deps = depsText(n);
            const d = a.querySelector(".pv-deps");
            setText(d, deps); d.hidden = !deps;
            const model = [BACKEND[n.backend] || n.backend, n.model].filter(Boolean).join(" · ");
            const m = a.querySelector(".pv-model");
            setText(m, model); m.hidden = !model;
            a.setAttribute("aria-label", `${n.id} ${n.title || ""}, ${st.label}${deps ? `, ${deps}` : ""}${n.runId ? "" : ", not started"}`);
          });
      });
  }
  return { el, update };
}
