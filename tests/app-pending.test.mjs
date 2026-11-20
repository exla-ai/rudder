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
  const first = { id: "p1", text: typed, ts: now, state: "sent", after: sentAfter(before) };
  const withFirst = [...before, { id: "u2", role: "user", ts: now + skew + 500, text: recorded }];
  assert.deepEqual(settlePending([first], withFirst, now + 2000), [], "whitespace and clock skew do not leave a second bubble");

  // The same text sent again: the first entry is not this one.
  const again = { id: "p2", text: typed, ts: now + 90_000, state: "sent", after: sentAfter(withFirst) };
  assert.deepEqual(settlePending([again], withFirst, now + 91_000), [again], "a second send shows until its own entry arrives");
  const withBoth = [...withFirst, { id: "u3", role: "user", ts: now + skew + 90_500, text: recorded }];
  assert.deepEqual(settlePending([again], withBoth, now + 92_000), []);

  // Both in flight before either entry shows: each settles on one entry.
  const twin = { ...first, id: "p1b", after: sentAfter(before) };
  const twin2 = { ...first, id: "p2b", state: "queued", after: sentAfter(before) };
  const remaining = settlePending([twin, twin2], withFirst, now + 2000);
  assert.deepEqual(remaining, [twin2]);
  assert.deepEqual(settlePending(remaining, withFirst, now + 600_000), [twin2], "a previous entry cannot settle a queued message on a later poll");
  assert.deepEqual(settlePending(remaining, withBoth, now + 601_000), []);
});

test("automatic compaction does not settle a queued /compact", () => {
  const item = { id: "p4", text: "/compact", ts: now, state: "queued", after: now + skew };
  const auto = { id: "c1", role: "system", kind: "compact", ts: now + skew + 1000, compact: { trigger: "auto" } };
  const manual = { ...auto, id: "c2", ts: auto.ts + 1000, compact: { trigger: "manual" } };
  assert.deepEqual(settlePending([item], [auto], now + 2000), [item]);
  assert.deepEqual(settlePending([item], [auto, manual], now + 3000), []);
});

test("a message sent with files settles on the transcript entry with the same words and files", () => {
  const shot = "/workspace/code/rudder/.rudder/uploads/20261119-044218-image.png";
  const sent = { id: "p3", text: "use chrome", files: [{ name: "image.png", path: shot }], ts: now, state: "sent", after: now + skew };
  const wordsOnly = { id: "u4", role: "user", ts: now + skew + 500, text: "use chrome" };
  assert.deepEqual(settlePending([sent], [wordsOnly], now + 2000), [sent], "the same words without the file are another message");
  const withFile = { ...wordsOnly, id: "u5", attachments: [shot] };
  assert.deepEqual(settlePending([sent], [wordsOnly, withFile], now + 2000), []);
});
