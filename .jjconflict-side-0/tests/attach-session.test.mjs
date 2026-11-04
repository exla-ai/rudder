import test from "node:test";
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { AttachSession } from "../dist/attach-session.js";
import { EchoPredictor } from "../dist/predict.js";

const settle = () => new Promise((resolve) => setTimeout(resolve, 30));

function fakeTerminal() {
  const input = new EventEmitter();
  input.raw = null;
  input.paused = true;
  input.setRawMode = (mode) => { input.raw = mode; };
  input.resume = () => { input.paused = false; };
  input.pause = () => { input.paused = true; };
  const output = new EventEmitter();
  output.columns = 60;
  output.rows = 10;
  output.writes = [];
  output.write = (text) => { output.writes.push(text); return true; };
  output.text = () => output.writes.join("");
  return { input, output };
}

function heldSession(options = {}) {
  let clock = 10_000;
  const { input, output } = fakeTerminal();
  const session = new AttachSession("libra on home", input, output, { interactive: true, now: () => clock, ...options });
  session.hold();
  return { session, input, output, tick: (ms) => { clock += ms; } };
}

test("the terminal is held once and given back exactly once", () => {
  const { session, input, output } = heldSession();
  session.hold();
  assert.equal(input.raw, true);
  assert.equal(output.text().split("\x1b[?1049h").length - 1, 1, "one alternate-screen switch");
  session.release();
  session.release();
  assert.equal(input.raw, false);
  assert.equal(output.text().split("\x1b[?1049l").length - 1, 1, "left once");
  assert.ok(output.text().includes("\x1b[<u"), "keyboard flags popped");
  assert.equal(input.paused, true);
});

test("a drop keeps the screen, shows a badge, and keeps typing for the next connection", async () => {
  const { session, input, output } = heldSession();
  session.predictor.onServerData("\x1b[1;1Hdashboard header\x1b[5;1H> ");
  await settle();
  output.writes.length = 0;

  session.enterGap();
  const drawn = output.text();
  assert.ok(!drawn.includes("\x1b[?1049l"), "never leaves the alternate screen");
  assert.ok(!drawn.includes("\x1b[2J"), "never clears the picture");
  assert.ok(drawn.includes("Reconnecting"), "says what is happening");
  assert.ok(drawn.includes("\x1b7") && drawn.includes("\x1b8"), "puts the cursor back where the dashboard had it");
  assert.equal(input.paused, false, "keys are read during the gap");

  input.emit("data", Buffer.from("yes"));
  input.emit("data", "\r");
  assert.equal(session.takeQueued()?.toString(), "yes\r");
  assert.equal(session.takeQueued(), null, "handed over once");

  session.exitGap();
  input.emit("data", Buffer.from("after"));
  assert.equal(session.takeQueued(), null, "the connection reads keys once it is open");
  session.release();
});

test("Ctrl+C in a gap is never queued; twice leaves", () => {
  const { session, input, output, tick } = heldSession();
  session.enterGap();
  let left = false;
  session.onLeave(() => { left = true; });
  input.emit("data", Buffer.from([0x03]));
  assert.equal(left, false);
  assert.ok(output.text().includes("Ctrl+C again to leave"));
  assert.equal(session.takeQueued(), null, "an interrupt is not replayed into the dashboard");
  tick(500);
  input.emit("data", Buffer.from("\x1b[99;5u", "latin1"));
  assert.equal(left, true);
  assert.equal(session.hasLeft, true);
  session.release();
});

test("a second Ctrl+C long after the first only asks again", () => {
  const { session, tick } = heldSession();
  assert.equal(session.ctrlCToLeave(), false);
  tick(10_000);
  assert.equal(session.ctrlCToLeave(), false);
  tick(100);
  assert.equal(session.ctrlCToLeave(), true);
  session.release();
});

test("the badge comes off by repainting exactly what it covered", async () => {
  const { session, output } = heldSession();
  // A row-0 picture that runs under where the badge goes.
  session.predictor.onServerData(`\x1b[1;1H${"abcdefghij".repeat(6)}`);
  await settle();
  session.showStatus("Cloud machine reconnecting");
  output.writes.length = 0;
  session.clearStatus();
  const erase = output.text();
  // The badge sat at the right edge; its cells are redrawn from the model.
  assert.match(erase, /\x1b\[1;\d+H/);
  const drawnBack = erase.replace(/\x1b\[[0-9;]*m/g, "");
  assert.ok(drawnBack.includes("ghij"), `the covered text is drawn back: ${JSON.stringify(erase)}`);
  assert.equal(session.statusText, null);
  output.writes.length = 0;
  session.clearStatus();
  assert.equal(output.text(), "", "nothing to take off twice");
  session.release();
});

test("waiting out a retry ends early when the user leaves", async () => {
  const { session } = heldSession();
  const started = Date.now();
  const waiting = session.wait(5_000);
  session.leave();
  assert.equal(await waiting, true);
  assert.ok(Date.now() - started < 1_000);
  assert.equal(await session.wait(5_000), true, "already gone");
  session.release();
});

test("without a TTY a drop is a line on stderr, never terminal codes", () => {
  const { input, output } = fakeTerminal();
  const logged = [];
  const session = new AttachSession("x", input, output, { interactive: false, log: (text) => logged.push(text) });
  session.hold();
  session.enterGap();
  session.noteAttempt(2);
  assert.equal(output.writes.length, 0);
  assert.match(logged.join(""), /Reconnecting \(try 2\)/);
  session.release();
});

test("paintRegion redraws cells as the model has them and restores the cursor", async () => {
  const out = [];
  const predictor = new EchoPredictor(20, 4, (text) => out.push(text), { predict: false });
  predictor.onServerData("\x1b[2;3H\x1b[31mred\x1b[0m plain");
  await settle();
  const paint = predictor.paintRegion(1, 2, 5);
  assert.ok(paint.startsWith("\x1b7\x1b[2;3H"), JSON.stringify(paint));
  assert.ok(paint.endsWith("\x1b8"));
  assert.match(paint, /38;5;1mr/, "in its colour");
  assert.ok(paint.includes("\x1b[0m "), "the blank between words is drawn too");
  // With prediction off, typing never draws anything.
  predictor.noteRtt(200);
  predictor.onInput("x");
  assert.equal(out.length, 0);
  predictor.dispose();
});
