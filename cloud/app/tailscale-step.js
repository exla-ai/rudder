// Tailscale on the cloud machine, as one step. Settings and the Get set up
// card show the same row; Connect opens a sheet that runs the sign-in.
//
// The machine runs `tailscale up` and answers with Tailscale's sign-in link
// (cloud/worker/tailscale-login.mjs, POST /api/app/tailscale/connect). The
// sheet opens that link, then follows home.tailnet in /api/app/state until the
// machine is on the tailnet. Connect again while a sign-in waits hands back
// the same link. Disconnect is `tailscale logout` there.
import { h, icon, setText } from "/app/render.js";
import { app, api, confirmDialog, showError, toast } from "/app/core.js";
import { showAgentMenu } from "/app/agent-menu.js";

// A grid of dots, the shape people know Tailscale by.
const TS_ICON = `<svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
  <circle cx="5" cy="5" r="2.3" opacity=".35"/><circle cx="12" cy="5" r="2.3" opacity=".35"/><circle cx="19" cy="5" r="2.3" opacity=".35"/>
  <circle cx="5" cy="12" r="2.3"/><circle cx="12" cy="12" r="2.3"/><circle cx="19" cy="12" r="2.3"/>
  <circle cx="5" cy="19" r="2.3" opacity=".35"/><circle cx="12" cy="19" r="2.3"/><circle cx="19" cy="19" r="2.3" opacity=".35"/>
</svg>`;
// Where a tailnet admin approves a new device.
const ADMIN_URL = "https://login.tailscale.com/admin/machines";
export const TAILSCALE_GIVES = "Private links to previews and servers on your cloud machine, from any of your devices";
// The Get set up row's one line, as short as the agent rows' "Agents run on your ... subscription".
const SETUP_SUB = "Private links to your previews from any device";

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// The machine's name on the tailnet: rudder-home of rudder-home.tail1234.ts.net.
function deviceName(t) {
  return t.dnsName ? t.dnsName.split(".")[0] : t.hostname;
}
function connectedAs(t) {
  return `${deviceName(t)}${t.tailnetName ? ` on ${t.tailnetName}` : ""}`;
}

/** Where Tailscale is on the machine, from home.tailnet. */
export function tailscaleState(home) {
  const t = home?.tailnet;
  const login = t?.login;
  if (t?.online) return { kind: "connected", pill: "Connected", cls: "s-done", sub: connectedAs(t) };
  if (t?.backendState === "NeedsMachineAuth") {
    return { kind: "approval", pill: "Needs approval", cls: "s-needs", sub: "An admin of your tailnet needs to approve this machine" };
  }
  if (login?.status === "starting" || login?.status === "waiting") {
    return { kind: "login", pill: "Connecting", cls: "s-accent", live: true,
      sub: login.status === "starting" ? "Starting Tailscale on your cloud machine" : "Waiting for you to sign in to Tailscale" };
  }
  if (t?.backendState === "Starting" || t?.backendState === "Running") {
    return { kind: "connecting", pill: "Connecting", cls: "s-accent", live: true, sub: "Joining your tailnet" };
  }
  if (login?.status === "failed") return { kind: "error", sub: login.error, err: true };
  if (t?.error) return { kind: "error", sub: t.error, err: true };
  return { kind: "off", sub: "Not connected" };
}

/**
 * One row (.acct, like the agent sign-ins). In the Get set up card (`setup`)
 * it says what Tailscale gives you and ticks off once connected; in Settings
 * it has a pill and, when connected, a More menu with Disconnect.
 */
export function TailscaleRow({ setup = false } = {}) {
  const row = h(`<div class="acct ts-row" role="listitem">
    <div class="m-icon ts-icon">${TS_ICON}</div>
    <div class="a-body"><div class="a-title"><span></span><span class="pill" hidden><span class="dot"></span><span class="pl"></span></span></div>
    <div class="a-sub"></div></div>
    <span class="setup-check" hidden>${icon("check")}</span>
    <button type="button" class="btn small accent a-btn">Connect</button>
    <button type="button" class="icon-btn a-more" aria-label="Tailscale options" hidden>${icon("more")}</button>
  </div>`);
  setText(row.querySelector(".a-title span"), setup ? "Connect Tailscale" : "Tailscale");
  const btn = row.querySelector(".a-btn");
  const more = row.querySelector(".a-more");
  let kind = "off";
  btn.addEventListener("click", () => {
    if (kind === "approval") app.openSignInPage(ADMIN_URL);
    else openTailscaleConnect();
  });
  more.addEventListener("click", () => showAgentMenu({
    anchor: more,
    label: "Tailscale",
    returnFocus: more,
    items: [{ label: "Disconnect", icon: "power", danger: true, run: () => void disconnectTailscale() }],
  }));

  function update(home) {
    const st = tailscaleState(home);
    kind = st.kind;
    const pill = row.querySelector(".pill");
    pill.hidden = setup || !st.pill;
    if (!pill.hidden) {
      pill.className = `pill ${st.cls}`;
      pill.querySelector(".dot").classList.toggle("live", !!st.live);
      setText(pill.querySelector(".pl"), st.pill);
    }
    const sub = row.querySelector(".a-sub");
    const text = setup && st.kind === "off" ? SETUP_SUB
      : setup && st.kind === "connected" ? `Connected as ${st.sub}` : st.sub;
    setText(sub, text);
    sub.classList.toggle("err", !!st.err);
    row.classList.toggle("done", setup && st.kind === "connected");
    row.querySelector(".setup-check").hidden = !(setup && st.kind === "connected");
    btn.hidden = st.kind === "connected" || st.kind === "connecting";
    setText(btn, { off: "Connect", login: "Continue", approval: "Approve", error: "Try again" }[st.kind] || "Connect");
    more.hidden = setup || st.kind !== "connected";
  }
  return { el: row, update };
}

/** Settings: the row in its own card, what it gives you under it, and the machine's address once connected. */
export function TailscaleCard() {
  const row = TailscaleRow();
  const el = h(`<div class="ts-settings"><div class="card accounts ts-card" role="list" aria-label="Tailscale"></div><p class="hint"></p></div>`);
  el.firstElementChild.appendChild(row.el);
  const hint = el.querySelector(".hint");
  function update(home) {
    row.update(home);
    const t = home?.tailnet;
    hint.replaceChildren(`${TAILSCALE_GIVES}.`);
    if (t?.online && t.dnsName) {
      const addr = h(`<span class="ts-addr"></span>`);
      setText(addr, t.dnsName);
      hint.append(" Its address on your tailnet is ", addr, ".");
    }
  }
  return { el, update };
}

async function disconnectTailscale() {
  const ok = await confirmDialog({
    title: "Disconnect Tailscale?",
    body: "Your cloud machine leaves your tailnet, and its private links stop working. You can connect it again any time.",
    confirm: "Disconnect",
    danger: true,
  });
  if (!ok) return;
  try {
    await api("/api/app/tailscale/disconnect", { method: "POST", body: {} });
    toast("Tailscale is disconnected");
  } catch (err) {
    showError(err);
  }
  await app.refreshState();
}

/** Connect, start to finish, in a sheet. */
export function openTailscaleConnect() {
  const dlg = h(`<dialog class="sheet login-sheet ts-sheet" aria-label="Connect Tailscale"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3>Connect Tailscale</h3><button type="button" class="icon-btn" data-act="close" aria-label="Close">${icon("x")}</button></div>
    <div class="ls-body" aria-live="polite"></div>
  </div></dialog>`);
  const body = dlg.querySelector(".ls-body");
  let closed = false;
  let session = null;
  let showing = "";

  const busy = (title, detail = "") => {
    showing = "busy";
    body.replaceChildren(h(`<div class="ls-wait"><span class="spin" aria-hidden="true"></span><div><div class="ls-title"></div><div class="ls-note"></div></div></div>`));
    setText(body.querySelector(".ls-title"), title);
    setText(body.querySelector(".ls-note"), detail);
  };
  const failed = (message) => {
    showing = "failed";
    body.replaceChildren(h(`<div class="ls-msg err"></div>`));
    setText(body.firstChild, message);
    const again = h(`<div class="ls-actions"><button type="button" class="btn block accent">Try again</button></div>`);
    again.querySelector("button").addEventListener("click", () => void begin());
    body.appendChild(again);
  };
  const succeeded = (t) => {
    showing = "done";
    body.replaceChildren(h(`<div class="ls-done"><div class="ls-check">${icon("check")}</div><div class="ls-title">Tailscale is connected</div><div class="ls-note"></div></div>`));
    setText(body.querySelector(".ls-note"), `Your cloud machine is on your tailnet as ${connectedAs(t)}.`);
    toast("Tailscale is connected");
    setTimeout(() => { if (!closed) dlg.close(); }, 1600);
  };

  function showLink() {
    if (showing === "link") return;
    showing = "link";
    body.replaceChildren(h(`<div class="ls-steps">
      <div class="ls-step"><span class="ls-n">1</span><div>Open Tailscale and sign in with the account your other devices use.</div></div>
      <button type="button" class="btn block accent ls-open">${icon("external")}<span>Open the Tailscale sign-in page</span></button>
      <div class="ls-step"><span class="ls-n">2</span><div>Approve adding your cloud machine to your tailnet.</div></div>
      <div class="ls-wait small"><span class="spin" aria-hidden="true"></span><div class="ls-note">Waiting for you to sign in. This closes by itself.</div></div>
    </div>`));
    body.querySelector(".ls-open").addEventListener("click", () => app.openSignInPage(session.url));
  }

  function showApproval(t) {
    if (showing === "approval") return;
    showing = "approval";
    body.replaceChildren(h(`<div class="ls-steps">
      <div class="ls-title">Your tailnet approves new devices</div>
      <div class="ls-step ts-approve"></div>
      <button type="button" class="btn block accent ls-open">${icon("external")}<span>Open the Tailscale admin console</span></button>
      <div class="ls-wait small"><span class="spin" aria-hidden="true"></span><div class="ls-note">Waiting for approval. This closes by itself.</div></div>
    </div>`));
    setText(body.querySelector(".ts-approve"), `An admin of your tailnet approves ${deviceName(t)} under Machines in the admin console. If that's not you, ask them to.`);
    body.querySelector(".ls-open").addEventListener("click", () => app.openSignInPage(ADMIN_URL));
  }

  // The machine may be asleep, or not exist yet for a new account: starting
  // wakes or creates it, which can outlast one request.
  async function start() {
    const deadline = Date.now() + 5 * 60_000;
    for (;;) {
      const home = app.store.state?.home;
      busy(
        "Starting Tailscale on your cloud machine",
        !home ? "Creating your cloud machine first. The first start takes a minute or two."
          : !home.connected ? "Waking your cloud machine first. This can take a minute." : "",
      );
      try {
        return await api("/api/app/tailscale/connect", { method: "POST", body: {} });
      } catch (err) {
        if (closed || err.status === 401) throw err;
        if (err.status !== 503 || Date.now() > deadline) throw err;
        await app.refreshState();
        await sleep(3000);
        if (closed) throw new Error("closed");
      }
    }
  }

  async function begin() {
    try {
      session = await start();
    } catch (err) {
      if (!closed && err.status !== 401) failed(err.message);
      return;
    }
    if (closed) return;
    await app.refreshState();
    // The machine finishes on its own; follow its state.
    const id = session.id;
    let seen = false;
    while (!closed && session?.id === id) {
      const home = app.store.state?.home;
      const t = home?.tailnet;
      const login = t?.login?.id === id ? t.login : null;
      seen ||= !!login;
      if (t?.online) return void succeeded(t);
      if (login?.status === "failed") return void failed(login.error);
      if (login?.status === "cancelled" || (seen && !login && home?.stateLive)) {
        return void failed("The sign-in stopped on your cloud machine before it finished. Connect again for a new link.");
      }
      if (t?.backendState === "NeedsMachineAuth" || login?.status === "approval" || session.status === "approval") showApproval(t);
      else if (login?.status === "done" || session.status === "done") {
        if (showing !== "joining") { busy("Joining your tailnet"); showing = "joining"; }
      }
      else if (session.url) showLink();
      await sleep(2000);
      if (closed || session?.id !== id) return;
      await app.refreshState();
    }
  }

  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest("[data-act=close]")) dlg.close();
  });
  // Closing leaves the sign-in running on the machine: tailscaled takes the
  // link whether or not this sheet is open, and Connect brings it back.
  dlg.addEventListener("close", () => {
    closed = true;
    dlg.remove();
    void app.refreshState();
  });
  document.body.appendChild(dlg);
  dlg.showModal();
  void begin();
}
