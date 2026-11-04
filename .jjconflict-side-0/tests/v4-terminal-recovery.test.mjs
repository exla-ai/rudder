import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";
import ts from "typescript";
import xterm from "@xterm/headless";
import { WebSocket, WebSocketServer } from "ws";
import { once } from "node:events";

// Compile only the owned, otherwise unregistered modules. No dist/shared
// manifest changes are required to execute these local fault tests.
async function source(file) {
  const result = ts.transpileModule(fs.readFileSync(new URL(file, import.meta.url), "utf8"), {
    fileName: file, compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 }, reportDiagnostics: true,
  });
  assert.deepEqual(result.diagnostics, []);
  return import(`data:text/javascript;base64,${Buffer.from(result.outputText).toString("base64")}`);
}
const { TerminalScreenSession } = await source("../cloud/src/private-access/terminal-session.ts");
const { TerminalRecovery, terminalLatencyBounds } = await source("../cloud/src/private-access/terminal-recovery.ts");

class Clock {
  time = 0; id = 0; jobs = new Map();
  now = () => this.time;
  set = (fn, ms) => { const id = ++this.id; this.jobs.set(id, { at: this.time + ms, fn }); return id; };
  clear = (id) => this.jobs.delete(id);
  advance(ms) {
    const end = this.time + ms;
    for (;;) {
      const next = [...this.jobs].filter(([, job]) => job.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!next) break;
      this.time = next[1].at; this.jobs.delete(next[0]); next[1].fn();
    }
    this.time = end;
  }
}
function socket(hold = false) {
  return { readyState: 1, bufferedAmount: 0, frames: [], callbacks: [],
    send(data, cb) { this.frames.push(JSON.parse(data)); if (hold) this.callbacks.push(cb); else cb(); },
    close() { this.readyState = 3; },
    ack(error) { this.callbacks.shift()?.(error); },
  };
}
const screen = (text = "ready", scrollback = []) => ({ format: "vt100-ansi-v1", columns: 80, rows: 24,
  screen: `\x1bc\x1b[H${text}\x1b[0m`, scrollback });
function session(options = {}) {
  const clock = new Clock(), writes = [], traces = [];
  const stream = new TerminalScreenSession("owner:1/run:1", { timing: clock, writeInput: (...x) => writes.push(x), trace: (x) => traces.push(x), ...options });
  return { stream, clock, writes, traces };
}

test("owned terminal modules pass strict type checks including browser and Cloud adapters", () => {
  const files = ["cloud/src/private-access/terminal-session.ts", "src/board/ui/terminal-transport/recovery.ts"];
  const program = ts.createProgram(files, { noEmit: true, strict: true, target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.NodeNext, moduleResolution: ts.ModuleResolutionKind.NodeNext, skipLibCheck: true,
    lib: ["lib.es2022.d.ts", "lib.dom.d.ts"], types: ["node"] });
  const diagnostics = ts.getPreEmitDiagnostics(program);
  assert.equal(diagnostics.length, 0, ts.formatDiagnosticsWithColorAndContext(diagnostics, {
    getCanonicalFileName: (file) => file, getCurrentDirectory: () => process.cwd(), getNewLine: () => "\n",
  }));
});

test("coalescing retains complete screen, history, geometry and a sequenced resume cursor", () => {
  const { stream, clock } = session(); const ws = socket(); stream.attach("laptop", ws);
  for (let i = 0; i < 100; i++) stream.publish(screen(`frame ${i}`, ["prior line"]));
  assert.equal(ws.frames.length, 0);
  clock.advance(4);
  assert.equal(ws.frames.length, 1);
  assert.deepEqual(ws.frames[0], { version: 1, type: "terminal-screen", generation: "owner:1/run:1", sequence: 100,
    reset: true, gap: false, screen: screen("frame 99", ["prior line"]) });
  assert.deepEqual(stream.cursor, { generation: "owner:1/run:1", sequence: 100 });
  stream.dispose();
});

test("continuous output keeps one in-flight frame per viewer; a hung observer cannot block controller", () => {
  const { stream, clock } = session({ maxScreenBytes: 128, stalledMs: 50 });
  const fast = socket(), hung = socket(true);
  const controller = stream.attach("controller", fast); stream.attach("observer", hung);
  stream.publish(screen("first")); clock.advance(4);
  stream.acknowledge(controller, stream.cursor);
  for (let i = 0; i < 5000; i++) stream.publish(screen(String(i)));
  clock.advance(4);
  assert.equal(hung.frames.length, 1);
  assert.equal(fast.frames.length, 2);
  assert.equal(fast.frames[1].sequence, 5001);
  assert.equal(fast.frames[1].gap, true);
  assert.equal(fast.frames[1].reset, true);
  stream.acknowledge(controller, stream.cursor);
  clock.advance(50);
  assert.equal(hung.readyState, 3); assert.equal(stream.viewCount, 1);
  assert.equal(stream.controller, "controller");
  assert.throws(() => stream.publish(screen("x".repeat(129))), /byte budget/);
  assert.throws(() => stream.publish(null), /invalid terminal screen/);
  stream.dispose(); assert.equal(clock.jobs.size, 0);
});

test("successful network sends still wait for exact paint acknowledgment and coalesce a slow browser", () => {
  const { stream, clock } = session({ stalledMs: 100 });
  const ws = socket(), view = stream.attach("browser", ws);
  stream.publish(screen("first")); clock.advance(4);
  for (let i = 0; i < 1000; i++) stream.publish(screen(String(i)));
  clock.advance(4); assert.equal(ws.frames.length, 1);
  assert.equal(stream.acknowledge(view, { generation: "old", sequence: 1 }), false);
  assert.equal(stream.acknowledge(view, { generation: stream.generation, sequence: 2 }), false);
  assert.equal(stream.acknowledge(view, ws.frames[0]), true);
  assert.equal(ws.frames.length, 2); assert.equal(ws.frames[1].sequence, 1001);
  assert.equal(ws.frames[1].gap, true);
  clock.advance(100); assert.equal(ws.readyState, 3); assert.equal(stream.viewCount, 0);
  stream.dispose();
});

test("one viewer can request a cursor reset without disturbing another viewer or accepting stale callbacks", () => {
  const { stream } = session();
  const a = socket(), b = socket(); const first = stream.attach("a", a); const second = stream.attach("b", b);
  stream.publish(screen("canonical")); stream.flush();
  stream.acknowledge(first, a.frames[0]); stream.acknowledge(second, b.frames[0]);
  assert.equal(stream.requestSnapshot(second, { generation: "old", sequence: 800 }), true);
  assert.equal(a.frames.length, 1); assert.equal(b.frames.length, 2);
  assert.equal(b.frames[1].reset, true); assert.equal(b.frames[1].gap, true);
  stream.detach(second); assert.equal(stream.requestSnapshot(second), false); stream.dispose();
});

test("socket bufferedAmount is bounded independently and recovers to a fresh snapshot", () => {
  const { stream, clock } = session({ highWaterBytes: 10, stalledMs: 100 }); const slow = socket();
  slow.bufferedAmount = 10; stream.attach("slow", slow, { generation: "old", sequence: 900 });
  stream.publish(screen("complete")); clock.advance(4); assert.equal(slow.frames.length, 0);
  slow.bufferedAmount = 0; clock.advance(25);
  assert.equal(slow.frames[0].reset, true); assert.equal(slow.frames[0].gap, true);
  assert.equal(slow.frames[0].screen.screen, screen("complete").screen);
  stream.dispose();
});

test("exit follows final snapshot even with held asynchronous sends and skipped intermediate screens", () => {
  const { stream } = session(); const ws = socket(true); const view = stream.attach("laptop", ws);
  stream.publish(screen("before")); stream.flush();
  stream.publish(screen("last")); stream.exit(7);
  assert.equal(ws.frames.length, 1);
  ws.ack(); assert.equal(ws.frames.length, 1);
  stream.acknowledge(view, ws.frames[0]); assert.equal(ws.frames[1].screen.screen, screen("last").screen);
  ws.ack(); assert.equal(ws.frames.length, 2);
  stream.acknowledge(view, ws.frames[1]); assert.equal(ws.frames[2].type, "terminal-exit"); assert.equal(ws.frames[2].code, 7);
  assert.equal(ws.frames[2].sequence, 2); ws.ack(); stream.dispose();
});

test("replacement connection and owner fence stale callbacks and never replay old Ctrl+C or partial input", () => {
  const { stream, writes } = session(); const old = socket(true); const a = stream.attach("same-client", old);
  stream.publish(screen()); stream.flush();
  assert.equal(stream.input(a, stream.generation, Uint8Array.from([0xf0, 0x9f]), "old-partial"), true);
  const fresh = socket(); const b = stream.attach("same-client", fresh, stream.cursor);
  old.ack(new Error("late send failure")); assert.equal(fresh.readyState, 1);
  assert.equal(stream.input(a, stream.generation, Buffer.from("\x03"), "stale-interrupt"), false);
  assert.equal(stream.input(b, stream.generation, Uint8Array.from([0x91, 0x8b]), "orphan"), true);
  assert.equal(writes.length, 0);
  stream.input(b, stream.generation, Buffer.from("👋\x1b[200~paste\x1b[201~"), "fresh-paste");
  assert.deepEqual(writes[0], ["👋\x1b[200~paste\x1b[201~", "owner:1/run:1", "fresh-paste"]);
  stream.replaceOwner("owner:2/run:1");
  assert.equal(stream.input(b, "owner:1/run:1", Buffer.from("\x03"), "old-owner"), false);
  assert.equal(stream.viewCount, 0); assert.equal(writes.length, 1); stream.dispose();
});

test("only controller may input or resize; promotion resets its decoder without replay", () => {
  const resizes = []; const { stream, writes } = session({ resize: (...x) => resizes.push(x) });
  const a = stream.attach("a", socket()), b = stream.attach("b", socket());
  assert.equal(stream.input(b, stream.generation, Buffer.from("\x03"), "spectator"), false);
  assert.equal(stream.resize(b, stream.generation, 120, 40), false);
  assert.equal(stream.resize(a, stream.generation, 120, 40), true);
  stream.input(a, stream.generation, Uint8Array.from([0xe2]), "half");
  stream.detach(a); assert.equal(stream.controller, "b");
  stream.input(b, stream.generation, Buffer.from("B"), "promoted");
  assert.deepEqual(writes.map((x) => x[0]), ["B"]); assert.deepEqual(resizes, [[120, 40, stream.generation]]);
  stream.dispose();
});

test("browser fences connection, explicit snapshot gaps, fresh UTF8 decoder, exit cursor and privileged input", () => {
  const restored = [], requests = [], sends = [], legacy = [], trace = [];
  const view = new TerminalRecovery({ acknowledge: () => {}, restore: (x, paint) => { restored.push(x); paint.painted(); }, requestSnapshot: (x) => requests.push(x),
    sendInput: (...x) => { sends.push(x); return true; }, writeLegacy: (x) => legacy.push(x), trace: (x) => trace.push(x), now: () => 42 });
  const a = view.connect("owner", true);
  const frame = (sequence, reset = true, gap = false) => ({ version: 1, type: "terminal-screen", generation: "owner", sequence, reset, gap, screen: screen("authoritative") });
  assert.equal(view.accept(a, null), false);
  assert.equal(view.accept(a, frame(1, false)), false); assert.equal(requests.length, 1);
  assert.equal(view.accept(a, frame(1)), true);
  view.legacyOutput(a, Uint8Array.from([0xe2]));
  view.disconnect(a); assert.equal(view.input(a, Buffer.from("\x03"), "offline"), false);
  const b = view.connect("owner", false);
  assert.equal(view.accept(a, frame(2)), false); view.disconnect(a);
  assert.equal(view.accept(b, frame(3, false)), false);
  assert.equal(view.accept(b, frame(3, true, true)), true);
  view.legacyOutput(b, Buffer.from("B")); assert.deepEqual(legacy, ["B"]);
  assert.equal(view.input(b, Buffer.from("\x03"), "spectator"), false);
  view.setController(b, true); assert.equal(view.input(b, Buffer.from("a"), "key:1"), true);
  view.predictionPainted("key:1"); view.predictionRolledBack("key:1");
  assert.equal(view.acceptExit(b, { version: 1, type: "terminal-exit", generation: "owner", sequence: 2, code: 0 }), false);
  assert.equal(view.acceptExit(b, { version: 1, type: "terminal-exit", generation: "owner", sequence: 3, code: 0 }), true);
  assert.equal(sends.length, 1); assert.equal(restored.length, 2);
  assert.ok(trace.every((x) => x.clock === "process-monotonic" && x.localMs === 42));
  assert.ok(trace.some((x) => x.phase === "predictive-paint")); assert.ok(trace.some((x) => x.phase === "authoritative-paint"));
});

test("asynchronous painting is generation fenced and exit waits for final authoritative paint", () => {
  const paints = [], exits = [], traces = [];
  const view = new TerminalRecovery({ acknowledge: () => {}, restore: (state, paint) => paints.push(paint), requestSnapshot: () => {},
    sendInput: () => true, exit: (code) => exits.push(code), trace: (event) => traces.push(event) });
  const frame = { version: 1, type: "terminal-screen", generation: "owner", sequence: 1, reset: true, gap: false, screen: screen() };
  const a = view.connect("owner", true); view.accept(a, frame);
  const b = view.connect("owner", true); view.accept(b, frame);
  assert.equal(paints[0].isCurrent(), false); paints[0].painted();
  assert.equal(traces.length, 0);
  assert.equal(view.acceptExit(b, { version: 1, type: "terminal-exit", generation: "owner", sequence: 1, code: 3 }), true);
  assert.deepEqual(exits, []);
  paints[1].painted(); assert.deepEqual(exits, [3]);
  assert.equal(traces[0].phase, "authoritative-paint");
});

test("real headless terminal preserves split UTF8/escapes and snapshot repaint state after disconnect", async () => {
  const authoritative = new xterm.Terminal({ cols: 30, rows: 3, scrollback: 20, allowProposedApi: true });
  const viewer = new xterm.Terminal({ cols: 30, rows: 3, scrollback: 20, allowProposedApi: true });
  const write = (term, data) => new Promise((resolve) => term.write(data, resolve));
  try {
    const bytes = Buffer.from("history\r\n\x1b[31m👋 日本語\x1b[0m\r\nline3\r\nlast");
    for (const byte of bytes) await write(authoritative, Uint8Array.of(byte));
    const buffer = authoritative.buffer.active;
    const rows = Array.from({ length: 3 }, (_, y) => buffer.getLine(buffer.baseY + y).translateToString(true));
    const snapshot = { format: "vt100-ansi-v1", columns: 30, rows: 3,
      screen: "\x1bc" + rows.map((line, y) => `\x1b[${y + 1};1H${line}`).join("") + `\x1b[${buffer.cursorY + 1};${buffer.cursorX + 1}H`,
      scrollback: Array.from({ length: buffer.baseY }, (_, y) => buffer.getLine(y).translateToString(true)) };
    const { stream } = session(); const ws = socket(); stream.attach("reconnected", ws, { generation: "stale", sequence: 1 });
    stream.publish(snapshot); stream.flush(); await write(viewer, ws.frames[0].screen.screen);
    assert.deepEqual(Array.from({ length: 3 }, (_, y) => viewer.buffer.active.getLine(y).translateToString(true)), rows);
    assert.equal(viewer.buffer.active.cursorX, buffer.cursorX); assert.equal(viewer.buffer.active.cursorY, buffer.cursorY);
    assert.deepEqual(ws.frames[0].screen.scrollback, ["history"]);
    stream.dispose();
  } finally { authoritative.dispose(); viewer.dispose(); }
});

test("actual WebSocket reconnect delivers full screen instead of old byte tail", { timeout: 5000 }, async () => {
  const server = new WebSocketServer({ host: "127.0.0.1", port: 0 }); await once(server, "listening");
  const { stream } = session(); const peers = [];
  server.on("connection", (ws) => { peers.push(ws); stream.attach("laptop", ws, { generation: "before-sleep", sequence: 11 }); });
  const address = `ws://127.0.0.1:${server.address().port}`;
  const first = new WebSocket(address); await once(first, "open");
  stream.publish(screen("before sleep")); const initial = once(first, "message"); stream.flush(); await initial;
  const second = new WebSocket(address); const received = once(second, "message"); await once(second, "open");
  const [message] = await received;
  assert.equal(JSON.parse(message).reset, true); assert.equal(JSON.parse(message).gap, true);
  assert.equal(JSON.parse(message).screen.screen, screen("before sleep").screen);
  stream.dispose(); first.terminate(); second.terminate(); for (const peer of peers) peer.terminate();
  await new Promise((resolve) => server.close(resolve));
});

test("disposed sessions fence all old handles and cannot be reattached to a retired owner", () => {
  const { stream, writes, clock } = session(); const ws = socket(true);
  const view = stream.attach("controller", ws); stream.publish(screen()); stream.flush();
  stream.dispose(); ws.ack(new Error("late"));
  assert.equal(stream.input(view, stream.generation, Buffer.from("\x03"), "stale"), false);
  assert.equal(stream.acknowledge(view, stream.cursor), false);
  assert.throws(() => stream.attach("new", socket()), /disposed/);
  assert.throws(() => stream.publish(screen()), /disposed/);
  assert.throws(() => stream.replaceOwner("next"), /disposed/);
  assert.equal(clock.jobs.size, 0); assert.deepEqual(writes, []);
});

test("local trace percentiles reject cross-clock/negative samples", () => {
  assert.deepEqual(terminalLatencyBounds(Array.from({ length: 100 }, (_, i) => i + 1)), { count: 100, p95: 95, p99: 99, max: 100 });
  assert.equal(terminalLatencyBounds([]), null);
  assert.throws(() => terminalLatencyBounds([-1]), /local monotonic clock/);
});
