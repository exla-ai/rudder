// Machine & Cache dashboard screen (/app/machine), project WarmChip badge,
// and workspace mode formatter for the agent-stats popover.
// Plain ES module, no build step.
import { h, icon, setText, reconcile } from "/app/render.js";
import { app, api, rel, toMs, toast, showError } from "/app/core.js";

const POLL_MS = 5000;
const ACTIVE_WINDOW_MS = 10 * 60_000;
const FEED_LIMIT = 20;

/* Formatting helpers -------------------------------------------------- */

function num(v) {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function formatBytes(bytes) {
  const n = num(bytes);
  if (n == null || n < 0) return "--";
  if (n === 0) return "0 B";
  if (n < 1024) return `${Math.round(n)} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let val = n / 1024;
  let u = 0;
  while (val >= 1024 && u < units.length - 1) {
    val /= 1024;
    u++;
  }
  const decimals = val < 10 ? 1 : val < 100 && u >= 1 ? 1 : 0;
  return `${val.toFixed(decimals)} ${units[u]}`;
}

function formatCount(n) {
  const v = num(n);
  if (v == null) return "0";
  return Math.round(v).toLocaleString();
}

function formatDurationMs(ms) {
  const v = num(ms);
  if (v == null || v < 0) return "--";
  if (v < 1000) return `${Math.round(v)} ms`;
  if (v < 60_000) return `${(v / 1000).toFixed(v < 10_000 ? 1 : 0)} s`;
  const m = Math.floor(v / 60_000);
  const s = Math.round((v % 60_000) / 1000);
  return `${m}m ${s}s`;
}

function formatSeconds(sec) {
  const v = num(sec);
  if (v == null || v < 0) return "";
  if (v < 60) return `${v.toFixed(1)} s`;
  const m = Math.floor(v / 60);
  const s = Math.round(v % 60);
  return `${m}m ${s}s`;
}

function formatMemoryMb(mb) {
  const v = num(mb);
  if (v == null || v <= 0) return "";
  if (v >= 1024) {
    const gb = v / 1024;
    return `${Number.isInteger(gb) ? gb : gb.toFixed(1)} GB`;
  }
  return `${Math.round(v)} MB`;
}

function ago(t) {
  const ms = toMs(t);
  if (!ms) return "";
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 5) return "just now";
  if (s < 60) return `${Math.floor(s)}s ago`;
  const r = rel(ms);
  return r === "now" ? "just now" : /^\d+[mhd]$/.test(r) ? `${r} ago` : r;
}

function shortCommit(s) {
  if (typeof s !== "string") return "";
  const t = s.trim();
  return t ? t.slice(0, 8) : "";
}

function clampPct(part, total) {
  const p = num(part);
  const t = num(total);
  if (p == null || t == null || t <= 0) return 0;
  return Math.max(0, Math.min(100, (p / t) * 100));
}

function levelOf(pct) {
  if (pct >= 90) return "hot";
  if (pct >= 70) return "warm";
  return "ok";
}

function psiLevel(pct) {
  if (pct >= 25) return "hot";
  if (pct >= 10) return "warm";
  return "ok";
}

function vfsModeLabel(mode) {
  if (mode === "overlayfs") return "OverlayFS CoW";
  if (mode === "apfs-clone") return "APFS Clone";
  if (mode === "plain") return "Plain copy";
  return mode ? String(mode) : "Unknown";
}

function sccacheModeLabel(mode) {
  if (!mode) return "Local";
  const m = String(mode).toLowerCase();
  if (/local.*s3|multilevel|local.*webdav|l1.*l2/.test(m)) return "Local + S3";
  if (/s3|webdav|remote/.test(m)) return "S3";
  if (/local|disk/.test(m)) return "Local";
  return String(mode);
}

function computeHitRatePct(hits, misses, rawHitRate) {
  const hCount = num(hits) ?? 0;
  const mCount = num(misses) ?? 0;
  if (hCount + mCount > 0) return (hCount / (hCount + mCount)) * 100;
  const raw = num(rawHitRate);
  if (raw == null) return null;
  return raw > 1 ? Math.min(100, raw) : Math.max(0, raw * 100);
}

function hostKindLabel(kind) {
  if (kind === "fly") return "Fly";
  if (kind === "external") return "External";
  return kind ? String(kind) : "";
}

function pickRemoteToolStats(homeStats, cloudStats) {
  const hTotal = (num(homeStats?.hits) ?? 0) + (num(homeStats?.misses) ?? 0) + (num(homeStats?.writes) ?? 0);
  const cTotal = (num(cloudStats?.hits) ?? 0) + (num(cloudStats?.misses) ?? 0) + (num(cloudStats?.writes) ?? 0);
  if (homeStats && (!cloudStats || hTotal >= cTotal)) return homeStats;
  return cloudStats || homeStats || null;
}

function mergeRecentEvents(homeRecent, cloudRecent) {
  const combined = [];
  if (Array.isArray(homeRecent)) {
    for (const ev of homeRecent) {
      if (ev && typeof ev === "object") combined.push({ ...ev, _src: "home", _ms: toMs(ev.at) || 0 });
    }
  }
  if (Array.isArray(cloudRecent)) {
    for (const ev of cloudRecent) {
      if (ev && typeof ev === "object") combined.push({ ...ev, _src: "cloud", _ms: toMs(ev.at) || 0 });
    }
  }
  combined.sort((a, b) => {
    if (b._ms !== a._ms) return b._ms - a._ms;
    if (a._src === b._src) return 0;
    return a._src === "home" ? -1 : 1;
  });
  const out = [];
  for (const ev of combined) {
    const dup = out.some((kept) =>
      kept.tool === ev.tool &&
      kept.op === ev.op &&
      kept.key === ev.key &&
      (num(kept.bytes) ?? -1) === (num(ev.bytes) ?? -1) &&
      Math.abs(kept._ms - ev._ms) <= 2000 &&
      (kept._src !== ev._src || kept._ms === ev._ms)
    );
    if (!dup) {
      out.push(ev);
      if (out.length >= FEED_LIMIT) break;
    }
  }
  return out;
}

function normalizeOp(op) {
  const raw = String(op || "").trim().toLowerCase();
  if (raw === "hit" || raw === "get_hit") return { label: "HIT", cls: "mp-op-hit" };
  if (raw === "put" || raw === "write" || raw === "upload" || raw === "set") return { label: "PUT", cls: "mp-op-put" };
  if (raw === "miss" || raw === "get_miss") return { label: "MISS", cls: "mp-op-miss" };
  if (raw === "err" || raw === "error" || raw === "fail") return { label: "ERR", cls: "mp-op-err" };
  return { label: raw ? raw.toUpperCase().slice(0, 6) : "OP", cls: "mp-op-miss" };
}

function extractFreedBytes(res) {
  if (!res || typeof res !== "object") return 0;
  const direct = num(res.freedBytes)
    ?? num(res.bytesFreed)
    ?? num(res.reclaimedBytes)
    ?? num(res.totalFreedBytes)
    ?? num(res.freed_bytes)
    ?? num(res.result?.freedBytes);
  if (direct != null) return Math.max(0, direct);
  if (Array.isArray(res.entries)) {
    let sum = 0;
    for (const e of res.entries) sum += num(e?.freedBytes) ?? num(e?.bytesFreed) ?? 0;
    return Math.max(0, sum);
  }
  return 0;
}

/* Shared tiny DOM helpers --------------------------------------------- */

function makeSection(title) {
  const el = h(`<div class="section mp-sec">
    <div class="section-head"><h2></h2><span class="count mp-sec-meta"></span></div>
    <div class="mp-sec-body"></div>
  </div>`);
  setText(el.querySelector("h2"), title);
  return {
    el,
    body: el.querySelector(".mp-sec-body"),
    setMeta: (text) => setText(el.querySelector(".mp-sec-meta"), text || ""),
  };
}

function setPill(el, text, variant = "muted", { live = false } = {}) {
  el.className = `mp-pill mp-pill-${variant}`;
  let dot = el.querySelector(".mp-dot");
  let label = el.querySelector(".mp-pill-label");
  if (!label) {
    el.replaceChildren();
    dot = h(`<span class="mp-dot" aria-hidden="true"></span>`);
    label = h(`<span class="mp-pill-label"></span>`);
    el.append(dot, label);
  }
  dot.hidden = !live;
  dot.classList.toggle("mp-dot-live", !!live);
  setText(label, text);
}

function setMeter(meterEl, pct, forcedLevel = null) {
  const clamped = Math.max(0, Math.min(100, Number.isFinite(pct) ? pct : 0));
  const lv = forcedLevel || levelOf(clamped);
  meterEl.classList.remove("lv-ok", "lv-warm", "lv-hot");
  meterEl.classList.add(`lv-${lv}`);
  const fill = meterEl.querySelector(".mp-meter-fill");
  if (fill) fill.style.width = `${clamped.toFixed(1)}%`;
}

function setStatsGrid(container, items) {
  reconcile(
    container,
    items,
    (it) => it.k,
    () => h(`<div class="mp-stat"><span class="mp-stat-k"></span><span class="mp-stat-v"></span></div>`),
    (el, it) => {
      setText(el.querySelector(".mp-stat-k"), it.k);
      const vEl = el.querySelector(".mp-stat-v");
      setText(vEl, it.v);
      vEl.classList.toggle("mp-v-err", !!it.err);
      vEl.classList.toggle("mp-v-ok", !!it.ok);
      vEl.classList.toggle("mp-v-mono", !!it.mono);
    }
  );
}

/* Public Export 3: formatWorkspaceMode -------------------------------- */

/**
 * Format a workspace mode descriptor for the agent-stats popover.
 * - { kind: "overlay-fork", snapshotKey, layers } -> "Overlay fork · 3 layers (de4609ec)"
 * - { kind: "plain" } -> "Full checkout"
 * - anything else -> ""
 */
export function formatWorkspaceMode(mode) {
  if (!mode || typeof mode !== "object") return "";
  if (mode.kind === "plain") return "Full checkout";
  if (mode.kind === "overlay-fork") {
    let out = "Overlay fork";
    const layers = num(mode.layers);
    if (layers != null && layers > 0) {
      const n = Math.round(layers);
      out += ` · ${n} ${n === 1 ? "layer" : "layers"}`;
    }
    const key = shortCommit(mode.snapshotKey);
    if (key) out += ` (${key})`;
    return out;
  }
  return "";
}

/* Public Export 2: WarmChip ------------------------------------------- */

const WARM_CLASSES = ["wc-ready", "wc-stale", "wc-building", "wc-failed"];

/**
 * Compact warm-snapshot status chip for project headers.
 * Hidden when !warm or warm.status === "none". Clicking invokes onClick().
 */
export function WarmChip({ onClick } = {}) {
  const el = h(`<button type="button" class="warm-chip" hidden>
    <span class="wc-dot" aria-hidden="true"></span>
    <span class="wc-label"></span>
  </button>`);
  const labelEl = el.querySelector(".wc-label");
  el.addEventListener("click", (e) => {
    if (typeof onClick === "function") {
      e.stopPropagation();
      onClick(e);
    }
  });

  function update(warm) {
    const status = warm?.status;
    if (!warm || !status || status === "none") {
      el.hidden = true;
      return;
    }
    el.classList.remove(...WARM_CLASSES);
    let label = "";
    let title = "";
    let cls = "";
    if (status === "ready") {
      const short = shortCommit(warm.latestCommit || warm.commit || warm.latestKey || warm.key || warm.baseCommit);
      label = short ? `Warm · ${short}` : "Warm";
      title = short ? `Warm snapshot ready (${short})` : "Warm snapshot ready";
      cls = "wc-ready";
    } else if (status === "stale") {
      label = "Warm stale";
      title = "Warm snapshot is behind the latest commit";
      cls = "wc-stale";
    } else if (status === "building") {
      label = "Warm building";
      title = "Building warm snapshot";
      cls = "wc-building";
    } else if (status === "failed") {
      label = "Warm failed";
      title = warm.lastError ? `Warm snapshot failed: ${warm.lastError}` : "Warm snapshot failed";
      cls = "wc-failed";
    } else {
      el.hidden = true;
      return;
    }
    el.classList.add(cls);
    setText(labelEl, label);
    el.title = title;
    el.setAttribute("aria-label", title);
    el.hidden = false;
  }

  return { el, update };
}

/* Public Export 1: MachineScreen -------------------------------------- */

export function MachineScreen({ navigate, store, onOpenProject } = {}) {
  const el = h(`<section class="screen mp-screen">
    <header class="topbar mp-topbar">
      <a class="icon-btn back" href="/app" aria-label="Back">${icon("back")}</a>
      <div class="titles">
        <div class="title">Machine &amp; Cache</div>
        <div class="subtitle mp-sub-line"></div>
      </div>
      <div class="mp-head-acts">
        <button type="button" class="btn small mp-prune-btn" title="Prune local tool caches" aria-label="Prune local tool caches">
          ${icon("trash")}<span>Prune caches</span>
        </button>
        <button type="button" class="icon-btn mp-refresh-btn" aria-label="Refresh" title="Refresh">
          ${icon("refresh")}
        </button>
      </div>
    </header>
    <div class="scroller">
      <div class="mp-body">
        <div class="mp-skeleton">
          <div class="mp-sk-kpis">
            <div class="sk mp-sk-kpi"></div>
            <div class="sk mp-sk-kpi"></div>
            <div class="sk mp-sk-kpi"></div>
            <div class="sk mp-sk-kpi"></div>
          </div>
          <div class="mp-sk-grid">
            <div class="sk mp-sk-card"></div>
            <div class="sk mp-sk-card"></div>
          </div>
          <div class="sk mp-sk-card"></div>
        </div>
        <div class="card mp-err-card" hidden role="alert">
          <div class="mp-err-ic">${icon("alert")}</div>
          <div class="mp-err-body">
            <div class="mp-err-title">Could not load machine telemetry</div>
            <div class="mp-err-msg"></div>
          </div>
          <button type="button" class="btn small mp-retry-btn">Try again</button>
        </div>
        <div class="mp-content" hidden>
          <div class="mp-stale-bar" hidden role="status"></div>
          <div class="card mp-banner" hidden>
            <div class="mp-banner-ic">${icon("power")}</div>
            <div class="mp-banner-text">
              <div class="mp-banner-title"></div>
              <div class="mp-banner-sub"></div>
            </div>
          </div>
          <div class="mp-kpis"></div>
          <div class="mp-sections"></div>
        </div>
      </div>
    </div>
  </section>`);

  const bar = el.querySelector(".topbar");
  const scroller = el.querySelector(".scroller");
  const subLine = el.querySelector(".mp-sub-line");
  const backEl = el.querySelector(".back");
  const pruneBtn = el.querySelector(".mp-prune-btn");
  const pruneLabel = pruneBtn.querySelector("span");
  const refreshBtn = el.querySelector(".mp-refresh-btn");
  const skeletonEl = el.querySelector(".mp-skeleton");
  const errCardEl = el.querySelector(".mp-err-card");
  const errMsgEl = el.querySelector(".mp-err-msg");
  const retryBtn = el.querySelector(".mp-retry-btn");
  const contentEl = el.querySelector(".mp-content");
  const staleBarEl = el.querySelector(".mp-stale-bar");
  const bannerEl = el.querySelector(".mp-banner");
  const bannerTitleEl = el.querySelector(".mp-banner-title");
  const bannerSubEl = el.querySelector(".mp-banner-sub");
  const kpisEl = el.querySelector(".mp-kpis");
  const sectionsEl = el.querySelector(".mp-sections");

  backEl.addEventListener("click", (e) => {
    e.preventDefault();
    if (app.goBack) app.goBack("/app");
    else if (typeof navigate === "function") navigate("/app");
  });
  scroller.addEventListener("scroll", () => bar.classList.toggle("scrolled", scroller.scrollTop > 2), { passive: true });

  function openProject(workspaceId) {
    if (!workspaceId) return;
    if (typeof onOpenProject === "function") {
      onOpenProject(workspaceId);
      return;
    }
    const path = `/app/p/${encodeURIComponent(workspaceId)}`;
    if (typeof navigate === "function") navigate(path);
    else app.navigate?.(path);
  }

  /* Build stable KPI cards -------------------------------------------- */

  function makeKpiCard(title, { withPill = false } = {}) {
    const card = h(`<div class="card mp-kpi">
      <div class="mp-kpi-head">
        <span class="mp-kpi-label"></span>
        ${withPill ? '<span class="mp-pill mp-pill-muted"></span>' : ""}
      </div>
      <div class="mp-kpi-val">--</div>
      <div class="mp-kpi-sub"></div>
      <div class="mp-kpi-sub2"></div>
    </div>`);
    setText(card.querySelector(".mp-kpi-label"), title);
    return {
      el: card,
      pill: withPill ? card.querySelector(".mp-pill") : null,
      val: card.querySelector(".mp-kpi-val"),
      sub: card.querySelector(".mp-kpi-sub"),
      sub2: card.querySelector(".mp-kpi-sub2"),
    };
  }

  const kpiWorkspaces = makeKpiCard("Workspaces (VFS / CoW)");
  const kpiSccache = makeKpiCard("sccache Hit Rate");
  const kpiRemote = makeKpiCard("Remote Cache (S3)", { withPill: true });
  const kpiHost = makeKpiCard("Machine Storage & RAM");
  kpisEl.append(kpiWorkspaces.el, kpiSccache.el, kpiRemote.el, kpiHost.el);

  /* Build stable sections --------------------------------------------- */

  // 1. Remote Cache & Build Pipeline
  const pipeSec = makeSection("Remote Cache & Build Pipeline");
  const pipeGrid = h(`<div class="mp-pipe-grid"></div>`);

  const sccacheCard = h(`<div class="card mp-card">
    <div class="mp-card-head">
      <div class="mp-card-title">sccache Multilevel Cache</div>
      <span class="mp-pill mp-pill-muted"></span>
    </div>
    <div class="mp-card-meta"></div>
    <div class="mp-levels"></div>
    <div class="mp-stats"></div>
    <div class="mp-empty-note" hidden></div>
  </div>`);

  const turboCard = h(`<div class="card mp-card">
    <div class="mp-card-head">
      <div class="mp-card-title">Turborepo Remote Cache</div>
      <span class="mp-pill mp-pill-muted"></span>
    </div>
    <div class="mp-card-meta"></div>
    <div class="mp-stats"></div>
    <div class="mp-empty-note" hidden></div>
  </div>`);

  const fabricCard = h(`<div class="card mp-card">
    <div class="mp-card-head">
      <div class="mp-card-title">Fabric CAS (S3) &amp; FUSE VFS</div>
      <span class="mp-pill mp-pill-muted"></span>
    </div>
    <div class="mp-card-meta"></div>
    <div class="mp-stats"></div>
    <div class="mp-proof" hidden>
      <div class="mp-proof-head"><span class="mp-proof-label"></span><span class="mp-proof-val"></span></div>
      <div class="mp-meter"><i class="mp-meter-fill"></i></div>
    </div>
    <div class="mp-empty-note" hidden></div>
  </div>`);

  const burstCard = h(`<div class="card mp-card">
    <div class="mp-card-head">
      <div class="mp-card-title">Burst Build Pool</div>
      <span class="mp-pill mp-pill-muted"></span>
    </div>
    <div class="mp-card-meta"></div>
    <div class="mp-builders"></div>
    <div class="mp-empty-note" hidden></div>
  </div>`);

  pipeGrid.append(sccacheCard, turboCard, fabricCard, burstCard);

  const feedCard = h(`<div class="card mp-card mp-feed-card">
    <div class="mp-card-head">
      <div class="mp-card-title">Live Remote Cache Activity</div>
      <span class="mp-pill mp-pill-muted"></span>
    </div>
    <div class="mp-feed-wrap" hidden>
      <div class="mp-feed-head" aria-hidden="true">
        <span>When</span>
        <span>Tool</span>
        <span>Op</span>
        <span>Key</span>
        <span class="mp-r">Size</span>
        <span class="mp-r">Time</span>
      </div>
      <div class="mp-feed-rows"></div>
    </div>
    <div class="mp-empty-note mp-feed-empty">No remote cache traffic yet. Activity appears when a build uses the remote cache.</div>
  </div>`);

  pipeSec.body.append(pipeGrid, feedCard);

  // 2. Warm Snapshots & Overlay Workspaces
  const warmSec = makeSection("Warm Snapshots & Overlay Workspaces");
  const warmEmptyCard = h(`<div class="card mp-card mp-empty-card" hidden><div class="mp-empty-note"></div></div>`);
  const warmListEl = h(`<div class="mp-proj-list"></div>`);
  warmSec.body.append(warmEmptyCard, warmListEl);

  // 3. Local Tool Caches
  const cachesSec = makeSection("Local Tool Caches");
  const cachesCard = h(`<div class="card mp-card mp-cache-card">
    <div class="mp-cache-root" hidden></div>
    <div class="mp-cache-rows"></div>
    <div class="mp-empty-note" hidden></div>
  </div>`);
  cachesSec.body.append(cachesCard);

  // 4. Machine Health
  const healthSec = makeSection("Machine Health");
  const healthCard = h(`<div class="card mp-card mp-health-card">
    <div class="mp-health-empty mp-empty-note" hidden></div>
    <div class="mp-health-body" hidden>
      <div class="mp-health-grid">
        <div class="mp-health-col">
          <div class="mp-health-subhead">Memory</div>
          <div class="mp-health-bars mp-mem-bars"></div>
        </div>
        <div class="mp-health-col">
          <div class="mp-health-subhead">Disks</div>
          <div class="mp-health-bars mp-disk-bars"></div>
        </div>
      </div>
      <div class="mp-health-foot">
        <div class="mp-health-cpu">
          <div class="mp-health-subhead">CPU &amp; Load Average</div>
          <div class="mp-stats mp-cpu-stats"></div>
        </div>
        <div class="mp-health-psi" hidden>
          <div class="mp-health-subhead">Pressure Stall (PSI)</div>
          <div class="mp-psi-grid"></div>
        </div>
      </div>
    </div>
  </div>`);
  healthSec.body.append(healthCard);

  sectionsEl.append(pipeSec.el, warmSec.el, cachesSec.el, healthSec.el);

  /* Screen state ------------------------------------------------------ */

  let data = null;
  let loadError = null;
  let pruning = false;
  let refreshing = false;
  let destroyed = false;
  let lastFetchAt = 0;
  let reqSeq = 0;
  let activeCtrl = null;
  const expandedErrors = new Set();

  /* Paint routines ---------------------------------------------------- */

  function paintHeader() {
    if (!data) {
      setText(subLine, loadError ? "Unavailable" : "Loading...");
      pruneBtn.disabled = true;
      return;
    }
    const parts = [];
    if (data.homeConnected) {
      parts.push("Online");
    } else {
      parts.push(data.homeStatus === "stopped" || data.homeStatus === "suspended" ? "Sleeping" : "Offline");
    }
    const kind = hostKindLabel(data.hostKind);
    if (kind) parts.push(kind);
    if (data.version) parts.push(`v${String(data.version).replace(/^v/i, "")}`);
    const updated = ago(data.collectedAt || lastFetchAt);
    if (updated) parts.push(`Updated ${updated}`);
    setText(subLine, parts.join(" · "));

    pruneBtn.disabled = pruning || !data.homeConnected;
    pruneBtn.title = data.homeConnected
      ? "Prune local tool caches"
      : "Machine must be online to prune local caches";
  }

  function paintBanner() {
    if (!data || data.homeConnected) {
      bannerEl.hidden = true;
      return;
    }
    bannerEl.hidden = false;
    const st = String(data.homeStatus || "").toLowerCase();
    const sleeping = st === "stopped" || st === "suspended" || st === "sleeping" || st === "idle";
    setText(bannerTitleEl, sleeping ? "Cloud machine is sleeping" : "Cloud machine is disconnected");
    const detail = data.homeStatus ? `Status: ${data.homeStatus}. ` : "";
    setText(
      bannerSubEl,
      `${detail}Live host telemetry, VFS mounts, and local tool caches appear when the machine wakes. Cloud-side Remote Cache, Fabric CAS, and Burst Pool stats remain live below.`
    );
  }

  function paintKpis() {
    const vfs = data.vfs;
    const sc = data.toolCache?.sccache || null;
    const homeRc = data.remoteCache || null;
    const cloudRc = data.cloud?.remoteCache || null;
    const host = data.host || null;

    // 1. Workspaces (VFS / CoW)
    if (vfs) {
      const forks = num(vfs.totalForks) ?? 0;
      const plain = num(vfs.totalPlainWorkspaces) ?? 0;
      const total = forks + plain;
      setText(kpiWorkspaces.val, total > 0 ? `${forks} / ${total}` : "0");
      const modeText = vfsModeLabel(vfs.activeMode);
      const warmText = (num(vfs.totalWarmBytes) ?? 0) > 0 ? ` · ${formatBytes(vfs.totalWarmBytes)} warm` : "";
      setText(kpiWorkspaces.sub, total > 0 ? `${modeText} forks${warmText}` : `${modeText}${warmText}`);
      const ov = num(vfs.overlayMounts) ?? 0;
      const fm = num(vfs.fuseMounts) ?? 0;
      setText(kpiWorkspaces.sub2, `${ov} overlay · ${fm} FUSE`);
    } else {
      setText(kpiWorkspaces.val, "--");
      setText(kpiWorkspaces.sub, data.homeConnected ? "VFS status unavailable" : "Machine offline");
      setText(kpiWorkspaces.sub2, "");
    }

    // 2. sccache Hit Rate
    if (sc && sc.available) {
      const hits = num(sc.cacheHits) ?? 0;
      const misses = num(sc.cacheMisses) ?? 0;
      const reqs = num(sc.compileRequests) ?? (hits + misses);
      const pct = computeHitRatePct(hits, misses, sc.hitRate);
      setText(kpiSccache.val, pct != null && (hits + misses > 0 || reqs > 0) ? `${pct.toFixed(1)}%` : "--");
      setText(kpiSccache.sub, `${formatCount(hits)} hits / ${formatCount(misses)} misses · ${sccacheModeLabel(sc.mode)}`);
      if (num(sc.avgReadHitMs) != null || num(sc.avgCompilerMs) != null) {
        setText(kpiSccache.sub2, `Hit ${formatDurationMs(sc.avgReadHitMs)} vs compile ${formatDurationMs(sc.avgCompilerMs)}`);
      } else if (reqs === 0) {
        setText(kpiSccache.sub2, "No compiles yet");
      } else {
        const d = num(sc.activeDaemons) ?? 0;
        setText(kpiSccache.sub2, `${formatCount(reqs)} compiles · ${d} ${d === 1 ? "daemon" : "daemons"}`);
      }
    } else if (sc && !sc.available) {
      setText(kpiSccache.val, "Off");
      setText(kpiSccache.sub, "sccache not running");
      setText(kpiSccache.sub2, "");
    } else {
      setText(kpiSccache.val, "--");
      setText(kpiSccache.sub, data.homeConnected ? "No sccache stats" : "Machine offline");
      setText(kpiSccache.sub2, "");
    }

    // 3. Remote Cache (S3)
    const remoteTools = ["sccache", "turbo", "buck2"].map((tool) => pickRemoteToolStats(homeRc?.[tool], cloudRc?.[tool]));
    const total = (field) => remoteTools.reduce((sum, stats) => sum + (num(stats?.[field]) ?? 0), 0);
    const totalHits = total("hits");
    const totalWrites = total("writes");
    const totalRead = total("bytesRead");
    const totalWritten = total("bytesWritten");
    const lastAct = Math.max(
      ...remoteTools.map((stats) => toMs(stats?.lastActivityAt) || 0),
      toMs(homeRc?.recent?.[0]?.at) || 0,
      toMs(cloudRc?.recent?.[0]?.at) || 0
    );
    const remoteEnabled = Boolean(cloudRc?.enabled || homeRc?.proxyActive);
    const isRecentlyActive = lastAct > 0 && (Date.now() - lastAct) < ACTIVE_WINDOW_MS;

    if (!remoteEnabled) {
      setPill(kpiRemote.pill, "Off", "muted");
    } else if (isRecentlyActive) {
      setPill(kpiRemote.pill, "Active", "ok", { live: true });
    } else {
      setPill(kpiRemote.pill, "Idle", "muted");
    }

    setText(kpiRemote.val, remoteEnabled || totalHits + totalWrites > 0
      ? `${formatCount(totalHits)} / ${formatCount(totalWrites)}`
      : "--");
    setText(kpiRemote.sub, "hits / writes across build caches");
    setText(kpiRemote.sub2, remoteEnabled || totalRead + totalWritten > 0
      ? `${formatBytes(totalRead)} saved · ${formatBytes(totalWritten)} uploaded`
      : "Remote cache disabled");

    // 4. Machine Storage & RAM
    if (host) {
      const disks = Array.isArray(host.disks) ? host.disks : [];
      const wsDisk = disks.find((d) => d?.mount === "/workspace") || disks[0] || null;
      if (wsDisk && num(wsDisk.totalBytes) != null) {
        const used = Math.max(0, wsDisk.totalBytes - (num(wsDisk.freeBytes) ?? 0));
        setText(kpiHost.val, `${formatBytes(used)} / ${formatBytes(wsDisk.totalBytes)}`);
      } else {
        setText(kpiHost.val, "--");
      }
      const availMem = formatBytes(host.memAvailableBytes);
      const totMem = formatBytes(host.memTotalBytes);
      const diskTag = wsDisk?.mount ? `${wsDisk.mount} · ` : "";
      setText(kpiHost.sub, `${diskTag}RAM ${availMem} free / ${totMem}`);
      const l1 = num(host.load1);
      const cpus = num(host.cpus);
      const loadParts = [];
      if (l1 != null) loadParts.push(`Load ${l1.toFixed(2)}`);
      if (cpus != null) loadParts.push(`${cpus} ${cpus === 1 ? "CPU" : "CPUs"}`);
      setText(kpiHost.sub2, loadParts.join(" · "));
    } else {
      setText(kpiHost.val, "--");
      setText(kpiHost.sub, data.homeConnected ? "Host metrics unavailable" : "Machine offline");
      setText(kpiHost.sub2, "");
    }
  }

  function paintPipeline() {
    const sc = data.toolCache?.sccache || null;
    const homeRc = data.remoteCache || null;
    const cloudRc = data.cloud?.remoteCache || null;
    const fabric = data.cloud?.fabricCas || null;
    const burst = data.cloud?.burstPool || null;
    const vfs = data.vfs || null;

    const bucket = cloudRc?.bucket || fabric?.bucket || "";
    pipeSec.setMeta(bucket ? `S3: ${bucket}` : "");

    // Card A: sccache Multilevel Cache
    {
      const pill = sccacheCard.querySelector(".mp-pill");
      const meta = sccacheCard.querySelector(".mp-card-meta");
      const levelsEl = sccacheCard.querySelector(".mp-levels");
      const statsEl = sccacheCard.querySelector(".mp-stats");
      const emptyEl = sccacheCard.querySelector(".mp-empty-note");
      const rcSc = pickRemoteToolStats(homeRc?.sccache, cloudRc?.sccache);

      if (sc?.available) {
        const modeTxt = sccacheModeLabel(sc.mode);
        setPill(pill, modeTxt, modeTxt.includes("S3") ? "ok" : "accent");
      } else if (rcSc && (cloudRc?.enabled || homeRc?.proxyActive)) {
        setPill(pill, data.homeConnected ? "Cloud only" : "S3 live", "accent");
      } else {
        setPill(pill, data.homeConnected ? "Off" : "Offline", "muted");
      }

      const metaParts = [];
      if (sc?.version) metaParts.push(`sccache ${sc.version}`);
      if (num(sc?.activeDaemons) != null) metaParts.push(`${sc.activeDaemons} ${sc.activeDaemons === 1 ? "daemon" : "daemons"}`);
      if (sc?.cacheLocation) metaParts.push(sc.cacheLocation);
      if (homeRc?.proxyActive) {
        metaParts.push(homeRc.upstreamConnected ? "Proxy connected" : "Proxy waiting for upstream");
      }
      setText(meta, metaParts.join(" · "));
      meta.hidden = metaParts.length === 0;

      const levels = (Array.isArray(sc?.levels) ? sc.levels : []).map((lv, idx) => ({
        ...lv,
        _key: lv?.name ? `${lv.name}#${idx}` : `lv-${idx}`,
      }));
      levelsEl.hidden = levels.length === 0;
      reconcile(
        levelsEl,
        levels,
        (lv) => lv._key,
        () => h(`<div class="mp-level-row">
          <div class="mp-level-head">
            <span class="mp-level-name"></span>
            <span class="mp-level-nums"></span>
          </div>
          <div class="mp-meter"><i class="mp-meter-fill"></i></div>
        </div>`),
        (row, lv) => {
          const hits = num(lv.hits) ?? 0;
          const misses = num(lv.misses) ?? 0;
          const writes = num(lv.writes) ?? 0;
          const backfills = num(lv.backfills) ?? 0;
          const pct = hits + misses > 0 ? (hits / (hits + misses)) * 100 : 0;
          setText(row.querySelector(".mp-level-name"), lv.name || "Level");
          const parts = [
            `${formatCount(hits)} hits`,
            `${formatCount(misses)} misses`,
            `${formatCount(writes)} writes`,
          ];
          if (backfills > 0) parts.push(`${formatCount(backfills)} backfills`);
          setText(row.querySelector(".mp-level-nums"), parts.join(" · "));
          setMeter(row.querySelector(".mp-meter"), pct, "ok");
        }
      );

      const statItems = [];
      if (sc?.available) {
        statItems.push({ k: "Requests", v: formatCount(sc.compileRequests) });
        statItems.push({ k: "Total hits", v: formatCount(sc.cacheHits), ok: (num(sc.cacheHits) ?? 0) > 0 });
        statItems.push({ k: "Misses", v: formatCount(sc.cacheMisses) });
        if ((num(sc.nonCacheable) ?? 0) > 0) statItems.push({ k: "Non-cacheable", v: formatCount(sc.nonCacheable) });
        if ((num(sc.errors) ?? 0) > 0) statItems.push({ k: "Errors", v: formatCount(sc.errors), err: true });
      }
      if (rcSc) {
        statItems.push({ k: "S3 hits", v: formatCount(rcSc.hits), ok: (num(rcSc.hits) ?? 0) > 0 });
        statItems.push({ k: "S3 writes", v: formatCount(rcSc.writes) });
        statItems.push({ k: "S3 read", v: formatBytes(rcSc.bytesRead) });
        statItems.push({ k: "S3 uploaded", v: formatBytes(rcSc.bytesWritten) });
        if ((num(rcSc.errors) ?? 0) > 0) statItems.push({ k: "S3 errors", v: formatCount(rcSc.errors), err: true });
      }
      statsEl.hidden = statItems.length === 0;
      setStatsGrid(statsEl, statItems);

      const hasAny = levels.length > 0 || statItems.length > 0;
      emptyEl.hidden = hasAny;
      if (!hasAny) {
        setText(emptyEl, data.homeConnected
          ? "sccache is not active on this machine."
          : "Local sccache stats appear when the machine wakes.");
      }
    }

    // Card B: Turborepo Remote Cache
    {
      const pill = turboCard.querySelector(".mp-pill");
      const meta = turboCard.querySelector(".mp-card-meta");
      const statsEl = turboCard.querySelector(".mp-stats");
      const emptyEl = turboCard.querySelector(".mp-empty-note");
      const rcTurbo = pickRemoteToolStats(homeRc?.turbo, cloudRc?.turbo);
      const enabled = Boolean(cloudRc?.enabled || homeRc?.proxyActive);
      const lastAct = toMs(rcTurbo?.lastActivityAt);
      const active = lastAct && (Date.now() - lastAct) < ACTIVE_WINDOW_MS;

      if (!enabled && !rcTurbo) setPill(pill, "Off", "muted");
      else if (active) setPill(pill, "Active", "ok", { live: true });
      else if (enabled) setPill(pill, "Ready", "accent");
      else setPill(pill, "Idle", "muted");

      const metaParts = [];
      if (cloudRc?.bucket) metaParts.push(`Bucket: ${cloudRc.bucket}`);
      if (lastAct) metaParts.push(`Last activity ${ago(lastAct)}`);
      else if (enabled) metaParts.push("Waiting for Turbo builds");
      setText(meta, metaParts.join(" · "));
      meta.hidden = metaParts.length === 0;

      if (rcTurbo || enabled) {
        const hits = num(rcTurbo?.hits) ?? 0;
        const misses = num(rcTurbo?.misses) ?? 0;
        const writes = num(rcTurbo?.writes) ?? 0;
        const errors = num(rcTurbo?.errors) ?? 0;
        const rate = hits + misses > 0 ? `${((hits / (hits + misses)) * 100).toFixed(1)}%` : "--";
        const statItems = [
          { k: "Hits", v: formatCount(hits), ok: hits > 0 },
          { k: "Misses", v: formatCount(misses) },
          { k: "Hit rate", v: rate },
          { k: "Uploads", v: formatCount(writes) },
          { k: "Downloaded", v: formatBytes(rcTurbo?.bytesRead ?? 0) },
          { k: "Uploaded", v: formatBytes(rcTurbo?.bytesWritten ?? 0) },
        ];
        if (errors > 0) statItems.push({ k: "Errors", v: formatCount(errors), err: true });
        statsEl.hidden = false;
        setStatsGrid(statsEl, statItems);
        emptyEl.hidden = true;
      } else {
        statsEl.hidden = true;
        emptyEl.hidden = false;
        setText(emptyEl, "Remote cache is not enabled for this environment.");
      }
    }

    // Card C: Fabric CAS (S3) & FUSE VFS
    {
      const pill = fabricCard.querySelector(".mp-pill");
      const meta = fabricCard.querySelector(".mp-card-meta");
      const statsEl = fabricCard.querySelector(".mp-stats");
      const proofEl = fabricCard.querySelector(".mp-proof");
      const emptyEl = fabricCard.querySelector(".mp-empty-note");

      const casEnabled = Boolean(fabric?.enabled);
      const fuseMounts = num(vfs?.fuseMounts) ?? 0;
      const casReqs = num(fabric?.requests) ?? 0;
      const lastAct = toMs(fabric?.lastActivityAt);

      if (!casEnabled) {
        setPill(pill, "Off", "muted");
      } else if (fuseMounts > 0 || (lastAct && (Date.now() - lastAct) < ACTIVE_WINDOW_MS)) {
        setPill(pill, "Active", "ok", { live: true });
      } else {
        setPill(pill, "Idle", "accent");
      }

      const metaParts = [];
      if (fabric?.bucket) metaParts.push(`S3: ${fabric.bucket}`);
      if (vfs) {
        metaParts.push(`FUSE: ${fuseMounts} ${fuseMounts === 1 ? "mount" : "mounts"} (${vfs.fuseReady ? "ready" : "not ready"})`);
      } else {
        metaParts.push("FUSE: machine offline");
      }
      if (lastAct) metaParts.push(`Last ${ago(lastAct)}`);
      setText(meta, metaParts.join(" · "));
      meta.hidden = metaParts.length === 0;

      if (fabric && casEnabled) {
        const statItems = [
          { k: "Requests", v: formatCount(casReqs) },
          { k: "Gets", v: formatCount(fabric.gets) },
          { k: "Puts", v: formatCount(fabric.puts) },
          { k: "Roots", v: formatCount(fabric.rootsPublished) },
          { k: "Read", v: formatBytes(fabric.bytesRead ?? 0) },
          { k: "Written", v: formatBytes(fabric.bytesWritten ?? 0) },
        ];
        if ((num(fabric.errors) ?? 0) > 0) statItems.push({ k: "Errors", v: formatCount(fabric.errors), err: true });
        statsEl.hidden = false;
        setStatsGrid(statsEl, statItems);
        emptyEl.hidden = true;

        const pc = fabric.proofCache;
        if (pc && typeof pc === "object") {
          const pcHits = num(pc.hits) ?? 0;
          const pcMisses = num(pc.misses) ?? 0;
          const pcRate = pcHits + pcMisses > 0 ? `${((pcHits / (pcHits + pcMisses)) * 100).toFixed(1)}% hit` : "no lookups";
          const est = num(pc.estimated_bytes) ?? num(pc.estimatedBytes) ?? 0;
          const maxB = num(pc.max_bytes) ?? num(pc.maxBytes) ?? 0;
          const entries = num(pc.entries) ?? 0;
          setText(proofEl.querySelector(".mp-proof-label"), `Proof cache · ${pcRate} (${formatCount(entries)} entries)`);
          setText(proofEl.querySelector(".mp-proof-val"), maxB > 0 ? `${formatBytes(est)} / ${formatBytes(maxB)}` : formatBytes(est));
          setMeter(proofEl.querySelector(".mp-meter"), maxB > 0 ? clampPct(est, maxB) : 0);
          proofEl.hidden = false;
        } else {
          proofEl.hidden = true;
        }
      } else {
        statsEl.hidden = true;
        proofEl.hidden = true;
        emptyEl.hidden = false;
        setText(emptyEl, "Fabric S3 CAS is not enabled.");
      }
    }

    // Card D: Burst Build Pool
    {
      const pill = burstCard.querySelector(".mp-pill");
      const meta = burstCard.querySelector(".mp-card-meta");
      const buildersEl = burstCard.querySelector(".mp-builders");
      const emptyEl = burstCard.querySelector(".mp-empty-note");

      const enabled = Boolean(burst?.enabled);
      const builders = Array.isArray(burst?.builders) ? burst.builders : [];
      const leasedCount = builders.filter((b) => b?.leased).length;

      if (!enabled) {
        setPill(pill, "Off", "muted");
        meta.hidden = true;
        buildersEl.hidden = true;
        buildersEl.replaceChildren();
        emptyEl.hidden = false;
        setText(emptyEl, "Off - builds run locally on this home.");
      } else {
        if (leasedCount > 0) setPill(pill, `${leasedCount} leased`, "accent", { live: true });
        else if (builders.length > 0) setPill(pill, `${builders.length} ready`, "ok");
        else setPill(pill, "Enabled", "ok");

        const pol = burst.policy;
        if (pol) {
          const maxN = num(pol.maxPerHome) ?? 0;
          const cpus = num(pol.cpus) ?? 0;
          const mem = formatMemoryMb(pol.memoryMb);
          const idle = num(pol.idleSeconds);
          const polParts = [];
          if (maxN && cpus) polParts.push(`${maxN} x ${cpus} CPU${mem ? ` / ${mem}` : ""}`);
          if (idle != null) polParts.push(`${idle}s idle timeout`);
          setText(meta, polParts.join(" · "));
          meta.hidden = polParts.length === 0;
        } else {
          meta.hidden = true;
        }

        buildersEl.hidden = builders.length === 0;
        emptyEl.hidden = builders.length > 0;
        if (builders.length === 0) {
          setText(emptyEl, "No builders running right now. Builders spin up on demand.");
        }

        const keyedBuilders = builders.map((b, idx) => ({
          ...b,
          _key: b?.id || `slot-${b?.slot ?? idx}`,
        }));

        reconcile(
          buildersEl,
          keyedBuilders,
          (b) => b._key,
          () => h(`<div class="mp-builder-row">
            <div class="mp-builder-main">
              <span class="mp-builder-id"></span>
              <span class="mp-pill mp-pill-muted"></span>
            </div>
            <div class="mp-builder-sub"></div>
            <div class="mp-builder-err" hidden></div>
          </div>`),
          (row, b) => {
            const label = b.slot != null ? `Slot ${b.slot} · ${b.id || "builder"}` : (b.id || "builder");
            setText(row.querySelector(".mp-builder-id"), label);
            const bPill = row.querySelector(".mp-pill");
            const st = String(b.status || b.machineState || "idle").toLowerCase();
            if (b.leased) setPill(bPill, "Leased", "accent", { live: true });
            else if (b.error || st === "failed" || st === "error") setPill(bPill, "Error", "err");
            else if (st === "ready" || st === "started" || st === "idle") setPill(bPill, "Ready", "ok");
            else if (st === "starting" || st === "creating") setPill(bPill, "Starting", "accent", { live: true });
            else setPill(bPill, b.status || b.machineState || "Stopped", "muted");

            const subParts = [];
            if (b.leased && b.leaseHolder) subParts.push(`Holder: ${b.leaseHolder}`);
            if (b.machineState && b.machineState !== b.status) subParts.push(b.machineState);
            const used = ago(b.lastUsedAt);
            if (used) subParts.push(`Used ${used}`);
            setText(row.querySelector(".mp-builder-sub"), subParts.join(" · "));

            const errEl = row.querySelector(".mp-builder-err");
            errEl.hidden = !b.error;
            if (b.error) setText(errEl, String(b.error));
          }
        );
      }
    }

    // Live Remote Cache Activity Feed
    {
      const pill = feedCard.querySelector(".mp-pill");
      const wrap = feedCard.querySelector(".mp-feed-wrap");
      const rowsEl = feedCard.querySelector(".mp-feed-rows");
      const emptyEl = feedCard.querySelector(".mp-feed-empty");

      const events = mergeRecentEvents(homeRc?.recent, cloudRc?.recent);
      const seenKeys = new Map();
      const keyedEvents = events.map((ev) => {
        const base = `${ev._ms}|${ev.tool}|${ev.op}|${ev.key}|${ev.bytes ?? ""}`;
        const n = seenKeys.get(base) || 0;
        seenKeys.set(base, n + 1);
        return { ...ev, _key: n ? `${base}#${n}` : base };
      });
      if (keyedEvents.length > 0) {
        const latestMs = keyedEvents[0]._ms;
        const fresh = latestMs && (Date.now() - latestMs) < ACTIVE_WINDOW_MS;
        setPill(pill, `${keyedEvents.length} recent`, fresh ? "ok" : "muted", { live: fresh });
      } else {
        setPill(pill, homeRc?.proxyActive || cloudRc?.enabled ? "Listening" : "Idle", "muted");
      }

      wrap.hidden = keyedEvents.length === 0;
      emptyEl.hidden = keyedEvents.length > 0;

      reconcile(
        rowsEl,
        keyedEvents,
        (ev) => ev._key,
        () => h(`<div class="mp-feed-row">
          <span class="mp-feed-time"></span>
          <span class="mp-feed-tool"></span>
          <span class="mp-feed-op"></span>
          <span class="mp-feed-key"></span>
          <span class="mp-feed-size mp-r"></span>
          <span class="mp-feed-ms mp-r"></span>
        </div>`),
        (row, ev) => {
          setText(row.querySelector(".mp-feed-time"), ago(ev.at) || "--");
          const toolEl = row.querySelector(".mp-feed-tool");
          const toolName = String(ev.tool || "cache").toLowerCase();
          setText(toolEl, toolName);
          toolEl.dataset.tool = toolName;

          const opInfo = normalizeOp(ev.op);
          const opEl = row.querySelector(".mp-feed-op");
          opEl.className = `mp-feed-op ${opInfo.cls}`;
          setText(opEl, opInfo.label);

          const keyEl = row.querySelector(".mp-feed-key");
          const fullKey = String(ev.key || "--");
          setText(keyEl, fullKey);
          keyEl.title = fullKey;

          setText(row.querySelector(".mp-feed-size"), ev.bytes != null ? formatBytes(ev.bytes) : "--");
          setText(row.querySelector(".mp-feed-ms"), ev.ms != null ? formatDurationMs(ev.ms) : "--");
        }
      );
    }
  }

  function paintWarmProjects() {
    const vfs = data.vfs;
    const projects = Array.isArray(vfs?.projects) ? vfs.projects : [];
    warmSec.setMeta(vfs ? `${projects.length} ${projects.length === 1 ? "project" : "projects"}` : "");

    if (!vfs) {
      warmEmptyCard.hidden = false;
      setText(
        warmEmptyCard.querySelector(".mp-empty-note"),
        data.homeConnected
          ? "No warm snapshot information reported by the machine."
          : "Warm snapshot and overlay workspace details appear when the machine wakes."
      );
      warmListEl.hidden = true;
      warmListEl.replaceChildren();
      return;
    }

    if (projects.length === 0) {
      warmEmptyCard.hidden = false;
      setText(warmEmptyCard.querySelector(".mp-empty-note"), "No projects with warm snapshots on this machine yet.");
      warmListEl.hidden = true;
      warmListEl.replaceChildren();
      return;
    }

    warmEmptyCard.hidden = true;
    warmListEl.hidden = false;

    reconcile(
      warmListEl,
      projects,
      (p) => p.workspaceId || p.name || p.repo,
      () => {
        const card = h(`<div class="card mp-card mp-proj-card">
          <div class="mp-proj-head">
            <div class="mp-proj-ident">
              <a class="mp-proj-link" href="/app"></a>
              <div class="mp-proj-repo"></div>
            </div>
            <span class="mp-pill mp-pill-muted"></span>
          </div>
          <div class="mp-proj-meta"></div>
          <div class="mp-layers">
            <div class="mp-layers-head">
              <span class="mp-layers-label"></span>
              <span class="mp-layers-warn" hidden>Compaction next</span>
            </div>
            <div class="mp-layers-bar"></div>
          </div>
          <div class="mp-proj-err" hidden>
            <button type="button" class="mp-proj-err-toggle" aria-expanded="false">
              ${icon("chevron")}
              <span class="mp-proj-err-summary"></span>
            </button>
            <pre class="mp-proj-err-pre" hidden></pre>
          </div>
          <div class="mp-chips"></div>
        </div>`);

        const link = card.querySelector(".mp-proj-link");
        link.addEventListener("click", (e) => {
          if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey || e.button !== 0) return;
          e.preventDefault();
          openProject(card.dataset.wsId);
        });

        const errBtn = card.querySelector(".mp-proj-err-toggle");
        errBtn.addEventListener("click", () => {
          const id = card.dataset.wsId;
          if (!id) return;
          if (expandedErrors.has(id)) expandedErrors.delete(id);
          else expandedErrors.add(id);
          const open = expandedErrors.has(id);
          errBtn.setAttribute("aria-expanded", String(open));
          card.querySelector(".mp-proj-err").classList.toggle("open", open);
          card.querySelector(".mp-proj-err-pre").hidden = !open;
        });

        return card;
      },
      (card, p) => {
        const wsId = p.workspaceId || "";
        card.dataset.wsId = wsId;
        const link = card.querySelector(".mp-proj-link");
        setText(link, p.name || p.repo || wsId || "Project");
        link.href = wsId ? `/app/p/${encodeURIComponent(wsId)}` : "/app";

        const repoEl = card.querySelector(".mp-proj-repo");
        const showRepo = p.repo && p.repo !== p.name;
        repoEl.hidden = !showRepo;
        if (showRepo) setText(repoEl, p.repo);

        // Status pill
        const pill = card.querySelector(".mp-pill");
        const st = p.status || "none";
        if (st === "ready") setPill(pill, "Ready", "ok");
        else if (st === "stale") setPill(pill, "Stale", "warn");
        else if (st === "building") setPill(pill, "Building", "accent", { live: true });
        else if (st === "failed") setPill(pill, "Failed", "err");
        else setPill(pill, "None", "muted");

        // Snapshot metadata
        const metaParts = [];
        const baseShort = shortCommit(p.baseCommit);
        const latestShort = shortCommit(p.latestCommit);
        const keyShort = shortCommit(p.latestKey);
        if (latestShort && baseShort && latestShort !== baseShort) {
          metaParts.push(`Snapshot ${latestShort} -> base ${baseShort}`);
        } else if (baseShort || latestShort || keyShort) {
          metaParts.push(`Snapshot ${latestShort || baseShort || keyShort}`);
        }
        if (p.buildSeconds != null) {
          const sec = formatSeconds(p.buildSeconds);
          if (sec) metaParts.push(`Built in ${sec}`);
        }
        const builtAgo = ago(p.builtAt);
        if (builtAgo) metaParts.push(builtAgo);
        if (num(p.warmBytes) != null) metaParts.push(`${formatBytes(p.warmBytes)} warm`);
        if ((num(p.failedCount) ?? 0) > 0) metaParts.push(`${p.failedCount} failed`);
        setText(card.querySelector(".mp-proj-meta"), metaParts.join(" · "));

        // Visual layer bar
        const layers = Math.max(0, Math.round(num(p.layers) ?? 0));
        const maxLayers = Math.max(1, Math.round(num(p.maxLayers) ?? 8));
        const warnCompaction = layers >= Math.max(1, maxLayers - 1);
        setText(
          card.querySelector(".mp-layers-label"),
          `${layers} / ${maxLayers} ${maxLayers === 1 ? "layer" : "layers"}`
        );
        const warnEl = card.querySelector(".mp-layers-warn");
        warnEl.hidden = !warnCompaction;

        const barEl = card.querySelector(".mp-layers-bar");
        const sig = `${layers}/${maxLayers}/${warnCompaction ? 1 : 0}`;
        if (barEl.dataset.sig !== sig) {
          barEl.dataset.sig = sig;
          barEl.replaceChildren();
          if (maxLayers <= 16) {
            barEl.className = "mp-layers-bar mp-layers-segs";
            for (let i = 0; i < maxLayers; i++) {
              const seg = document.createElement("span");
              seg.className = "mp-layers-seg";
              if (i < layers) {
                seg.classList.add("filled");
                if (warnCompaction) seg.classList.add("warn");
              }
              barEl.appendChild(seg);
            }
          } else {
            barEl.className = "mp-layers-bar";
            const meter = h(`<div class="mp-meter"><i class="mp-meter-fill"></i></div>`);
            setMeter(meter, clampPct(layers, maxLayers), warnCompaction ? "warm" : "ok");
            barEl.appendChild(meter);
          }
        }

        // Collapsible error line
        const errWrap = card.querySelector(".mp-proj-err");
        const hasErr = Boolean(p.lastError);
        errWrap.hidden = !hasErr;
        if (hasErr) {
          const open = expandedErrors.has(wsId);
          errWrap.classList.toggle("open", open);
          const errBtn = errWrap.querySelector(".mp-proj-err-toggle");
          errBtn.setAttribute("aria-expanded", String(open));
          const oneLine = String(p.lastError).replace(/\s+/g, " ").trim();
          setText(errWrap.querySelector(".mp-proj-err-summary"), `Last build error: ${oneLine}`);
          const pre = errWrap.querySelector(".mp-proj-err-pre");
          pre.hidden = !open;
          setText(pre, String(p.lastError));
        }

        // Fork chips & plain checkout chips
        const chipsEl = card.querySelector(".mp-chips");
        const forks = Array.isArray(p.forks) ? p.forks : [];
        const plains = Array.isArray(p.plainWorkspaces) ? p.plainWorkspaces : [];
        const chipItems = [];
        for (const f of forks) {
          if (!f || !f.runId) continue;
          chipItems.push({ kind: "fork", key: `f:${f.runId}`, f });
        }
        for (const name of plains) {
          if (!name) continue;
          chipItems.push({ kind: "plain", key: `p:${name}`, name: String(name) });
        }
        if (chipItems.length === 0) {
          chipItems.push({ kind: "empty", key: "_empty" });
        }

        reconcile(
          chipsEl,
          chipItems,
          (it) => it.key,
          (it) => {
            if (it.kind === "empty") return h(`<span class="mp-chips-empty">No active workspaces</span>`);
            if (it.kind === "plain") {
              return h(`<span class="mp-ws-chip mp-ws-chip-plain"><span class="mp-ws-name"></span><span class="mp-ws-meta">full checkout</span></span>`);
            }
            return h(`<span class="mp-ws-chip"><span class="mp-ws-dot" aria-hidden="true"></span><span class="mp-ws-name"></span><span class="mp-ws-meta"></span></span>`);
          },
          (chip, it) => {
            if (it.kind === "empty") return;
            if (it.kind === "plain") {
              setText(chip.querySelector(".mp-ws-name"), it.name);
              chip.title = `${it.name} (full checkout)`;
              return;
            }
            const f = it.f;
            chip.classList.toggle("active", !!f.active);
            setText(chip.querySelector(".mp-ws-name"), f.runId);
            const changed = num(f.upperFiles);
            const metaText = changed != null
              ? `${formatCount(changed)} changed ${changed === 1 ? "file" : "files"}`
              : (f.active ? "active" : "idle");
            setText(chip.querySelector(".mp-ws-meta"), metaText);
            const tipParts = [f.runId];
            if (f.snapshotKey) tipParts.push(`snapshot ${shortCommit(f.snapshotKey)}`);
            if (num(f.layers) != null) tipParts.push(`${f.layers} ${f.layers === 1 ? "layer" : "layers"}`);
            if (changed != null) tipParts.push(`${changed} changed files`);
            chip.title = tipParts.join(" · ");
          }
        );
      }
    );
  }

  function paintToolCaches() {
    const tc = data.toolCache;
    const rootEl = cachesCard.querySelector(".mp-cache-root");
    const rowsEl = cachesCard.querySelector(".mp-cache-rows");
    const emptyEl = cachesCard.querySelector(".mp-empty-note");

    if (!tc) {
      cachesSec.setMeta("");
      rootEl.hidden = true;
      rowsEl.hidden = true;
      rowsEl.replaceChildren();
      emptyEl.hidden = false;
      setText(
        emptyEl,
        data.homeConnected
          ? "No local tool cache data reported."
          : "Local tool cache sizes appear when the machine wakes."
      );
      return;
    }

    cachesSec.setMeta(formatBytes(tc.totalBytes));
    rootEl.hidden = !tc.root;
    if (tc.root) setText(rootEl, `Root: ${tc.root}`);

    const entries = Array.isArray(tc.entries) ? tc.entries : [];
    rowsEl.hidden = entries.length === 0;
    emptyEl.hidden = entries.length > 0;
    if (entries.length === 0) {
      setText(emptyEl, "No local tool cache directories yet.");
      return;
    }

    reconcile(
      rowsEl,
      entries,
      (e) => e.name,
      () => h(`<div class="mp-cache-row">
        <div class="mp-cache-top">
          <span class="mp-cache-title"></span>
          <span class="mp-cache-nums"></span>
        </div>
        <div class="mp-meter"><i class="mp-meter-fill"></i></div>
        <div class="mp-cache-sub"></div>
      </div>`),
      (row, e) => {
        const bytes = num(e.bytes) ?? 0;
        const limit = num(e.limitBytes);
        const hasLimit = limit != null && limit > 0;
        const pct = hasLimit ? clampPct(bytes, limit) : clampPct(bytes, tc.totalBytes || 1);

        setText(row.querySelector(".mp-cache-title"), e.name);
        setText(
          row.querySelector(".mp-cache-nums"),
          hasLimit
            ? `${formatBytes(bytes)} / ${formatBytes(limit)} (${Math.round(pct)}%)`
            : `${formatBytes(bytes)} (no limit)`
        );
        setMeter(row.querySelector(".mp-meter"), pct, hasLimit ? levelOf(pct) : "ok");

        const subParts = [];
        if (num(e.fileCount) != null) subParts.push(`${formatCount(e.fileCount)} ${e.fileCount === 1 ? "object" : "objects"}`);
        if (num(e.recentFiles24h) != null) subParts.push(`${formatCount(e.recentFiles24h)} written in 24h`);
        const lw = ago(e.lastWriteAt);
        subParts.push(lw ? `Last write ${lw}` : "No recent writes");
        setText(row.querySelector(".mp-cache-sub"), subParts.join(" · "));
      }
    );
  }

  function paintHealth() {
    const host = data.host;
    const emptyEl = healthCard.querySelector(".mp-health-empty");
    const bodyEl = healthCard.querySelector(".mp-health-body");

    if (!host) {
      healthSec.setMeta("");
      emptyEl.hidden = false;
      bodyEl.hidden = true;
      setText(
        emptyEl,
        data.homeConnected
          ? "Host telemetry unavailable."
          : "Machine health metrics appear when the machine wakes."
      );
      return;
    }

    const cpus = num(host.cpus);
    const kind = hostKindLabel(data.hostKind);
    const metaParts = [];
    if (cpus != null) metaParts.push(`${cpus} ${cpus === 1 ? "CPU" : "CPUs"}`);
    if (kind) metaParts.push(kind);
    healthSec.setMeta(metaParts.join(" · "));

    emptyEl.hidden = true;
    bodyEl.hidden = false;

    // Memory & swap bars
    const memItems = [];
    const memTotal = num(host.memTotalBytes) ?? 0;
    const memAvail = num(host.memAvailableBytes) ?? 0;
    const memUsed = Math.max(0, memTotal - memAvail);
    const memPct = clampPct(memUsed, memTotal);
    memItems.push({
      key: "ram",
      label: "RAM",
      right: `${formatBytes(memUsed)} / ${formatBytes(memTotal)} (${Math.round(memPct)}%)`,
      sub: `${formatBytes(memAvail)} available`,
      pct: memPct,
    });

    const swapTotal = num(host.swapTotalBytes) ?? 0;
    if (swapTotal > 0) {
      const swapFree = num(host.swapFreeBytes) ?? swapTotal;
      const swapUsed = Math.max(0, swapTotal - swapFree);
      const swapPct = clampPct(swapUsed, swapTotal);
      memItems.push({
        key: "swap",
        label: "Swap",
        right: `${formatBytes(swapUsed)} / ${formatBytes(swapTotal)} (${Math.round(swapPct)}%)`,
        sub: `${formatBytes(swapFree)} free`,
        pct: swapPct,
      });
    } else {
      memItems.push({
        key: "swap",
        label: "Swap",
        right: "Disabled",
        sub: "",
        pct: 0,
        noBar: true,
      });
    }

    paintHealthBars(healthCard.querySelector(".mp-mem-bars"), memItems);

    // Disk bars
    const disks = Array.isArray(host.disks) ? host.disks : [];
    const diskItems = disks.map((d, idx) => {
      const total = num(d.totalBytes) ?? 0;
      const free = num(d.freeBytes) ?? 0;
      const used = Math.max(0, total - free);
      const pct = clampPct(used, total);
      return {
        key: d.mount || `disk-${idx}`,
        label: d.mount || "Disk",
        mono: true,
        right: `${formatBytes(used)} / ${formatBytes(total)} (${Math.round(pct)}%)`,
        sub: `${formatBytes(free)} free`,
        pct,
      };
    });
    paintHealthBars(healthCard.querySelector(".mp-disk-bars"), diskItems);

    // CPU & load average
    const l1 = num(host.load1);
    const l5 = num(host.load5);
    const l15 = num(host.load15);
    const cpuItems = [
      { k: "CPUs", v: cpus != null ? String(cpus) : "--" },
      { k: "Load 1m", v: l1 != null ? l1.toFixed(2) : "--", err: cpus != null && l1 != null && l1 >= cpus * 1.5 },
      { k: "Load 5m", v: l5 != null ? l5.toFixed(2) : "--" },
      { k: "Load 15m", v: l15 != null ? l15.toFixed(2) : "--" },
    ];
    setStatsGrid(healthCard.querySelector(".mp-cpu-stats"), cpuItems);

    // Linux PSI stall indicators (only when non-null)
    const p = host.pressure || null;
    const psiItems = [];
    if (p) {
      const defs = [
        ["memorySome", "Memory (some)"],
        ["memoryFull", "Memory (full)"],
        ["ioSome", "I/O (some)"],
        ["cpuSome", "CPU (some)"],
      ];
      for (const [field, label] of defs) {
        const val = num(p[field]);
        if (val != null) psiItems.push({ key: field, label, val });
      }
    }
    const psiWrap = healthCard.querySelector(".mp-health-psi");
    psiWrap.hidden = psiItems.length === 0;
    if (psiItems.length > 0) {
      reconcile(
        psiWrap.querySelector(".mp-psi-grid"),
        psiItems,
        (it) => it.key,
        () => h(`<div class="mp-psi-item"><span class="mp-psi-k"></span><span class="mp-psi-v"></span></div>`),
        (tile, it) => {
          setText(tile.querySelector(".mp-psi-k"), it.label);
          const vEl = tile.querySelector(".mp-psi-v");
          const formatted = it.val > 0 && it.val < 0.05 ? "<0.1%" : `${it.val.toFixed(1)}%`;
          setText(vEl, formatted);
          tile.classList.remove("lv-ok", "lv-warm", "lv-hot");
          tile.classList.add(`lv-${psiLevel(it.val)}`);
        }
      );
    }
  }

  function paintHealthBars(container, items) {
    reconcile(
      container,
      items,
      (it) => it.key,
      () => h(`<div class="mp-hbar">
        <div class="mp-hbar-head">
          <span class="mp-hbar-label"></span>
          <span class="mp-hbar-right"></span>
        </div>
        <div class="mp-meter"><i class="mp-meter-fill"></i></div>
        <div class="mp-hbar-sub"></div>
      </div>`),
      (row, it) => {
        const labelEl = row.querySelector(".mp-hbar-label");
        setText(labelEl, it.label);
        labelEl.classList.toggle("mp-v-mono", !!it.mono);
        setText(row.querySelector(".mp-hbar-right"), it.right);
        const meter = row.querySelector(".mp-meter");
        meter.hidden = !!it.noBar;
        if (!it.noBar) setMeter(meter, it.pct);
        const subEl = row.querySelector(".mp-hbar-sub");
        subEl.hidden = !it.sub;
        if (it.sub) setText(subEl, it.sub);
      }
    );
  }

  function paintAll() {
    paintHeader();
    if (!data) {
      skeletonEl.hidden = !!loadError;
      errCardEl.hidden = !loadError;
      contentEl.hidden = true;
      if (loadError) setText(errMsgEl, loadError);
      return;
    }
    skeletonEl.hidden = true;
    errCardEl.hidden = true;
    contentEl.hidden = false;

    staleBarEl.hidden = !loadError;
    if (loadError) setText(staleBarEl, `Refresh failed: ${loadError}. Showing last update.`);

    paintBanner();
    paintKpis();
    paintPipeline();
    paintWarmProjects();
    paintHealth();
    paintToolCaches();
  }

  /* Data fetching & actions ------------------------------------------- */

  async function load({ refresh = false, manual = false } = {}) {
    if (destroyed) return;
    if (!refresh && activeCtrl) return;
    if (refresh && activeCtrl) activeCtrl.abort();

    const seq = ++reqSeq;
    const ctrl = new AbortController();
    activeCtrl = ctrl;
    if (manual) {
      refreshing = true;
      refreshBtn.classList.add("spinning");
    }

    try {
      const next = await api(refresh ? "/api/app/machine?refresh=1" : "/api/app/machine", {
        signal: ctrl.signal,
      });
      if (destroyed || seq !== reqSeq) return;
      data = next;
      loadError = null;
      lastFetchAt = Date.now();
      paintAll();
    } catch (err) {
      if (destroyed || seq !== reqSeq || ctrl.signal.aborted) return;
      if (err?.status === 401) return;
      loadError = err?.message || "Could not reach machine telemetry";
      if (manual) showError(err);
      paintAll();
    } finally {
      if (activeCtrl === ctrl) activeCtrl = null;
      if (manual && !destroyed) {
        refreshing = false;
        refreshBtn.classList.remove("spinning");
      }
    }
  }

  async function handlePrune() {
    if (pruning || destroyed) return;
    pruning = true;
    pruneBtn.disabled = true;
    setText(pruneLabel, "Pruning...");
    try {
      const res = await api("/api/app/machine/prune", { method: "POST" });
      if (destroyed) return;
      const freed = extractFreedBytes(res);
      if (freed > 0) {
        toast(`Freed ${formatBytes(freed)} from caches`);
      } else if (typeof res?.message === "string" && res.message.trim()) {
        toast(res.message.trim());
      } else {
        toast("Caches are already within limits");
      }
      await load({ refresh: true });
    } catch (err) {
      if (!destroyed) showError(err);
    } finally {
      pruning = false;
      if (!destroyed) {
        setText(pruneLabel, "Prune caches");
        paintHeader();
      }
    }
  }

  refreshBtn.addEventListener("click", () => {
    if (!refreshing) void load({ refresh: true, manual: true });
  });
  retryBtn.addEventListener("click", () => void load({ refresh: true, manual: true }));
  pruneBtn.addEventListener("click", () => void handlePrune());

  /* Auto-polling while mounted & visible ------------------------------ */

  const pollTimer = setInterval(() => {
    if (!destroyed && document.visibilityState === "visible") {
      void load({ refresh: false });
    }
  }, POLL_MS);

  const onVisibility = () => {
    if (!destroyed && document.visibilityState === "visible" && Date.now() - lastFetchAt >= POLL_MS) {
      void load({ refresh: false });
    }
  };
  document.addEventListener("visibilitychange", onVisibility);

  // Initial load on mount
  paintAll();
  void load({ refresh: false });

  function update() {
    if (destroyed) return;
    const homeStatus = store?.state?.home?.status || app.store?.state?.home?.status;
    if (data && !data.homeConnected && homeStatus === "connected" && Date.now() - lastFetchAt >= 2000) {
      void load({ refresh: false });
      return;
    }
    if (data) paintHeader();
  }

  function destroy() {
    destroyed = true;
    clearInterval(pollTimer);
    document.removeEventListener("visibilitychange", onVisibility);
    activeCtrl?.abort();
    activeCtrl = null;
  }

  return { el, update, destroy };
}
