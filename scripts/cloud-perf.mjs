// Rudder Cloud typing latency and fidelity, measured two ways.
//
// emulate (default): the real attach client (dist/index.js) -> TCP delay ->
// a relay with the control plane's forwarding and deflate settings <- TCP
// delay <- a home emulator (the real cloud/worker/terminal-transport.mjs and
// rudder-native in a PTY, in a disposable repo with fake agents). Measures
// keystroke echo on the laptop screen, per-character lag while typing, and
// whether every character arrives and stays on screen. Touches nothing remote.
//
//   node scripts/cloud-perf.mjs --client-ms 45 --home-ms 46 --predict 1
//   node scripts/cloud-perf.mjs --direct            # dashboard alone, no relay
//   scenarios: slow,burst,fast15,fast5,gap40..gap220,utf8,glyphs,esc,stream,first
//
// probe <workspace-id> [samples]: the live path, without typing. Attaches to a
// home project's terminal channel and sends only WS pings (answered by the
// control plane) and {"type":"probe"} frames (answered by the home), paired,
// so the difference is the control plane <-> home leg. It never sends input,
// resize, repaint or theme, and leaves if another terminal holds control. Use
// a project nobody is attached to: while it runs it holds that channel's
// control. Needs `rudder login` (~/.rudder/cloud.json).
//
// Needs target/release/rudder-native (or RUDDER_NATIVE_BIN), dist/ built, jj.
import fs from "node:fs";
import fsp from "node:fs/promises";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { parseArgs } from "node:util";
import { fileURLToPath } from "node:url";
import crypto from "node:crypto";

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const require = createRequire(path.join(REPO, "package.json"));
const pty = require(path.join(REPO, "cloud/worker/node_modules/node-pty"));
const { WebSocket, WebSocketServer } = require("ws");
const { Terminal } = require("@xterm/headless");
const { createTerminalTransport } = await import(path.join(REPO, "cloud/worker/terminal-transport.mjs"));
const { scratchRepo, fakeBackends, removeScratch } = await import(path.join(REPO, "tests/tui/helpers.mjs"));

const { values: opt } = parseArgs({
  options: {
    "client-ms": { type: "string", default: "0" },
    "home-ms": { type: "string", default: "0" },
    "jitter-ms": { type: "string", default: "0" },
    predict: { type: "string", default: "1" },
    scenarios: { type: "string", default: "first,slow,burst,fast15,gap120,utf8,stream" },
    cols: { type: "string", default: "140" },
    rows: { type: "string", default: "40" },
    out: { type: "string", default: "" },
    direct: { type: "boolean", default: false },
  },
  allowPositionals: true,
});
const CLIENT_MS = Number(opt["client-ms"]), HOME_MS = Number(opt["home-ms"]), JITTER = Number(opt["jitter-ms"]);
const COLS = Number(opt.cols), ROWS = Number(opt.rows);
const nativeBin = process.env.RUDDER_NATIVE_BIN ?? path.join(REPO, "target/release/rudder-native");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const now = () => performance.now();
const log = (...a) => console.log(...a);

// ---------------------------------------------------------------- screens
function makeScreen(name) {
  const term = new Terminal({ cols: COLS, rows: ROWS, allowProposedApi: true, scrollback: 0 });
  let sync = false, carry = "";
  const listeners = new Set();
  const s = {
    term, name,
    text() { const b = term.buffer.active; const out = []; for (let y = 0; y < term.rows; y++) out.push(b.getLine(y)?.translateToString(true) ?? ""); return out.join("\n"); },
    get inSync() { return sync; },
    feed(data) {
      const str = typeof data === "string" ? data : data.toString("utf8");
      const scan = carry + str;
      // Synchronized output: only judge the picture between frames.
      const on = scan.lastIndexOf("\x1b[?2026h"), off = scan.lastIndexOf("\x1b[?2026l");
      if (on > off) sync = true; else if (off > on) sync = false;
      carry = scan.slice(-8);
      term.write(str, () => { for (const fn of listeners) fn(s); });
    },
    on(fn) { listeners.add(fn); return () => listeners.delete(fn); },
    resize(c, r) { term.resize(c, r); },
  };
  return s;
}
function waitScreen(screen, pred, timeout = 5000) {
  return new Promise((resolve) => {
    if (pred(screen.text())) return resolve(true);
    const off = screen.on((s) => { if (pred(s.text())) { off(); clearTimeout(t); resolve(true); } });
    const t = setTimeout(() => { off(); resolve(false); }, timeout);
  });
}

// ---------------------------------------------------------------- delay proxy
function delayProxy(targetPort, oneWayMs) {
  const server = net.createServer((inbound) => {
    inbound.setNoDelay(true);
    const outbound = net.connect({ host: "127.0.0.1", port: targetPort });
    outbound.setNoDelay(true);
    const pipe = (from, to) => {
      // One ordered queue per direction: chunks leave in arrival order, each
      // no earlier than its own delay (jitter bunches them up, as Wi-Fi does).
      const q = []; let timer = null;
      const drain = () => {
        timer = null;
        while (q.length && q[0].at <= Date.now()) { const { chunk } = q.shift(); if (!to.destroyed) to.write(chunk); }
        if (q.length) timer = setTimeout(drain, Math.max(0, q[0].at - Date.now()));
      };
      from.on("data", (chunk) => {
        if (!oneWayMs && !JITTER) { to.write(chunk); return; }
        q.push({ chunk, at: Date.now() + oneWayMs + (JITTER ? Math.random() * JITTER : 0) });
        if (!timer) timer = setTimeout(drain, Math.max(0, q[0].at - Date.now()));
      });
      from.on("close", () => setTimeout(() => to.destroy(), oneWayMs + JITTER + 5));
      from.on("error", () => {});
    };
    pipe(inbound, outbound); pipe(outbound, inbound);
  });
  return new Promise((r) => server.listen(0, "127.0.0.1", () => r({ server, port: server.address().port })));
}

// ---------------------------------------------------------------- relay
// Mirrors cloud/src/server.ts attachWorker/attachClient forwarding and its
// WebSocketServer deflate settings.
async function startRelay() {
  const httpServer = http.createServer((req, res) => {
    res.setHeader("content-type", "application/json");
    if (req.url.startsWith("/api/rudder/home")) return res.end(JSON.stringify({ projects: [] }));
    if (req.url.startsWith("/api/rudder/sail")) return res.end(JSON.stringify({ sails: [{ id: "perf" }] }));
    res.statusCode = 404; res.end("{}");
  });
  const wss = new WebSocketServer({ noServer: true, perMessageDeflate: { threshold: 1024, zlibDeflateOptions: { level: 1 } } });
  const ch = { worker: null, clients: new Set(), controller: null, buffer: [], bytes: 0 };
  const isOpen = (w) => w && w.readyState === WebSocket.OPEN;
  httpServer.on("upgrade", (req, socket, head) => {
    try { socket.setNoDelay(true); } catch {}
    const url = new URL(req.url, "http://x");
    if (url.pathname.endsWith("/worker")) {
      wss.handleUpgrade(req, socket, head, (ws) => {
        ch.worker = ws; ch.buffer = []; ch.bytes = 0;
        for (const c of ch.clients) c.send(JSON.stringify({ type: "status", state: "worker-connected" }));
        ws.on("message", (data, isBinary) => {
          if (isBinary) {
            ch.buffer.push(data); ch.bytes += data.length;
            while (ch.bytes > 256 * 1024) ch.bytes -= ch.buffer.shift().length;
            for (const c of ch.clients) if (isOpen(c)) c.send(data, { binary: true });
          } else for (const c of ch.clients) if (isOpen(c)) c.send(data.toString("utf8"));
        });
      });
    } else if (url.pathname.endsWith("/attach")) {
      wss.handleUpgrade(req, socket, head, (ws) => {
        ch.clients.add(ws);
        const control = !isOpen(ch.controller);
        if (control) ch.controller = ws;
        ws.send(JSON.stringify({ type: "status", state: isOpen(ch.worker) ? "worker-connected" : "worker-waiting", control: control ? "active" : "read-only" }));
        if (ch.bytes) ws.send(Buffer.concat(ch.buffer), { binary: true });
        ws.on("message", (data, isBinary) => {
          if (ch.controller !== ws) return;
          if (isBinary) { if (isOpen(ch.worker)) ch.worker.send(data, { binary: true }); }
          else if (isOpen(ch.worker)) ch.worker.send(data.toString("utf8"));
        });
        ws.on("close", () => { ch.clients.delete(ws); if (ch.controller === ws) ch.controller = null; });
      });
    } else socket.destroy();
  });
  await new Promise((r) => httpServer.listen(0, "127.0.0.1", r));
  return { httpServer, wss, port: httpServer.address().port };
}

// ---------------------------------------------------------------- home emulator
async function startHome(repo, env, relayPort) {
  const homeScreen = makeScreen("dashboard");
  const inputLog = [];
  let term = null, ws = null;
  const io = createTerminalTransport({
    writeInput: (text) => { inputLog.push({ at: now(), text }); term?.write(text); },
    pauseOutput: () => term?.pause(), resumeOutput: () => term?.resume(),
    requestRepaint: () => { try { process.kill(term.pid, "SIGWINCH"); } catch {} },
    onError: (_e, s) => { try { s.terminate(); } catch {} },
  });
  term = pty.spawn(nativeBin, [], { name: "xterm-256color", cols: COLS, rows: ROWS, cwd: repo, env });
  term.onData((d) => { homeScreen.feed(d); io.writeOutput(d); });
  const connect = () => new Promise((resolve) => {
    ws = new WebSocket(`ws://127.0.0.1:${relayPort}/api/rudder/sail/perf/worker`, { handshakeTimeout: 10000 });
    ws.binaryType = "nodebuffer";
    ws.on("open", () => {
      try { ws._socket.setNoDelay(true); } catch {}
      ws.send(JSON.stringify({ type: "hello", cols: COLS, rows: ROWS }));
      io.attach(ws);
      try { process.kill(term.pid, "SIGWINCH"); } catch {}
      resolve();
    });
    ws.on("message", (data, isBinary) => {
      if (!io.isCurrent(ws)) return;
      if (isBinary) return io.writeInput(ws, data);
      const p = JSON.parse(data.toString("utf8"));
      if (p.type === "resize") { try { term.resize(p.cols, p.rows); homeScreen.resize(p.cols, p.rows); } catch {} }
      else if (p.type === "probe") io.sendControl({ type: "probe-reply", id: p.id ?? null });
      else if (p.type === "repaint") { try { process.kill(term.pid, "SIGWINCH"); } catch {} }
    });
  });
  return { homeScreen, inputLog, get term() { return term; }, connect, io, get ws() { return ws; } };
}

// ---------------------------------------------------------------- client
function startClient(relayProxyPort, home) {
  const clientScreen = makeScreen("laptop");
  const env = {
    ...process.env, HOME: home, TERM: "xterm-256color", COLORTERM: "truecolor",
    RUDDER_CLOUD_URL: `http://127.0.0.1:${relayProxyPort}`, RUDDER_CLOUD_TOKEN: "rdr_perf_local",
    RUDDER_PREDICT: opt.predict,
  };
  const p = pty.spawn(process.execPath, [path.join(REPO, "dist/index.js"), "cloud", "attach", "perf"], { name: "xterm-256color", cols: COLS, rows: ROWS, cwd: home, env });
  p.onData((d) => clientScreen.feed(d));
  // Answer terminal queries (DA, DSR, OSC 10/11 if supported) like a terminal.
  clientScreen.term.onData((d) => p.write(d));
  return { p, clientScreen };
}

// ---------------------------------------------------------------- typing
const stats = (xs) => {
  if (!xs.length) return "none";
  const s = [...xs].sort((a, b) => a - b);
  const q = (p) => s[Math.min(s.length - 1, Math.floor(p * s.length))];
  return `n=${s.length} p50=${q(0.5).toFixed(1)} p95=${q(0.95).toFixed(1)} max=${s.at(-1).toFixed(1)}`;
};
function visiblePrefix(screenText, typed) {
  let lo = 0, hi = typed.length;
  while (lo < hi) { const mid = (lo + hi + 1) >> 1; if (screenText.includes(typed.slice(0, mid))) lo = mid; else hi = mid - 1; }
  return lo;
}

// Watch a screen while typing: the visible typed prefix must never shrink
// between frames (a shrink is a character the user saw vanish).
function monotonic(screen, typedRef) {
  let best = 0; const events = [];
  const off = screen.on((s) => {
    if (s.inSync) return;
    const typed = typedRef();
    const v = visiblePrefix(s.text(), typed);
    if (v < best && best - v >= 1) events.push({ at: now(), best, v });
    best = Math.max(best, v);
  });
  return { stop: () => { off(); return events; } };
}

// ---------------------------------------------------------------- live probe
async function probe(workspaceId, samples = 60, gapMs = 500) {
  const { token, cloudUrl } = JSON.parse(fs.readFileSync(path.join(os.homedir(), ".rudder", "cloud.json"), "utf8"));
  const url = `${cloudUrl.replace(/^http/, "ws")}/api/rudder/workspace/${encodeURIComponent(workspaceId)}/attach?client=perf${crypto.randomBytes(8).toString("hex")}`;
  const t0 = now();
  const sock = new WebSocket(url, { headers: { authorization: `Bearer ${token}` }, perMessageDeflate: false, handshakeTimeout: 15000 });
  let control = null, pingAt = 0, replayAt = null, replayBytes = 0;
  const pings = [], probes = [], pairs = [], pending = new Map();
  sock.on("open", () => { try { sock._socket.setNoDelay(true); } catch {} });
  sock.on("pong", () => { if (pingAt) { pings.push(now() - pingAt); pingAt = 0; } });
  sock.on("message", (data, isBinary) => {
    if (isBinary) { if (replayAt === null) { replayAt = now() - t0; replayBytes = data.length; } return; }
    const m = JSON.parse(data.toString("utf8"));
    if (m.type === "status" && control === null) control = m.control ?? null;
    if (m.type === "status" && (m.control === "read-only" || m.state === "read-only")) { control = "read-only"; sock.close(1000); }
    if (m.type === "probe-reply") { const at = pending.get(m.id); if (at !== undefined) { probes.push(now() - at); pending.delete(m.id); } }
  });
  const closed = new Promise((r) => sock.on("close", r));
  sock.on("error", (e) => log("error", e.message));
  while (control === null && sock.readyState <= 1) await sleep(20);
  if (control !== "active") {
    log(`${workspaceId}: another terminal holds control (${control}); probes would be dropped, so leaving.`);
    sock.terminate();
    return;
  }
  await sleep(500);
  for (let i = 1; i <= samples && sock.readyState === WebSocket.OPEN; i++) {
    pingAt = now(); sock.ping();
    pending.set(i, now()); sock.send(JSON.stringify({ type: "probe", id: i }));
    await sleep(gapMs);
    pairs.push([pings.at(-1), probes.at(-1)]);
  }
  sock.close(1000, "perf-probe-done");
  await closed;
  log(`${workspaceId}: replay ${replayBytes}B, first frame ${replayAt?.toFixed(0) ?? "-"}ms after connect`);
  log(`  ws ping (this machine <-> control plane)   ${stats(pings)}`);
  log(`  probe (this machine <-> control <-> home)  ${stats(probes)}  lost=${pending.size}`);
  log(`  probe minus paired ping (control <-> home) ${stats(pairs.filter(([a, b]) => a != null && b != null).map(([a, b]) => b - a))}`);
}

if (process.argv[2] === "probe") {
  if (!process.argv[3]) throw new Error("usage: node scripts/cloud-perf.mjs probe <workspace-id> [samples]");
  await probe(process.argv[3], Number(process.argv[4] || 60));
  process.exit(0);
}

async function main() {
  const repo = await scratchRepo("rudder-perf-");
  const backends = await fakeBackends(repo);
  const bin = path.join(repo, "fake-bin");
  const streamer = path.join(bin, "claude-streamer");
  await fsp.writeFile(streamer, "#!/bin/bash\ni=0\nwhile :; do i=$((i+1)); printf '\\033[3%dmstream line %d lorem ipsum dolor sit amet consectetur adipiscing elit sed do\\033[0m\\n' $((i%7)) $i; sleep 0.01; done\n");
  await fsp.chmod(streamer, 0o755);
  for (const name of ["claude", "codex"]) { try { await fsp.symlink(streamer, path.join(bin, name)); } catch {} }
  const laptopHome = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-perf-laptop-"));
  const rudderHome = path.join(repo, ".tui-home");
  await fsp.mkdir(rudderHome, { recursive: true });
  const env = {
    ...process.env, PATH: `${bin}:${process.env.PATH}`,
    RUDDER_HOME: rudderHome, RUDDER_OFFLINE: "1", RUDDER_CLI: path.join(REPO, "dist/index.js"),
    RUDDER_CLAUDE_BIN: streamer, RUDDER_CODEX_BIN: streamer, NO_COLOR: "", COLORTERM: "truecolor",
    RUDDER_TASK_ROUTING: "local", TERM: "xterm-256color",
  };
  const results = { config: { CLIENT_MS, HOME_MS, JITTER, predict: opt.predict, direct: opt.direct }, scenarios: {} };
  const cleanup = [];
  try {
    let home, client, typeInto, clientScreen;
    if (opt.direct) {
      // Local baseline: keys straight into the dashboard PTY, no relay.
      const homeScreen = makeScreen("dashboard");
      const inputLog = [];
      const term = pty.spawn(nativeBin, [], { name: "xterm-256color", cols: COLS, rows: ROWS, cwd: repo, env });
      term.onData((d) => homeScreen.feed(d));
      homeScreen.term.onData((d) => term.write(d));
      home = { homeScreen, inputLog, term };
      clientScreen = homeScreen;
      typeInto = (s) => { inputLog.push({ at: now(), text: s }); term.write(s); };
      cleanup.push(() => { try { process.kill(term.pid, "SIGKILL"); } catch {} });
    } else {
      const relay = await startRelay();
      const homeProxy = await delayProxy(relay.port, HOME_MS);
      const clientProxy = await delayProxy(relay.port, CLIENT_MS);
      cleanup.push(() => { relay.httpServer.close(); homeProxy.server.close(); clientProxy.server.close(); });
      home = await startHome(repo, env, homeProxy.port);
      cleanup.push(() => { try { process.kill(home.term.pid, "SIGKILL"); } catch {} });
      await waitScreen(home.homeScreen, (t) => t.includes("describe a task"), 20000);
      await home.connect();
      if (opt.scenarios.split(",").includes("first")) {
        // First keys right after attach: start typing as soon as the client exists.
        client = startClient(clientProxy.port, laptopHome);
        clientScreen = client.clientScreen;
        typeInto = (s) => client.p.write(s);
      } else {
        client = startClient(clientProxy.port, laptopHome);
        clientScreen = client.clientScreen;
        typeInto = (s) => client.p.write(s);
      }
      cleanup.push(() => { try { client.p.kill("SIGKILL"); } catch {} });
    }
    const homeScreen = home.homeScreen;
    const inputText = () => home.inputLog.map((e) => e.text).join("");
    const clear = async () => { typeInto("\x15"); await sleep(400); home.inputLog.length = 0; };

    const scenarios = opt.scenarios.split(",");
    const t0 = now();
    if (scenarios.includes("first") && !opt.direct) {
      // Type the moment the laptop shows any remote frame.
      const word = "firstkeysafterattach";
      await waitScreen(clientScreen, (t) => t.includes("describe a task") || t.includes("Rudder"), 20000);
      const shownAt = now();
      for (const ch of word) { typeInto(ch); await sleep(15); }
      await sleep(3000);
      const dash = homeScreen.text().includes(word), cli = clientScreen.text().includes(word);
      results.scenarios.first = { attachToFrameMs: shownAt - t0, homeReceived: inputText(), dashboardShows: dash, laptopShows: cli };
      log("first:", JSON.stringify(results.scenarios.first));
      await clear();
    }
    await waitScreen(clientScreen, (t) => t.includes("describe a task"), 20000);
    await sleep(1500);

    if (scenarios.includes("slow")) {
      // One key every 250ms; time each echo on the laptop screen and in the dashboard.
      const word = "zqabcdefghijklmnopqrstuv";
      let typed = "";
      const lap = [], dash = [], arrive = [];
      for (const ch of word) {
        typed += ch;
        const target = typed, start = now(), before = home.inputLog.length;
        const pLap = waitScreen(clientScreen, (t) => t.includes(target), 5000).then((ok) => ok ? now() - start : NaN);
        const pDash = waitScreen(homeScreen, (t) => t.includes(target), 5000).then((ok) => ok ? now() - start : NaN);
        typeInto(ch);
        const [a, b] = await Promise.all([pLap, pDash]);
        const recv = home.inputLog[before];
        if (typed.length > 2) { lap.push(a); dash.push(b); if (recv) arrive.push(recv.at - start); }
        await sleep(250);
      }
      results.scenarios.slow = { laptopEcho: lap, dashboardEcho: dash, keyArrive: arrive };
      log(`slow: laptop-visible ${stats(lap)} | dashboard-drawn ${stats(dash)} | key reaches home PTY ${stats(arrive)}`);
      await clear();
    }

    async function fidelity(name, word, gapMs, { chunked = false } = {}) {
      let typed = "";
      const mon = monotonic(clientScreen, () => word);
      // Per character: when it was typed, and when the laptop first showed it.
      const typedAt = [], shownAt = [];
      const offLag = clientScreen.on((sc) => {
        if (sc.inSync) return;
        const v = visiblePrefix(sc.text(), word);
        for (let k = 0; k < v; k++) if (shownAt[k] === undefined) shownAt[k] = now();
      });
      const start = now();
      if (chunked) { const t = now(); for (let k = 0; k < word.length; k++) typedAt[k] = t; typeInto(word); typed = word; }
      else for (const ch of word) { typedAt[typed.length] = now(); typeInto(ch); typed += ch; if (gapMs) await sleep(gapMs); }
      const lastKeyAt = now();
      const okLap = await waitScreen(clientScreen, (t) => t.includes(word), 8000);
      const lapDone = now() - lastKeyAt;
      const okDash = await waitScreen(homeScreen, (t) => t.includes(word), 2000);
      await sleep(300);
      const events = mon.stop(); offLag();
      const allLags = typedAt.map((t, k) => Math.round((shownAt[k] ?? NaN) - t));
      const lags = allLags.filter((x) => Number.isFinite(x) && x >= 0).slice(4);
      const recv = inputText();
      const r = {
        typed: word.length, gapMs, homeReceivedExact: recv === word, homeReceived: recv === word ? undefined : recv,
        dashboardShows: okDash, laptopShows: okLap, laptopSettleMsAfterLastKey: Math.round(lapDone),
        perCharLag: stats(lags), firstLags: allLags.slice(0, 8).join(","), visibleShrinks: events.length, shrinkDetail: events.slice(0, 5).map((e) => `${e.best}->${e.v}`),
        dashboardLine: okDash ? undefined : homeScreen.text().split("\n").find((l) => l.includes(word.slice(0, 4))),
      };
      results.scenarios[name] = r;
      log(`${name}:`, JSON.stringify(r));
      await clear();
    }

    if (scenarios.includes("burst")) await fidelity("burst", "burstabcdefghijklmnopqrstuvwxyz0123456789", 0, { chunked: true });
    if (scenarios.includes("fast15")) await fidelity("fast15", "fifteenmsabcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH", 15);
    for (const g of [40, 80, 120, 160, 220]) if (scenarios.includes(`gap${g}`)) await fidelity(`gap${g}`, `gap${g}abcdefghijklmnopqrstuvw`, g);
    if (scenarios.includes("fast5")) await fidelity("fast5", "fivemsabcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH", 5);
    if (scenarios.includes("utf8")) await fidelity("utf8", "utfnaïveñüé日本語€→ßåæø", 15);
    if (scenarios.includes("glyphs")) {
      // Option-key glyphs one at a time: which ones never reach the input?
      const eaten = [];
      for (const g of "¡™£¢∂©√∫˙¬∆˚“‘åßƒ…≈ç") {
        typeInto("gl"); await sleep(150); typeInto(g); await sleep(150); typeInto("yph"); await sleep(400);
        if (!homeScreen.text().includes(`gl${g}yph`)) eaten.push(g);
        typeInto("\x15"); await sleep(150); typeInto("\x1b3"); await sleep(250); typeInto("\x15"); await sleep(150);
      }
      results.scenarios.glyphs = { eaten: eaten.join("") };
      log("glyphs eaten in the task input:", eaten.join(" "));
      await clear();
    }

    if (scenarios.includes("esc")) {
      // Legacy Esc followed quickly by a letter: does the letter survive?
      const res = {};
      for (const gap of [0, 5, 15, 50]) {
        typeInto("esctest"); await sleep(300);
        home.inputLog.length = 0;
        typeInto("\x1b"); if (gap) await sleep(gap); typeInto("x"); await sleep(gap || 1); typeInto("yz");
        await sleep(800);
        const scr = homeScreen.text();
        res[`gap${gap}`] = { homeReceived: JSON.stringify(inputText()), writes: home.inputLog.map((e) => JSON.stringify(e.text)).join(" | "), dashHas: ["esctestxyz", "xyz", "yz", "esctest"].find((w) => scr.includes(w)) ?? "none", lines: scr.split("\n").map((l) => l.trim()).filter((l) => /yz|esctest|describe a task/.test(l)).slice(0, 3) };
        typeInto("\x15"); await sleep(200); typeInto("\x1b[51;3u"); typeInto("\x1b3"); await sleep(300); typeInto("\x15"); await sleep(300);
      }
      results.scenarios.esc = res;
      log("esc:", JSON.stringify(res));
      await clear();
    }

    if (scenarios.includes("stream")) {
      // Start a streaming agent so the dashboard redraws constantly, refocus
      // the task input (Alt+3), then type at 15ms.
      typeInto("stream output for load"); await sleep(200); typeInto("\r");
      await waitScreen(homeScreen, (t) => /stream line \d+/.test(t), 30000);
      await sleep(1500);
      typeInto("\x1b3"); await sleep(500);
      home.inputLog.length = 0;
      let frames = 0; const offF = clientScreen.on(() => frames++);
      const f0 = now(); await sleep(2000); const fps = frames / ((now() - f0) / 1000); offF();
      log(`stream: laptop screen updates while agent streams ≈ ${fps.toFixed(1)}/s`);
      // Echo under load
      const word = "zqloadabcdefghijklmnop"; let typed = ""; const lap = [], dash = [];
      for (const ch of word) {
        typed += ch; const target = typed, start = now();
        const pLap = waitScreen(clientScreen, (t) => t.includes(target), 5000).then((ok) => ok ? now() - start : NaN);
        const pDash = waitScreen(homeScreen, (t) => t.includes(target), 5000).then((ok) => ok ? now() - start : NaN);
        typeInto(ch);
        const [a, b] = await Promise.all([pLap, pDash]);
        if (typed.length > 2) { lap.push(a); dash.push(b); }
        await sleep(250);
      }
      log(`stream: echo under load laptop ${stats(lap)} | dashboard ${stats(dash)}`);
      results.scenarios.streamEcho = { laptopEcho: lap, dashboardEcho: dash, updatesPerSec: fps };
      await clear();
      await fidelity("stream15", "underloadabcdefghijklmnopqrstuvwxyz0123456789ABC", 15);
      await fidelity("streamBurst", "loadburstabcdefghijklmnopqrstuvwxyz0123456789", 0, { chunked: true });
    }
  } finally {
    for (const fn of cleanup.reverse()) { try { await fn(); } catch {} }
    await sleep(300);
    if (opt.out) fs.writeFileSync(opt.out, JSON.stringify(results, null, 1));
    await removeScratch(repo).catch(() => {});
    await fsp.rm(laptopHome, { recursive: true, force: true }).catch(() => {});
  }
}
await main();
process.exit(0);
