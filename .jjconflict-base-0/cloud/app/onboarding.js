// First run after sign-in: "How do you want to use Rudder?" Four surfaces,
// any of them; then what to do next for each one picked. Saved to the
// account (POST /api/app/onboarding) so every device skips it afterwards.
// Also reachable from Settings.
import { h, icon, setText, copyText } from "/app/render.js";
import { app, api, ls, PLATFORM, openExternal, showError, toast, track } from "/app/core.js";

export const DOWNLOAD_MAC_URL = "https://rudder.exla.ai/download/mac";
export const PHONE_APP_URL = "https://rudder.exla.ai/download/ios";
export const INSTALL_LINE = "npm i -g @exla/rudder && rudder login";

const MAC_ICON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="4" width="18" height="12" rx="2"/><path d="M8 20h8M12 16v4"/></svg>';
const PHONE_ICON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="7" y="2.5" width="10" height="19" rx="2.5"/><path d="M11 18.5h2"/></svg>';

export const SURFACES = [
  { id: "web", title: "Web app", body: "Right here, in any browser.", svg: icon("globe") },
  { id: "mac", title: "Mac app", body: "A native window for your agents.", svg: MAC_ICON },
  { id: "terminal", title: "Terminal", body: "The full dashboard in your terminal.", svg: icon("terminal") },
  { id: "phone", title: "Phone", body: "Steer agents and answer them on the go.", svg: PHONE_ICON },
];
const DONE_KEY = "rudder.onboarded";

/** The account has not picked yet (and this server knows about onboarding). */
export function onboardingDue(state) {
  if (!state?.onboarding || typeof state.onboarding !== "object") return false;
  if (state.onboarding.completedAt) return false;
  // Saving failed earlier on this device: do not trap the person in the picker.
  return !ls.get(DONE_KEY);
}

function defaultSurfaces(saved) {
  if (Array.isArray(saved) && saved.length) return saved.filter((s) => SURFACES.some((x) => x.id === s));
  if (PLATFORM === "ios") return ["phone"];
  if (PLATFORM === "mac") return ["mac"];
  return ["web"];
}

/**
 * OnboardingScreen({ again }): `again` when opened from Settings (a back
 * button, no skip).
 */
export function OnboardingScreen({ again = false } = {}) {
  const el = h(`<section class="screen onboarding">
    <div class="scroller"><div class="ob-wrap"></div></div></section>`);
  const wrap = el.querySelector(".ob-wrap");
  const saved = app.store?.state?.onboarding?.surfaces;
  const picked = new Set(defaultSurfaces(saved));
  let step = "pick";
  track("onboarding_viewed", { again, platform: PLATFORM || "browser" });

  async function save(complete) {
    const surfaces = SURFACES.map((s) => s.id).filter((id) => picked.has(id));
    try {
      const res = await api("/api/app/onboarding", { method: "POST", body: complete ? { surfaces, complete: true } : { surfaces } });
      if (app.store?.state) app.store.state.onboarding = res || { surfaces, completedAt: complete ? new Date().toISOString() : null };
      return true;
    } catch (err) {
      if (err.status === 401) return false;
      // An older server has no onboarding route: finish locally rather than loop.
      if (err.status !== 404) showError(err);
      return err.status === 404;
    } finally {
      if (complete) ls.set(DONE_KEY, true);
    }
  }

  async function finish() {
    await save(true); // the server records onboarding_completed itself
    if (again) app.goBack?.("/app/settings");
    else { app.navigate?.("/app", { replace: true }); app.updateScreen?.(); }
  }

  function renderPick() {
    wrap.replaceChildren(h(`<div class="ob-pick">
      <img class="ob-logo" src="/app/icon.svg" alt="">
      <h1>How do you want to use Rudder?</h1>
      <p class="ob-lead">Pick any. You can use all of them with one account.</p>
      <div class="ob-cards" role="group" aria-label="Ways to use Rudder"></div>
      <p class="ob-sub">Agents run on your own Claude Code / Codex subscription.</p>
      <div class="ob-actions"><button type="button" class="btn accent block ob-next">Continue</button>
      ${again ? "" : `<button type="button" class="ob-skip">Skip for now</button>`}</div>
    </div>`));
    if (again) wrap.prepend(backBtn());
    const cards = wrap.querySelector(".ob-cards");
    const next = wrap.querySelector(".ob-next");
    const sync = () => {
      next.disabled = picked.size === 0;
      setText(next, picked.size ? "Continue" : "Pick at least one");
    };
    for (const s of SURFACES) {
      const b = h(`<button type="button" class="ob-card" aria-pressed="false">
        <span class="ob-ic">${s.svg}</span><span class="ob-t"><b></b><span></span></span>
        <span class="ob-check">${icon("check")}</span></button>`);
      setText(b.querySelector("b"), s.title);
      setText(b.querySelector(".ob-t span"), s.body);
      const paint = () => { const on = picked.has(s.id); b.setAttribute("aria-pressed", String(on)); b.classList.toggle("on", on); };
      b.addEventListener("click", () => {
        if (picked.has(s.id)) picked.delete(s.id); else picked.add(s.id);
        paint(); sync();
        track("onboarding_surface_toggled", { surface: s.id, on: picked.has(s.id) });
      });
      paint();
      cards.appendChild(b);
    }
    next.addEventListener("click", async () => {
      if (!picked.size) return;
      next.disabled = true;
      await save(false);
      step = "next"; render();
    });
    wrap.querySelector(".ob-skip")?.addEventListener("click", finish);
    sync();
  }

  function backBtn() {
    const b = h(`<button type="button" class="icon-btn ob-back" aria-label="Back">${icon("back")}</button>`);
    b.addEventListener("click", () => {
      if (step === "next") { step = "pick"; render(); } else app.goBack?.("/app/settings");
    });
    return b;
  }

  function stepCard(s) {
    const card = h(`<div class="card ob-step"><div class="ob-sh"><span class="ob-ic">${s.svg}</span><b></b></div><div class="ob-sb"></div></div>`);
    setText(card.querySelector("b"), s.title);
    const body = card.querySelector(".ob-sb");
    if (s.id === "web") {
      body.append(h(`<p>You're in. Bookmark this page, or install it from your browser's menu.</p>`));
    } else if (s.id === "terminal") {
      body.append(h(`<p>Install it, then sign in. <code>rudder login</code> shows a code you approve here.</p>`));
      const cmd = h(`<button type="button" class="c-cmd ob-cmd" aria-label="Copy the install command"><code></code>${icon("copy")}</button>`);
      setText(cmd.querySelector("code"), INSTALL_LINE);
      cmd.addEventListener("click", async () => {
        if (await copyText(INSTALL_LINE)) { toast("Copied"); track("install_cli_copied", { from: "onboarding" }); }
      });
      body.append(cmd);
    } else if (s.id === "mac") {
      body.append(h(`<p>The Mac app shows these same projects and agents, and can pull an agent onto your Mac.</p>`));
      const b = h(`<a class="btn accent ob-dl" target="_blank" rel="noopener noreferrer">${icon("down")}<span>Download for Mac</span></a>`);
      b.href = DOWNLOAD_MAC_URL;
      b.addEventListener("click", (e) => {
        track("download_mac_clicked", { from: "onboarding" });
        if (PLATFORM) { e.preventDefault(); openExternal(DOWNLOAD_MAC_URL); }
      });
      body.append(b);
    } else if (s.id === "phone") {
      if (PLATFORM === "ios") {
        body.append(h(`<p>You're in. This is the phone app.</p>`));
      } else {
        body.append(h(`<p>Get the iPhone app, or open this page on your phone and add it to your home screen.</p>`));
        const row = h(`<div class="ob-row"></div>`);
        const app1 = h(`<a class="btn accent" target="_blank" rel="noopener noreferrer">${PHONE_ICON}<span>Get the iPhone app</span></a>`);
        app1.href = PHONE_APP_URL;
        app1.addEventListener("click", (e) => {
          track("phone_link_opened", { kind: "app" });
          if (PLATFORM) { e.preventDefault(); openExternal(PHONE_APP_URL); }
        });
        const webUrl = `${location.origin}/app`;
        const copy = h(`<button type="button" class="btn">${icon("copy")}<span>Copy web link</span></button>`);
        copy.addEventListener("click", async () => {
          if (await copyText(webUrl)) { toast("Link copied"); track("phone_link_opened", { kind: "web" }); }
        });
        row.append(app1, copy);
        body.append(row);
      }
    }
    return card;
  }

  function renderNext() {
    const chosen = SURFACES.filter((s) => picked.has(s.id));
    wrap.replaceChildren(backBtn(), h(`<div class="ob-pick">
      <h1>You're almost set</h1>
      <p class="ob-lead">Here's how to get going with what you picked.</p>
      <div class="ob-steps"></div>
      <div class="ob-actions"><button type="button" class="btn accent block ob-done"></button></div>
    </div>`));
    const steps = wrap.querySelector(".ob-steps");
    for (const s of chosen) steps.appendChild(stepCard(s));
    const done = wrap.querySelector(".ob-done");
    setText(done, again ? "Done" : "Continue to setup");
    done.addEventListener("click", async () => { done.disabled = true; await finish(); });
  }

  function render() {
    if (step === "pick") renderPick(); else renderNext();
    el.querySelector(".scroller").scrollTop = 0;
  }
  render();
  return { el, isOnboarding: true };
}
