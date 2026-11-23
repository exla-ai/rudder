import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

const { waitRunWorking } = await import("../cloud/worker/steer-inbox.mjs");

function repoWithRun(status) {
  const repo = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-steer-inbox-"));
  const dir = path.join(repo, ".rudder", "runs", "run-1");
  fs.mkdirSync(dir, { recursive: true });
  const write = (next) => {
    // The dashboard writes run.json through a temp file and a rename.
    fs.writeFileSync(path.join(dir, "run.json.tmp"), JSON.stringify({ id: "run-1", status: next }));
    fs.renameSync(path.join(dir, "run.json.tmp"), path.join(dir, "run.json"));
  };
  write(status);
  return { repo, write };
}

test("a send to an idle agent waits until its row turns working", async (t) => {
  const { repo, write } = repoWithRun("completed");
  t.after(() => fs.rmSync(repo, { recursive: true, force: true }));
  const started = Date.now();
  const seen = waitRunWorking(repo, "run-1", 5000);
  setTimeout(() => write("running"), 150);
  assert.equal(await seen, true);
  assert.ok(Date.now() - started < 2000, "resolved on the write, not the timeout");
  assert.equal(await waitRunWorking(repo, "run-1", 5000), true, "already working answers at once");
});

test("a send that starts no turn answers after the wait", async (t) => {
  const { repo } = repoWithRun("completed");
  t.after(() => fs.rmSync(repo, { recursive: true, force: true }));
  assert.equal(await waitRunWorking(repo, "run-1", 200), false);
});
