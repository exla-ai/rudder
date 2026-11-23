// The home publishes the app's state the moment a watched run record changes
// (cloud/worker/run-watch.mjs), so Working ends as the reply lands instead of
// up to 1.5 s later.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRunWatch, runSignature } from "../cloud/worker/run-watch.mjs";

test("a watched run record publishes once when it changes", (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-run-watch-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const working = path.join(dir, "working.json");
  const idle = path.join(dir, "idle.json");
  fs.writeFileSync(working, JSON.stringify({ status: "running" }));
  fs.writeFileSync(idle, JSON.stringify({ status: "completed" }));
  let published = 0;
  const watch = createRunWatch(() => { published += 1; });

  assert.equal(watch.wants(working, "working"), true);
  assert.equal(watch.wants(idle, "done"), false, "an idle agent's record is not watched");
  watch.sent(idle);
  assert.equal(watch.wants(idle, "done"), true, "until a send reaches it");

  // The state read both; nothing changed since.
  watch.watch(new Map([[working, runSignature(fs.statSync(working))], [idle, runSignature(fs.statSync(idle))]]));
  watch.check();
  assert.equal(published, 0);

  // The turn ends: one publish, which reads the records again.
  fs.writeFileSync(working, JSON.stringify({ status: "completed", updatedAt: 2 }));
  watch.check();
  watch.check();
  assert.equal(published, 1, "one publish until the state reads the records again");

  // A record the state read before it existed shows up.
  const fresh = path.join(dir, "fresh.json");
  watch.watch(new Map([[fresh, runSignature(undefined)]]));
  watch.check();
  assert.equal(published, 1);
  fs.writeFileSync(fresh, "{}");
  watch.check();
  assert.equal(published, 2);
});
