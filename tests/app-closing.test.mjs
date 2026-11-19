import test from "node:test";
import assert from "node:assert/strict";
import { CLOSE_AGREE_MS, hideClosed } from "../cloud/app/closing.js";

// Cloud answers /api/app/state from the home's last publish, which can come
// after the dashboard's answer to a Close. Those states still list the agent.
const stateWith = (...ids) => ({
  projects: [{ workspaceId: "ws1", agents: ids.map((id) => ({ id })) }],
  coordinator: { workspaceId: "wsAll", agents: [{ id: "c1" }] },
});
const ids = (state) => state.projects[0].agents.map((a) => a.id);

test("a closed agent stays off the list until the state stops listing it", () => {
  const closing = new Map([["ws1|a1", { title: "Fix it", answered: null }]]);
  const now = Date.UTC(2026, 10, 19, 3, 0);

  // Asked, not answered: a state from before the close does not bring it back.
  let state = stateWith("a1", "a2");
  assert.deepEqual(hideClosed(state, closing, now), []);
  assert.deepEqual(ids(state), ["a2"]);

  // Answered: Cloud's copy still lags by a publish.
  closing.set("ws1|a1", { title: "Fix it", answered: now + 900 });
  state = stateWith("a1", "a2");
  assert.deepEqual(hideClosed(state, closing, now + 1000), []);
  assert.deepEqual(ids(state), ["a2"], "the stale state after 'Agent closed' keeps the row gone");
  assert.equal(closing.size, 1);

  // The publish lands: the close is done, and nothing is hidden any more.
  state = stateWith("a2");
  hideClosed(state, closing, now + 2500);
  assert.equal(closing.size, 0);
  state = stateWith("a1", "a2");
  hideClosed(state, closing, now + 3000);
  assert.deepEqual(ids(state), ["a1", "a2"]);
});

test("a close the state never agrees with shows the row again and reports it", () => {
  const now = Date.UTC(2026, 10, 19, 3, 0);
  const closing = new Map([["ws1|a1", { title: "Fix it", answered: now }]]);
  let state = stateWith("a1");
  assert.deepEqual(hideClosed(state, closing, now + CLOSE_AGREE_MS - 1), []);
  assert.deepEqual(ids(state), []);
  state = stateWith("a1");
  assert.deepEqual(hideClosed(state, closing, now + CLOSE_AGREE_MS), [{ title: "Fix it", answered: now }]);
  assert.deepEqual(ids(state), ["a1"]);
  assert.equal(closing.size, 0);
});

test("a close still being asked is kept even when a state does not list the agent", () => {
  const closing = new Map([["wsAll|c1", { title: null, answered: null }]]);
  const state = { projects: [{ workspaceId: "ws1", agents: [] }], coordinator: null };
  hideClosed(state, closing, 0);
  assert.equal(closing.size, 1, "only the dashboard's answer ends a close in flight");
  const all = stateWith();
  hideClosed(all, closing, 0);
  assert.deepEqual(all.coordinator.agents, [], "the All agents project is covered too");
});
