// Shared plumbing for every screen: the native bridge, storage, the API
// client, toasts, dialogs, sheets and product events. app.js fills `app` with
// the router and store so the feature modules can reach them without a cycle.
import { h, icon } from "/app/render.js";

/** Filled in by app.js at boot: navigate, refreshState, store, updateScreen, ... */
export const app = {};

/* Native shell bridge ------------------------------------------------ */

export const NATIVE = window.RUDDER_NATIVE || null;
/** "ios" | "mac" | null: which native shell, if any, hosts this page. */
export const PLATFORM = NATIVE?.platform || null;
export function postNative(msg) {
  try { window.webkit?.messageHandlers?.rudder?.postMessage(msg); } catch { /* not in the shell */ }
}
export function openExternal(url) {
  if (!url) return;
  if (NATIVE) postNative({ type: "openExternal", url });
  else window.open(url, "_blank", "noopener,noreferrer");
}

/* Small utilities ---------------------------------------------------- */

export const ls = {
  get(k, d = null) { try { const v = localStorage.getItem(k); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch { /* private mode */ } },
};
export const coarse = matchMedia("(pointer: coarse)").matches;
export const toMs = (t) => (t == null ? null : typeof t === "number" ? t : Date.parse(t) || null);

export function rel(t) {
  const ms = toMs(t);
  if (!ms) return "";
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 45) return "now";
  if (s < 3600) return `${Math.round(s / 60)}m`;
  if (s < 86400) return `${Math.round(s / 3600)}h`;
  if (s < 2 * 86400) return "Yesterday";
  if (s < 7 * 86400) return `${Math.round(s / 86400)}d`;
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export const TIME_ZONE = (() => { try { return Intl.DateTimeFormat().resolvedOptions().timeZone || ""; } catch { return ""; } })();

/* API ---------------------------------------------------------------- */

export class ApiError extends Error { constructor(msg, status, waking = false) { super(msg); this.status = status; this.waking = waking; } }

function authHeaders() {
  const headers = { Accept: "application/json" };
  const token = NATIVE?.token || ls.get("rudder.token");
  if (token) headers.Authorization = `Bearer ${token}`;
  // The morning digest arrives at 9:01 on this phone's clock.
  if (TIME_ZONE) headers["X-Rudder-Timezone"] = TIME_ZONE;
  // Which client this is (web, mac, ios), for the server's product events.
  headers["X-Rudder-Platform"] = PLATFORM || "browser";
  return headers;
}

export async function api(path, { method = "GET", body, raw } = {}) {
  const headers = authHeaders();
  if (raw) headers["Content-Type"] = raw.type || "application/octet-stream";
  else if (body !== undefined) headers["Content-Type"] = "application/json";
  let res;
  try {
    res = await fetch(path, {
      method, headers, credentials: "same-origin", cache: "no-store",
      body: raw || (body !== undefined ? JSON.stringify(body) : undefined),
    });
  } catch {
    throw new ApiError("Can't reach Rudder. Check your connection.", 0);
  }
  if (res.status === 401) { app.onSignedOut?.(); throw new ApiError("Signed out", 401); }
  let data = null;
  try { data = await res.json(); } catch { /* empty body */ }
  if (!res.ok) throw new ApiError(data?.error || `${res.status} ${res.statusText || "Request failed"}`.trim(), res.status, !!data?.waking);
  return data;
}

/** Toast an API error unless it is the sign-out every screen already handles. */
export function showError(err) {
  if (err?.status !== 401) toast(err?.message || "Something went wrong", { error: true });
}

/**
 * Product events (onboarding, orgs, integrations...). Fire and forget: a
 * failed or refused event never reaches the person using the app.
 */
export function track(name, props) {
  try {
    const headers = authHeaders();
    headers["Content-Type"] = "application/json";
    fetch("/api/app/event", {
      method: "POST", headers, credentials: "same-origin", cache: "no-store", keepalive: true,
      body: JSON.stringify({ name, props: { platform: PLATFORM || "browser", ...(props || {}) } }),
    }).catch(() => {});
  } catch { /* never throws */ }
}

/* Toasts + dialogs --------------------------------------------------- */

let lastToast = { text: "", at: 0 };
export function toast(text, { error = false } = {}) {
  const box = document.getElementById("toasts");
  if (!box) return;
  if (text === lastToast.text && Date.now() - lastToast.at < 2500) return;
  lastToast = { text, at: Date.now() };
  const el = h(`<div class="toast${error ? " error" : ""}"></div>`);
  el.textContent = text;
  box.appendChild(el);
  setTimeout(() => { el.classList.add("out"); setTimeout(() => el.remove(), 220); }, error ? 4200 : 2800);
}

export function confirmDialog({ title, body, confirm = "Confirm", danger = false }) {
  return new Promise((resolve) => {
    const dlg = h(`<dialog aria-labelledby="dlg-t"><div class="dialog">
      <h3 id="dlg-t"></h3><p></p>
      <div class="d-actions"><button type="button" class="btn" data-v="0">Cancel</button>
      <button type="button" class="btn ${danger ? "confirm-danger" : "primary"}" data-v="1"></button></div></div></dialog>`);
    dlg.querySelector("h3").textContent = title;
    dlg.querySelector("p").textContent = body;
    dlg.querySelector('[data-v="1"]').textContent = confirm;
    let result = false;
    dlg.addEventListener("click", (e) => {
      const b = e.target.closest("[data-v]");
      if (b) { result = b.dataset.v === "1"; dlg.close(); }
      else if (e.target === dlg) dlg.close();
    });
    dlg.addEventListener("close", () => { dlg.remove(); resolve(result); });
    document.body.appendChild(dlg);
    dlg.showModal();
  });
}

/**
 * A bottom sheet (a centered dialog on desktop) with a title and a close
 * button. Returns { dlg, body, close }; the caller fills `body`.
 */
export function openSheet({ title, className = "" }) {
  const dlg = h(`<dialog class="sheet add-sheet ${className}"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3></h3><button type="button" class="icon-btn" data-act="close" aria-label="Close">${icon("x")}</button></div>
    <div class="sheet-body"></div>
  </div></dialog>`);
  dlg.querySelector("h3").textContent = title;
  dlg.setAttribute("aria-label", title);
  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest('[data-act="close"]')) dlg.close();
  });
  dlg.addEventListener("close", () => dlg.remove());
  document.body.appendChild(dlg);
  dlg.showModal();
  return { dlg, body: dlg.querySelector(".sheet-body"), close: () => dlg.close() };
}
