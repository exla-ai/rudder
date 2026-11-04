import assert from "node:assert/strict";
import test from "node:test";
import { formatTimeline, parseTimeline } from "../dist/run-timeline.js";

test("timeline summary reads the latest workspace and summary events", () => {
  const content = [
    JSON.stringify({ kind: "workspace", ms: 800 }),
    JSON.stringify({ kind: "summary", buildMs: 12000, modelMs: 180000, input: 100000, cached: 1100000, output: 20000 }),
    "not json",
  ].join("\n");
  const t = parseTimeline(content);
  assert.equal(formatTimeline(t), "start 0.8s · install 0s · build 12s · test 0s · model 3m · 1.22M tok, 91% cached");
});

test("headless runs fall back to run.tokens", () => {
  const t = parseTimeline("", { tokens: { input: 500, output: 20 } });
  assert.equal(t.input, 500);
  assert.equal(parseTimeline(""), undefined);
});
