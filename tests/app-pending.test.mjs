import test from "node:test";
import assert from "node:assert/strict";
import { sentAfter, settlePending } from "../cloud/app/pending.js";

// What the composer sent, and what Claude Code recorded after the dashboard
// typed it in (sanitize_steer_instruction trims each line and collapses runs
// of spaces). The agent's machine runs three minutes behind this device.
const typed = "ok I talked to them.  It is fine. \n\nGo ahead and make it work";
const recorded = "ok I talked to them. It is fine.\n\nGo ahead and make it work";
const now = Date.UTC(2026, 10, 18, 4, 52);
const skew = -3 * 60_000;

test("a sent message settles on its own transcript entry, once", () => {
  const before = [
    { id: "u1", role: "user", ts: now + skew - 60_000, text: "earlier" },
    { id: "a1", role: "assistant", ts: now + skew - 30_000, text: "reply" },
  ];
  const first = { id: "p1", text: typed, sentText: typed, ts: now, state: "sent", after: sentAfter(before) };
  const withFirst = [...before, { id: "u2", role: "user", ts: now + skew + 500, text: recorded }];
  assert.deepEqual(settlePending([first], withFirst, now + 2000), [], "whitespace and clock skew do not leave a second bubble");

  // The same text sent again: the first entry is not this one.
  const again = { id: "p2", text: typed, sentText: typed, ts: now + 90_000, state: "sent", after: sentAfter(withFirst) };
  assert.deepEqual(settlePending([again], withFirst, now + 91_000), [again], "a second send shows until its own entry arrives");
  const withBoth = [...withFirst, { id: "u3", role: "user", ts: now + skew + 90_500, text: recorded }];
  assert.deepEqual(settlePending([again], withBoth, now + 92_000), []);

  // Both in flight before either entry shows: each settles on one entry.
  const twin = { ...first, id: "p1b", after: sentAfter(before) };
  const twin2 = { ...first, id: "p2b", after: sentAfter(before) };
  assert.deepEqual(settlePending([twin, twin2], withFirst, now + 2000), [twin2]);
  assert.deepEqual(settlePending([twin, twin2], withBoth, now + 92_000), []);
});
