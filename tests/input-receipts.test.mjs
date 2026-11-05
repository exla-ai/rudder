// Keys typed into a dead `rudder cloud` link must reach the next connection
// instead of vanishing (src/input-receipts.ts, AttachSession.keepUnsent).
import test from "node:test";
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { InputReceipts } from "../dist/input-receipts.js";
import { AttachSession } from "../dist/attach-session.js";

const keys = (text) => Buffer.from(text);
const texts = (chunks) => chunks.map((chunk) => chunk.toString());

test("a pong confirms exactly the keys sent before its ping", () => {
  let clock = 1_000;
  const receipts = new InputReceipts(() => clock);
  assert.equal(receipts.sent(keys("h")), true, "the first key asks for a receipt");
  const first = receipts.nextPing();
  assert.equal(receipts.sent(keys("i")), false, "one receipt in flight at a time");
  clock += 40;
  const answer = receipts.pong(first);
  assert.equal(answer.rtt, 40);
  assert.equal(answer.again, true, "the key after the ping still needs a receipt");
  assert.deepEqual(texts(receipts.takeUnconfirmed()), ["i"], "only the key after the ping is unconfirmed");
});

test("an ordered stream: a later pong answers earlier pings too", () => {
  const receipts = new InputReceipts(() => 0);
  receipts.sent(keys("a"));
  receipts.nextPing();
  receipts.sent(keys("b"));
  const second = receipts.nextPing();
  assert.deepEqual(receipts.pong(second), { rtt: 0, again: false });
  assert.equal(receipts.unconfirmed, 0);
  assert.equal(receipts.oldestPingAge(), 0, "no ping left out");
});

test("a pong nobody asked for confirms nothing", () => {
  const receipts = new InputReceipts(() => 0);
  receipts.sent(keys("x"));
  receipts.nextPing();
  assert.deepEqual(receipts.pong(Buffer.from("not-ours")), { rtt: null, again: false });
  assert.equal(receipts.unconfirmed, 1);
});

test("an unanswered ping ages, which is how a dead link is noticed", () => {
  let clock = 0;
  const receipts = new InputReceipts(() => clock);
  receipts.sent(keys("y"));
  receipts.nextPing();
  clock += 4_500;
  assert.equal(receipts.oldestPingAge(), 4_500);
});

function heldSession() {
  const input = new EventEmitter();
  input.setRawMode = () => {};
  input.resume = () => {};
  input.pause = () => {};
  const output = new EventEmitter();
  output.columns = 60;
  output.rows = 10;
  output.write = () => true;
  const session = new AttachSession("libra on home", input, output, { interactive: true, now: () => 0, predict: false });
  session.hold();
  return { session, input };
}

test("unconfirmed keys go first on the next connection, then keys typed during the gap", () => {
  const { session, input } = heldSession();
  session.keepUnsent([keys("fix "), keys("the")], true);
  session.enterGap();
  input.emit("data", keys(" bug"));
  session.keepUnsent([keys("!")], false);
  assert.equal(session.takeQueued()?.toString(), "fix the bug!");
  session.release();
});

test("an interrupt is never replayed into the next connection", () => {
  const { session } = heldSession();
  session.keepUnsent([keys("ok"), keys("\x03"), keys(" go")], true);
  assert.equal(session.takeQueued()?.toString(), "ok go");
  session.release();
});
