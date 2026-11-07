// Keys typed into a Rudder Cloud dashboard, as the dashboard's key parser
// reads them. Every client's keys reach the dashboard through one place on the
// machine: the terminal transport's writeInput (home dashboards and sails
// alike). A client may split its keys anywhere (`rudder cloud` before 4.2.10
// sent each terminal read as it came), and each message is its own PTY write,
// so the dashboard can read every message on its own.

import assert from "node:assert/strict";
import test from "node:test";

import { createTerminalTransport } from "../cloud/worker/terminal-transport.mjs";

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// --- crossterm 0.29's Unix key parser, reduced to what decides typed text ---
// src/event/source/unix/mio.rs reads what the PTY has and calls the parser per
// byte, where "more input" means only "more bytes in THIS read".
// src/event/sys/unix/parse.rs: Ok(None) waits, Err clears the buffer, Ok(Some)
// is an event. Keys other than characters are kept as their raw bytes.
const WAIT = "wait";
const ERROR = "error";
const ascii = (byte) => String.fromCharCode(byte);
const raw = (buffer) => ({ seq: buffer.toString("latin1") });

function parseUtf8(buffer) {
  try {
    return { char: new TextDecoder("utf-8", { fatal: true }).decode(buffer) };
  } catch {
    const lead = buffer[0];
    const required = lead < 0x80 ? 1 : lead >= 0xc0 && lead <= 0xdf ? 2 : lead >= 0xe0 && lead <= 0xef ? 3 : lead >= 0xf0 && lead <= 0xf7 ? 4 : 0;
    if (required === 0 || buffer.subarray(1).some((byte) => (byte & 0xc0) !== 0x80)) return ERROR;
    return buffer.length < required ? WAIT : ERROR;
  }
}

function parseCsi(buffer) {
  if (buffer.length === 2) return WAIT;
  const last = buffer[buffer.length - 1];
  switch (ascii(buffer[2])) {
    case "[":
      if (buffer.length === 3) return WAIT;
      return "ABCDE".includes(ascii(buffer[3])) ? raw(buffer) : ERROR;
    case "D": case "C": case "A": case "B": case "H": case "F": case "Z":
    case "I": case "O": case "P": case "Q": case "S":
      return raw(buffer);
    case "M":
      return buffer.length < 6 ? WAIT : raw(buffer);
    case "<":
      return last === 0x4d || last === 0x6d ? raw(buffer) : WAIT;
    case ";":
      return ERROR;
    case "?":
      // Only "u" (keyboard flags) or "c" (device attributes) ends one; until
      // then every key typed is taken for part of it.
      return last === 0x75 || last === 0x63 ? { internal: buffer.toString("latin1") } : WAIT;
  }
  if (buffer[2] >= 0x30 && buffer[2] <= 0x39) {
    if (buffer.length === 3 || last < 64 || last > 126) return WAIT;
    if (buffer.subarray(0, 6).toString("latin1") === "\x1b[200~") {
      return buffer.subarray(-6).toString("latin1") === "\x1b[201~" ? { paste: buffer.subarray(6, -6).toString("utf8") } : WAIT;
    }
    return last === 0x52 ? { internal: buffer.toString("latin1") } : raw(buffer);
  }
  return ERROR;
}

function parseEvent(buffer, more) {
  const first = buffer[0];
  if (first === 0x1b) {
    if (buffer.length === 1) return more ? WAIT : { seq: "\x1b" };
    if (buffer[1] === 0x4f) {
      if (buffer.length === 2) return WAIT;
      return "DCABHFPQRS".includes(ascii(buffer[2])) ? raw(buffer) : ERROR;
    }
    if (buffer[1] === 0x5b) return parseCsi(buffer);
    if (buffer[1] === 0x1b) return { seq: "\x1b" };
    const inner = parseEvent(buffer.subarray(1), more);
    return typeof inner === "object" && (inner.char || inner.seq) ? { alt: inner.char ?? inner.seq } : inner;
  }
  if (first === 0x7f || first === 0x09 || first === 0x0d || first <= 0x1f) return { seq: ascii(first) };
  return parseUtf8(buffer);
}

/** The events the dashboard sees when each write is a read of its own. */
function crossterm(reads) {
  const events = [];
  let buffer = [];
  for (const read of reads) {
    for (let index = 0; index < read.length; index += 1) {
      buffer.push(read[index]);
      const result = parseEvent(Buffer.from(buffer), index + 1 < read.length);
      if (result === WAIT) continue;
      buffer = [];
      if (result !== ERROR) events.push(result);
    }
  }
  return events;
}

const typedText = (events) => events.filter((event) => event.char).map((event) => event.char).join("");

// --- a fast burst of keys, with the replies a terminal sends among them ---
const key = (text) => ({ text, reply: false });
const reply = (text) => ({ text, reply: true });
const BURST = [
  key("hel"), key("l"), key("o "),
  key("\x1b[<0;233;12M"), key("\x1b[<0;233;12m"), // a click at column 233
  key("wörld "),
  reply("\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\"), // background color, ST
  key("日本語"),
  reply("\x1b[?997;1n"), // appearance changed
  key("\x1b[I"), // focus in
  key(" 👩🏽‍💻"),
  reply("\x1b]10;rgb:d8d8/d8d8/d8d8\x07"), // foreground color, BEL
  key("\x1b[27u"), // Esc with the kitty keyboard
  reply("\x1b[?62;22;52c"), // primary device attributes
  key("\x1b[1;5D"), // Ctrl+Left
  reply("\x1b[>1;10;0c"), // secondary device attributes
  key("\x1b[13;2u"), // Shift+Enter
  reply("\x1b[?2026;2$y"), // mode report
  key("é"),
  reply("\x1bP>|ghostty 1.1.3\x1b\\"), // XTVERSION
  key("\x1b[<65;233;40M"), // wheel
  reply("\x1b[?5u"), // keyboard flags
  key("\x1b[200~pasted 👋 \x1b]0;kept as pasted\x07 text\r\nline 2\x1b[201~"),
  key("\x1b[12;233R"), // a cursor report: crossterm keeps it, never types it
  key("\x1b[O"), // focus out
  key("ok"),
  key("\x1bOA"), // Up in application cursor mode
  key("\x1ba"), // Alt+a without the kitty keyboard
  key("!"),
];
const STREAM = Buffer.from(BURST.map((part) => part.text).join(""), "utf8");
const KEYS = Buffer.from(BURST.filter((part) => !part.reply).map((part) => part.text).join(""), "utf8");
const TYPED = "hello wörld 日本語 👩🏽‍💻éok!";
const EXPECTED = crossterm([KEYS]);

function fakeSocket() {
  return { readyState: 1, bufferedAmount: 0, send(_data, _options, callback) { callback?.(); } };
}

/** Send `messages` through the machine's transport; return its PTY writes. */
function deliver(messages, options = {}) {
  const writes = [];
  const io = createTerminalTransport({ writeInput: (text) => writes.push(Buffer.from(text, "utf8")), ...options });
  const socket = fakeSocket();
  io.attach(socket);
  for (const message of messages) io.writeInput(socket, message);
  io.dispose();
  return writes;
}

function splitAt(buffer, cuts) {
  const parts = [];
  let start = 0;
  for (const cut of cuts) {
    parts.push(buffer.subarray(start, cut));
    start = cut;
  }
  parts.push(buffer.subarray(start));
  return parts;
}

function assertDashboardReads(messages, label) {
  const events = crossterm(deliver(messages));
  assert.equal(typedText(events), TYPED, `typed text, ${label}`);
  assert.deepEqual(events, EXPECTED, `events, ${label}`);
}

test("the reference burst parses to exactly the typed text and keys", () => {
  assert.equal(typedText(EXPECTED), TYPED);
  assert.ok(EXPECTED.some((event) => event.paste?.includes("kept as pasted")));
  assert.ok(EXPECTED.some((event) => event.alt === "a"));
});

test("terminal replies sent whole never become typed text or swallow keys", () => {
  assertDashboardReads([STREAM], "one message");
});

test("a burst split at any byte reaches the dashboard as the same keys", () => {
  for (let cut = 1; cut < STREAM.length; cut += 1) {
    assertDashboardReads(splitAt(STREAM, [cut]), `split at byte ${cut}`);
  }
});

test("a burst sent one byte per message reaches the dashboard as the same keys", () => {
  assertDashboardReads(splitAt(STREAM, Array.from({ length: STREAM.length - 1 }, (_, index) => index + 1)), "one byte each");
});

test("a burst split at random bytes reaches the dashboard as the same keys", () => {
  let seed = 0x2545f491;
  const random = () => {
    seed ^= seed << 13;
    seed ^= seed >>> 17;
    seed ^= seed << 5;
    return (seed >>> 0) / 0x100000000;
  };
  for (let round = 0; round < 300; round += 1) {
    const cuts = new Set();
    const count = 1 + Math.floor(random() * 12);
    while (cuts.size < count) cuts.add(1 + Math.floor(random() * (STREAM.length - 1)));
    const sorted = [...cuts].sort((a, b) => a - b);
    assertDashboardReads(splitAt(STREAM, sorted), `cuts ${sorted.join(",")}`);
  }
});

test("Esc pressed alone still reaches the dashboard as Esc, after a short wait", async (t) => {
  const writes = [];
  const io = createTerminalTransport({ writeInput: (text) => writes.push(text), keyWaitMs: 10 });
  t.after(() => io.dispose());
  const socket = fakeSocket();
  io.attach(socket);

  // A terminal without the kitty keyboard sends Esc as a lone ESC.
  io.writeInput(socket, Buffer.from("x\x1b"));
  assert.deepEqual(writes, ["x"], "an ESC that ends a message waits for the rest");
  await sleep(40);
  assert.deepEqual(writes, ["x", "\x1b"]);
  assert.deepEqual(crossterm(writes.map((text) => Buffer.from(text))), [{ char: "x" }, { seq: "\x1b" }]);

  // Esc, then Up before the wait ran out: two keys, not ESC ESC (one Esc to
  // crossterm) followed by "[A" typed.
  writes.length = 0;
  io.writeInput(socket, Buffer.from("\x1b"));
  io.writeInput(socket, Buffer.from("\x1b[A"));
  assert.deepEqual(writes, ["\x1b", "\x1b[A"]);
  assert.deepEqual(crossterm(writes.map((text) => Buffer.from(text))), [{ seq: "\x1b" }, { seq: "\x1b[A" }]);

  // A mouse report whose tail never comes is not keys: written, crossterm
  // would take the keys typed after it for the rest of it.
  writes.length = 0;
  io.writeInput(socket, Buffer.from("\x1b[<0;23"));
  await sleep(40);
  io.writeInput(socket, Buffer.from("hi"));
  assert.deepEqual(writes, ["hi"]);
});

test("a reconnect drops a sequence the old connection left unfinished", (t) => {
  const writes = [];
  const io = createTerminalTransport({ writeInput: (text) => writes.push(text) });
  t.after(() => io.dispose());
  const old = fakeSocket();
  io.attach(old);
  io.writeInput(old, Buffer.from("a\x1b[<0;2"));
  io.detach(old);
  const current = fakeSocket();
  io.attach(current);
  // Joined to the old half, "b" would end a bogus mouse report that crossterm
  // holds open, taking every key after it.
  io.writeInput(current, Buffer.from("b"));
  assert.deepEqual(writes, ["a", "b"]);
});
