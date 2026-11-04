import assert from "node:assert/strict";
import { once } from "node:events";
import { createRequire } from "node:module";
import { performance } from "node:perf_hooks";
import test from "node:test";
import { WebSocket, WebSocketServer } from "ws";

import { createTerminalTransport } from "../cloud/worker/terminal-transport.mjs";

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitFor(predicate, timeout = 3000) {
  const deadline = performance.now() + timeout;
  while (!predicate()) {
    if (performance.now() > deadline) throw new Error("terminal transport timed out");
    await sleep(5);
  }
}

function fakeSocket({ holdCallbacks = false, failSend = false } = {}) {
  return {
    readyState: 1,
    bufferedAmount: 0,
    frames: [],
    callbacks: [],
    send(data, { binary }, callback) {
      if (failSend) throw new Error("send failed");
      this.frames.push({ data: Buffer.from(data), binary });
      if (holdCallbacks) this.callbacks.push(callback);
      else callback?.();
    },
  };
}

function binaryOutput(socket) {
  return Buffer.concat(socket.frames.filter((frame) => frame.binary).map((frame) => frame.data));
}

test("PTY bursts are coalesced without changing Unicode or terminal control bytes", (t) => {
  const io = createTerminalTransport();
  t.after(() => io.dispose());
  const socket = fakeSocket();
  io.attach(socket);
  const parts = ["\x1b[?2026h", "\x1b]8;;https://example.test\x1b\\", "👩🏽‍💻 é 日本語", "\x1b]8;;\x1b\\", "\x1b[?2026l"];
  for (let i = 0; i < 100; i += 1) for (const part of parts) io.writeOutput(part);
  assert.equal(socket.frames.length, 0, "small output waits for the short batching window");
  io.flush();
  assert.equal(socket.frames.length, 1, "500 PTY writes use one WebSocket frame");
  assert.deepEqual(binaryOutput(socket), Buffer.from(parts.join("").repeat(100)));
});

test("frame targets never cut an original UTF-8 PTY chunk", (t) => {
  const io = createTerminalTransport({ frameBytes: 5 });
  t.after(() => io.dispose());
  const socket = fakeSocket();
  io.attach(socket);
  io.writeOutput("👩🏽‍💻");
  io.writeOutput("日本語");
  io.flush();
  assert.deepEqual(socket.frames.map((frame) => frame.data.toString("utf8")), ["👩🏽‍💻", "日本語"]);
});

test("initial buffered output is preserved, and overflow requests a clean frame instead of a corrupt byte tail", (t) => {
  const initial = createTerminalTransport();
  t.after(() => initial.dispose());
  initial.writeOutput("\x1b[31mboot 👋\x1b[0m");
  const first = fakeSocket();
  initial.attach(first);
  assert.equal(binaryOutput(first).toString("utf8"), "\x1b[31mboot 👋\x1b[0m");

  let repaints = 0;
  const io = createTerminalTransport({
    maxQueuedBytes: 32,
    requestRepaint: () => { repaints += 1; io.writeOutput("FULL FRAME 👋"); },
  });
  t.after(() => io.dispose());
  io.writeOutput("\x1b]8;;https://example.test/".repeat(50));
  io.writeOutput("unusable continuation 👋");
  const recovered = fakeSocket();
  io.attach(recovered);
  const text = binaryOutput(recovered).toString("utf8");
  assert.equal(repaints, 1);
  assert.ok(text.startsWith("\x18\x1b\\"), "cancel any incomplete terminal sequence first");
  assert.ok(text.endsWith("FULL FRAME 👋"));
  assert.ok(!text.includes("unusable continuation"));
  assert.ok(!text.includes("�"));
});

test("slow socket backpressure pauses the PTY and drains output before the exit control", (t) => {
  const states = [];
  const io = createTerminalTransport({
    highWaterBytes: 32,
    lowWaterBytes: 8,
    pauseOutput: () => states.push("paused"),
    resumeOutput: () => states.push("resumed"),
  });
  t.after(() => io.dispose());
  const socket = fakeSocket();
  io.attach(socket);
  socket.bufferedAmount = 32;
  io.writeOutput("last terminal line 👋\r\n");
  let completed = false;
  io.sendControl({ type: "exit", code: 0 }, () => { completed = true; });
  assert.deepEqual(states, ["paused"]);
  assert.equal(socket.frames.length, 0);
  assert.equal(completed, false);
  socket.bufferedAmount = 0;
  io.flush();
  assert.deepEqual(states, ["paused", "resumed"]);
  assert.equal(socket.frames[0].data.toString("utf8"), "last terminal line 👋\r\n");
  assert.deepEqual(JSON.parse(socket.frames[1].data), { type: "exit", code: 0 });
  assert.equal(socket.frames[1].binary, false);
  assert.equal(completed, true);
});

test("small control replies flush a prior batch immediately and retain output/control order", (t) => {
  const io = createTerminalTransport();
  t.after(() => io.dispose());
  const socket = fakeSocket();
  io.attach(socket);
  io.writeOutput("before");
  io.sendControl({ type: "probe-reply", id: 17 });
  io.writeOutput("after");
  io.flush();
  assert.deepEqual(socket.frames.map((frame) => [frame.binary, frame.data.toString()]), [
    [true, "before"],
    [false, '{"type":"probe-reply","id":17}'],
    [true, "after"],
  ]);
});

test("a paused reader resumes when socket pressure clears, without another PTY write", async (t) => {
  const states = [];
  const io = createTerminalTransport({
    highWaterBytes: 32,
    lowWaterBytes: 8,
    pauseOutput: () => states.push("paused"),
    resumeOutput: () => states.push("resumed"),
  });
  t.after(() => io.dispose());
  const socket = fakeSocket();
  socket.bufferedAmount = 32;
  io.attach(socket);
  io.writeOutput("buffered while the socket drains");
  await waitFor(() => states[0] === "paused");
  socket.bufferedAmount = 0;
  await waitFor(() => states.at(-1) === "resumed");
  assert.equal(binaryOutput(socket).toString(), "buffered while the socket drains");
});

test("connected overload never replays an arbitrary byte suffix", (t) => {
  let repaints = 0;
  const io = createTerminalTransport({ maxQueuedBytes: 64, requestRepaint: () => { repaints += 1; } });
  t.after(() => io.dispose());
  const socket = fakeSocket();
  io.attach(socket);
  io.writeOutput("\x1b[?2026h");
  io.writeOutput("👋".repeat(1000));
  io.flush();
  assert.equal(repaints, 1);
  assert.equal(binaryOutput(socket).toString(), "\x18\x1b\\\x1b[0m\x1b[?2026l");
});

test("a slow receiver cannot accumulate an unbounded control-message queue", (t) => {
  const io = createTerminalTransport({ maxQueuedBytes: 64, frameBytes: 64, highWaterBytes: 32 });
  t.after(() => io.dispose());
  const socket = fakeSocket();
  socket.bufferedAmount = 32;
  io.attach(socket);
  let accepted = 0;
  for (let id = 0; id < 1000; id += 1) if (io.sendControl({ type: "probe-reply", id })) accepted += 1;
  assert.ok(accepted > 0 && accepted < 5);
  socket.bufferedAmount = 0;
  io.flush();
  assert.equal(socket.frames.length, accepted);
});

test("UTF-8 input is streamed immediately, preserves split key sequences and never joins connections", (t) => {
  const written = [];
  const io = createTerminalTransport({ writeInput: (text) => written.push(text) });
  t.after(() => io.dispose());
  const old = fakeSocket();
  io.attach(old);
  io.writeInput(old, Buffer.from("a"));
  assert.deepEqual(written, ["a"], "input does not wait for the output batching timer");
  const euro = Buffer.from("€");
  io.writeInput(old, euro.subarray(0, 1));
  io.writeInput(old, euro.subarray(1));
  io.writeInput(old, Buffer.from("\x1b["));
  io.writeInput(old, Buffer.from("1;5D"));
  assert.equal(written.join(""), "a€\x1b[1;5D");
  io.writeInput(old, euro.subarray(0, 1));
  io.detach(old);
  const current = fakeSocket();
  io.attach(current);
  assert.equal(io.writeInput(old, Buffer.from("stale\x03")), false);
  io.writeInput(current, Buffer.concat([euro.subarray(1), Buffer.from("new 👋")]));
  assert.equal(written.join(""), "a€\x1b[1;5Dnew 👋");
});

test("old closes and late send failures cannot reset the replacement socket", (t) => {
  const errors = [];
  const inputs = [];
  const io = createTerminalTransport({ writeInput: (text) => inputs.push(text), onError: (error) => errors.push(error) });
  t.after(() => io.dispose());
  const old = fakeSocket({ holdCallbacks: true });
  io.attach(old);
  io.writeOutput("old output");
  io.flush();
  const current = fakeSocket();
  io.attach(current);
  assert.equal(io.detach(old), false);
  old.callbacks[0](new Error("late socket error"));
  assert.equal(io.isCurrent(current), true);
  assert.equal(errors.length, 0);
  io.writeInput(current, Buffer.from("current"));
  assert.deepEqual(inputs, ["current"]);
});

test("send exceptions recover by repaint, release PTY flow control and do not escape the event handler", (t) => {
  const errors = [];
  let repaints = 0;
  const io = createTerminalTransport({ onError: (error) => errors.push(error.message), requestRepaint: () => { repaints += 1; } });
  t.after(() => io.dispose());
  io.attach(fakeSocket({ failSend: true }));
  io.writeOutput("last write");
  assert.doesNotThrow(() => io.flush());
  assert.deepEqual(errors, ["send failed"]);
  const current = fakeSocket();
  io.attach(current);
  assert.equal(repaints, 1);
  io.writeOutput("new frame");
  io.flush();
  assert.ok(binaryOutput(current).toString().endsWith("new frame"));
});

test("disconnect releases a paused reader and disposal rejects stale keys", (t) => {
  const states = [];
  const io = createTerminalTransport({
    writeInput: () => assert.fail("disposed input reached the PTY"),
    pauseOutput: () => states.push("paused"),
    resumeOutput: () => states.push("resumed"),
    highWaterBytes: 32,
  });
  t.after(() => io.dispose());
  const socket = fakeSocket();
  io.attach(socket);
  socket.bufferedAmount = 32;
  io.writeOutput("x");
  io.flush();
  io.detach(socket);
  assert.deepEqual(states, ["paused", "resumed"]);
  io.dispose();
  assert.equal(io.writeInput(socket, Buffer.from("ignored")), false);
  assert.equal(io.attach(fakeSocket()), false);
});

test("real WebSockets batch a burst while keys and the final output/control arrive intact", async (t) => {
  const server = new WebSocketServer({ host: "127.0.0.1", port: 0 });
  await once(server, "listening");
  const peerPromise = once(server, "connection");
  const client = new WebSocket(`ws://127.0.0.1:${server.address().port}`);
  const [peer] = await peerPromise;
  if (client.readyState !== WebSocket.OPEN) await once(client, "open");
  const inputs = [];
  const io = createTerminalTransport({ writeInput: (text) => inputs.push(text) });
  t.after(() => { io.dispose(); client.terminate(); peer.terminate(); server.close(); });
  peer.on("message", (data, binary) => { if (binary) io.writeInput(peer, data); });
  io.attach(peer);
  const frames = [];
  client.on("message", (data, binary) => frames.push({ data: Buffer.from(data), binary }));
  const start = performance.now();
  for (let i = 0; i < 1000; i += 1) io.writeOutput("👋\x1b[32m.");
  await waitFor(() => frames.length > 0);
  const elapsed = performance.now() - start;
  assert.equal(binaryOutput({ frames }).toString(), "👋\x1b[32m.".repeat(1000));
  assert.equal(frames.length, 1);
  client.send(Buffer.from("€").subarray(0, 1));
  client.send(Buffer.from("€").subarray(1));
  await waitFor(() => inputs.join("") === "€");
  io.writeOutput("FINAL 👋\r\n");
  io.sendControl({ type: "exit", code: 0 });
  await waitFor(() => frames.some((frame) => !frame.binary));
  assert.equal(frames.at(-2).data.toString(), "FINAL 👋\r\n");
  assert.equal(JSON.parse(frames.at(-1).data).type, "exit");
  t.diagnostic(`1,000 writes -> ${frames.length - 2} burst frame; first output observed after ${elapsed.toFixed(1)}ms on loopback`);
});

const requireWorker = createRequire(new URL("../cloud/worker/package.json", import.meta.url));
let pty;
try { pty = requireWorker("node-pty"); } catch { /* optional worker dependencies */ }

test("a real PTY preserves Unicode and control input through the WebSocket bridge", { skip: !pty && "worker node-pty dependencies unavailable" }, async (t) => {
  const term = pty.spawn(process.execPath, ["-e", 'process.stdin.setRawMode(true);process.stdin.on("data",d=>process.stdout.write(d));process.stdout.write("PTY-READY\\n")'], {
    name: "xterm-256color", cols: 100, rows: 30, env: process.env,
  });
  let io;
  const earlyOutput = [];
  term.onData((data) => { if (io) io.writeOutput(data); else earlyOutput.push(data); });
  t.after(() => { io?.dispose(); term.kill(); });
  const server = new WebSocketServer({ host: "127.0.0.1", port: 0 });
  t.after(() => { for (const socket of server.clients) socket.terminate(); server.close(); });
  await once(server, "listening");
  const peerPromise = once(server, "connection");
  const client = new WebSocket(`ws://127.0.0.1:${server.address().port}`);
  const [peer] = await peerPromise;
  if (client.readyState !== WebSocket.OPEN) await once(client, "open");
  io = createTerminalTransport({ writeInput: (text) => term.write(text), pauseOutput: () => term.pause(), resumeOutput: () => term.resume() });
  const frames = [];
  t.after(() => client.terminate());
  client.on("message", (data, binary) => { if (binary) frames.push(Buffer.from(data)); });
  peer.on("message", (data, binary) => { if (binary) io.writeInput(peer, data); });
  for (const data of earlyOutput) io.writeOutput(data);
  io.attach(peer);
  await waitFor(() => Buffer.concat(frames).includes(Buffer.from("PTY-READY")));
  frames.length = 0;
  const input = Buffer.from("日本語 👩🏽‍💻 é\x1b[1;5D END");
  for (let offset = 0; offset < input.length; offset += 1) client.send(input.subarray(offset, offset + 1));
  await waitFor(() => Buffer.concat(frames).includes(Buffer.from(" END")));
  assert.deepEqual(Buffer.concat(frames), input);
});
