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
  assert.equal(out.join(""), "\x1b[3;3H\x1b[0mh\x1b[0m\x1b[3;3H\x1b[0mh\x1b[3;4H\x1b[0mi\x1b[0m");
  out.length = 0;
  predictor.onServerData(frameWith("hi", 5));
  await settle();
  assert.equal(predictor.confirmed, 2);
  assert.equal(predictor.rollbacks, 0);
  assert.equal(out.length, 0, "a confirmed echo needs no repaint");
});

test("a typed character keeps the input box's background and the text's color", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  // opencode: a grey input box, earlier text in a lighter grey, caret after it.
  const box = "\x1b[48;2;30;30;30m";
  predictor.onServerData(SYNC(`\x1b[2J\x1b[3;1H${box}\x1b[38;2;238;238;238mab${box}\x1b[38;2;255;255;255m          \x1b[0m\x1b[?25h\x1b[3;3H`));
  await settle();
  predictor.onInput("c");
  assert.equal(out.join(""), "\x1b[3;3H\x1b[0;38;2;238;238;238;48;2;30;30;30mc\x1b[0m");
});

test("on an empty input the character takes the blank cell's look", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(SYNC(`\x1b[2J\x1b[3;1H\x1b[48;5;236m          \x1b[0m\x1b[?25h\x1b[3;1H`));
  await settle();
  predictor.onInput("x");
  assert.equal(out.join(""), "\x1b[3;1H\x1b[0;48;5;236mx\x1b[0m");
});

// An empty opencode input: grey box, grey hint text starting at the caret.
const hintFrame = (caretCol = 3) => SYNC(`\x1b[2J\x1b[3;1H\x1b[48;2;30;30;30m  \x1b[38;2;128;128;128mAsk me\x1b[38;2;255;255;255m     \x1b[0m\x1b[?25h\x1b[3;${caretCol}H`);

test("the first key types over an empty input's hint, which clears like the echo will", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(hintFrame());
  await settle();
  predictor.onInput("ok");
  const box = "\x1b[0;38;2;255;255;255;48;2;30;30;30m";
  assert.equal(out.at(-1), `\x1b[3;3H${box}      \x1b[3;3H${box}o\x1b[3;4H${box}k\x1b[0m`);
  out.length = 0;
  predictor.onServerData(SYNC(`\x1b[2J\x1b[3;1H\x1b[48;2;30;30;30m  ok          \x1b[0m\x1b[?25h\x1b[3;5H`));
  await settle();
  assert.equal(predictor.confirmed, 2);
  assert.equal(predictor.rollbacks, 0);
});

test("a hint typed over comes back on a rollback and on deleting the character", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(hintFrame());
  await settle();
  predictor.onInput("a");
  out.length = 0;
  predictor.onInput("\x7f");
  assert.match(out.join("").replace(/\x1b\[[0-9;]*m/g, ""), /Ask me/, "backspace repaints the hint");
  predictor.onInput("j");
  out.length = 0;
  predictor.onServerData(SYNC("\x1b[6;1Hmenu\x1b[6;5H"));
  await settle();
  assert.equal(predictor.rollbacks, 1);
  assert.match(out.join("").replace(/\x1b\[[0-9;]*m/g, ""), /Ask me/, "a rollback repaints the hint");
});

test("ordinary text under the caret is not mistaken for a hint", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("hello", 3));
  await settle();
  predictor.onInput("x");
  assert.equal(out.length, 0);
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
  assert.match(out.join(""), /\x1b\[3;3H\x1b\[0mx/);
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
  assert.match(out.join(""), /\x1b\[3;3H\x1b\[0mz/);
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
  assert.match(out.join(""), /\x1b\[3;3H\x1b\[0mk/);
});

test("a cloud frame that lands before the echo keeps the cursor after the typed characters", async () => {
  const { predictor } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.onInput("hi");
  // An animation frame (a spinner elsewhere) parks the cloud's caret at col 3:
  // the frame redraws the predictions and parks the cursor after them.
  const spinner = SYNC("\x1b[6;1H*\x1b[?25h\x1b[3;3H");
  const written = predictor.decorate(spinner);
  assert.equal(written, SYNC("\x1b[6;1H*\x1b[?25h\x1b[3;3H\x1b[3;3H\x1b[0mh\x1b[3;4H\x1b[0mi\x1b[0m\x1b[3;5H"));
  predictor.onServerData(spinner);
  await settle();
  // Once the echo is in, frames pass through untouched.
  predictor.onServerData(frameWith("hi", 5));
  await settle();
  assert.equal(predictor.decorate(spinner), spinner);
  // A chunk that ends inside an open frame is left alone.
  predictor.onInput("!");
  assert.equal(predictor.decorate("\x1b[?2026h\x1b[6;1H*"), "\x1b[?2026h\x1b[6;1H*");
});

test("keys typed while the cloud catches up are drawn once it has, at normal typing speed", async () => {
  const { predictor, out, tick } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("", 3));
  await settle();
  // Ctrl+U: the model cannot know what it did, so prediction holds.
  predictor.onInput("\x15");
  for (const key of "abc") {
    tick(40);
    predictor.onInput(key);
  }
  assert.equal(out.length, 0, "nothing is guessed before the cloud answers the hold");
  // The cloud has drawn Ctrl+U's effect and the first held key; b and c are in flight.
  predictor.onServerData(frameWith("a", 4));
  await settle();
  tick(100);
  predictor.onInput("d");
  const drawn = out.join("");
  assert.match(drawn, /\x1b\[3;4H\x1b\[0mb/, "a key still on its way is drawn after the echoed one");
  assert.match(drawn, /\x1b\[3;5H\x1b\[0mc/);
  assert.match(drawn, /\x1b\[3;6H\x1b\[0md/, "the key typed now comes after them");
  out.length = 0;
  for (const key of "ef") {
    tick(40);
    predictor.onInput(key);
  }
  assert.match(out.join(""), /\x1b\[3;8H\x1b\[0mf/, "later keys predict at once: the hold did not re-arm");
  predictor.onServerData(frameWith("abcdef", 9));
  await settle();
  assert.equal(predictor.confirmed, 5);
  assert.equal(predictor.rollbacks, 0);
});

test("Rudder's own slate-grey task hint is typed over like any hint", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  const hint = "\x1b[38;2;140;147;163mdescribe a task…\x1b[0m";
  predictor.onServerData(SYNC(`\x1b[2J\x1b[3;1H> ${hint}\x1b[?25h\x1b[3;3H`));
  await settle();
  predictor.onInput("x");
  assert.ok(out.join("").includes("x"), "the first key into an empty task input is predicted");
});

test("an echo frame that repaints the input never wipes keys still in flight", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(hintFrame());
  await settle();
  predictor.onInput("okay");
  // The first echo repaints the whole input row: "o", then blanks where the hint was.
  const echo = SYNC(`\x1b[2J\x1b[3;1H\x1b[48;2;30;30;30m  o           \x1b[0m\x1b[?25h\x1b[3;4H`);
  const written = predictor.decorate(echo);
  const close = written.lastIndexOf("\x1b[?2026l");
  const inside = written.slice(0, close);
  for (const [col, ch] of [[4, "k"], [5, "a"], [6, "y"]]) {
    assert.ok(inside.includes(`\x1b[3;${col}H`) && inside.includes(ch), `${ch} is redrawn inside the echo frame`);
  }
  out.length = 0;
  predictor.onServerData(echo);
  await settle();
  assert.equal(predictor.confirmed, 1);
  assert.equal(predictor.rollbacks, 0);
});

test("a prediction drawn in a frame takes the echo's look when the guess was off", async () => {
  const { predictor, out } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.onInput("h");
  // The echo draws the character bold: decorate redraws it plain on top.
  const echo = SYNC("\x1b[2J\x1b[3;1H> \x1b[1mh\x1b[0m\x1b[?25h\x1b[3;4H");
  predictor.decorate(echo);
  out.length = 0;
  predictor.onServerData(echo);
  await settle();
  assert.equal(predictor.confirmed, 1);
  assert.match(out.join(""), /\x1b\[3;3H\x1b\[0;1mh/, "the confirmed cell is repainted as the cloud drew it");
});

test("a chunk that closes the echo frame and opens the next still redraws the predictions", async () => {
  const { predictor } = setup();
  predictor.noteRtt(100);
  predictor.onServerData(frameWith("", 3));
  await settle();
  predictor.onInput("hi");
  const chunk = `${SYNC("\x1b[3;1H> h\x1b[?25h\x1b[3;4H")}\x1b[?2026h\x1b[6;1H*`;
  const written = predictor.decorate(chunk);
  const close = written.indexOf("\x1b[?2026l");
  assert.ok(written.slice(0, close).includes("\x1b[3;4H\x1b[0mi"), "the key in flight is redrawn before the first frame closes");
  assert.ok(written.endsWith("\x1b[?2026h\x1b[6;1H*"), "the frame left open is untouched");
});
