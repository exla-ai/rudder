import test from "node:test";
import assert from "node:assert/strict";
import { EchoPredictor, endsMidEscape } from "../dist/predict.js";

const settle = () => new Promise((resolve) => setTimeout(resolve, 30));
const SYNC = (body) => `\x1b[?2026h${body}\x1b[?2026l`;
// A cloud frame: prompt on row 3, caret (visible cursor) after "> ".
const frameWith = (text, caretCol) => SYNC(`\x1b[2J\x1b[3;1H> ${text}\x1b[?25h\x1b[3;${caretCol}H`);

function setup(options = {}) {
  let clock = 1000;
  const out = [];
  const predictor = new EchoPredictor(40, 8, (text) => out.push(text), { now: () => clock, ...options });
  return { predictor, out, tick: (ms) => { clock += ms; } };
}

test("no prediction until the round trip is worth hiding", async () => {
  const { predictor, out } = setup();
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.noteRtt(5);
  predictor.onInput("a");
  assert.equal(out.length, 0);
});

test("a typed character shows at the caret at once and the cloud's echo confirms it", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.onInput("hi");
  assert.equal(out.join(""), "\x1b[0m\x1b[3;3Hh\x1b[0m\x1b[3;3Hh\x1b[3;4Hi");
  out.length = 0;
  predictor.onServerData(frameWith("hi", 5));
  await settle();
  assert.equal(predictor.confirmed, 2);
  assert.equal(predictor.rollbacks, 0);
  assert.equal(out.length, 0, "a confirmed echo needs no repaint");
});

test("a frame that has not echoed yet keeps the prediction drawn", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.onInput("x");
  out.length = 0;
  // A spinner repaint arrives before the echo; it may have redrawn the row.
  predictor.onServerData(frameWith("", 3));
  await settle();
  assert.match(out.join(""), /\x1b\[3;3Hx/);
});

test("a key that was not typing is rolled back from the model", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.onInput("j");
  out.length = 0;
  // The key moved a selection: the caret went to another row, nothing typed.
  predictor.onServerData(SYNC("\x1b[6;1Hmenu\x1b[6;5H"));
  await settle();
  assert.equal(predictor.rollbacks, 1);
  const repaint = out.join("");
  assert.match(repaint, /\x1b\[3;3H\x1b\[0m /, "the cell is repainted blank, as the cloud has it");
  assert.match(repaint, /\x1b\[6;5H$/, "the cursor goes back to the cloud's caret");
});

test("an unanswered prediction times out and is rolled back", async () => {
  const { predictor, tick } = setup({ timeoutMs: 500 });
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.onInput("q");
  tick(600);
  await new Promise((resolve) => setTimeout(resolve, 150));
  assert.equal(predictor.rollbacks, 1);
  predictor.dispose();
});

test("nothing is drawn inside a cloud frame; it waits for the frame to end", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.onServerData("\x1b[?2026h\x1b[1;1Hhalf a frame");
  await settle();
  predictor.onInput("z");
  assert.equal(out.length, 0);
  predictor.onServerData("\x1b[3;3H\x1b[?2026l");
  await settle();
  assert.match(out.join(""), /\x1b\[3;3Hz/);
});

test("backspace takes back a prediction instantly; Enter stops predicting until the cloud catches up", async () => {
  const { predictor, out, tick } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.onInput("ab");
  out.length = 0;
  predictor.onInput("\x7f");
  assert.match(out.join(""), /\x1b\[3;4H\x1b\[0m \x1b\[0m\x1b\[3;4H$/);
  out.length = 0;
  predictor.onInput("\r");
  predictor.onInput("c");
  assert.equal(out.length, 0, "no guess right after Enter");
  tick(1000);
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.onInput("c");
  assert.ok(out.length > 0, "predicts again once a round trip has passed");
});

test("only blank cells are typed over, and a hidden caret is never guessed at", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(SYNC("\x1b[2J\x1b[3;1H> abc\x1b[?25h\x1b[3;4H"));
  await settle();
  predictor.onInput("x");
  assert.equal(out.length, 0, "the caret is over text: editing mid-line is left to the cloud");
  const hidden = setup();
  hidden.predictor.noteRtt(100);
  hidden.predictor.onServerData(SYNC("\x1b[2J\x1b[?25l\x1b[3;3H"));
  await settle();
  hidden.predictor.onInput("x");
  assert.equal(hidden.out.length, 0);
});

test("endsMidEscape spots a chunk cut inside a sequence", () => {
  assert.equal(endsMidEscape("abc"), false);
  assert.equal(endsMidEscape("abc\x1b"), true);
  assert.equal(endsMidEscape("abc\x1b[3;"), true);
  assert.equal(endsMidEscape("abc\x1b[3;4H"), false);
  assert.equal(endsMidEscape("\x1b]0;title"), true);
  assert.equal(endsMidEscape("\x1b]0;title\x07"), false);
});

test("a frame drawn elsewhere mid-flight is not mistaken for a contradiction", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.onInput("k");
  predictor.onServerData("\x1b[?2026h\x1b[7;1Hstatus line");
  await settle();
  assert.equal(predictor.rollbacks, 0);
  predictor.onServerData("\x1b[3;3H\x1b[?2026l");
  await settle();
  assert.equal(predictor.rollbacks, 0);
  assert.match(out.join(""), /\x1b\[3;3Hk/);
});
