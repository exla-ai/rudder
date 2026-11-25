import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { queueSteer, readSteerReceipt, withdrawSteer } from "../cloud/worker/steer-inbox.mjs";
import { sendAttempts } from "../cloud/app/pending.js";

// A queued message the app sends again must not also arrive late: withdrawing
// it writes its receipt first, which the dashboard treats as already handled
// (native poll_steer_inbox), unless the dashboard got to it first.
test("withdrawing a queued request leaves the dashboard nothing to deliver", async (t) => {
  const repo = fs.mkdtempSync(path.join(os.tmpdir(), "steer-withdraw-"));
  t.after(() => fs.rmSync(repo, { recursive: true, force: true }));
  const inbox = path.join(repo, ".rudder", "steer");
  const receipts = path.join(repo, ".rudder", "steer-receipts");

  const id = queueSteer(repo, { kind: "steer", taskId: "run-1", instruction: "hello" });
  assert.deepEqual(await withdrawSteer(repo, id), { status: "withdrawn" });
  assert.deepEqual(fs.readdirSync(inbox), [], "the request left the inbox");
  assert.equal(JSON.parse(fs.readFileSync(path.join(receipts, `${id}.json`), "utf8")).status, "failed", "its receipt marks it handled");
  assert.deepEqual(await withdrawSteer(repo, id), { status: "withdrawn" }, "a second withdraw says the same");

  // The dashboard is acting on it, or already answered: that answer wins.
  const busy = queueSteer(repo, { kind: "steer", taskId: "run-1", instruction: "two" });
  fs.writeFileSync(path.join(receipts, `${busy}.json`), JSON.stringify({ requestId: busy, status: "processing" }));
  assert.deepEqual(await withdrawSteer(repo, busy), { status: "processing" });
  fs.writeFileSync(path.join(receipts, `${busy}.json`), JSON.stringify({ requestId: busy, status: "delivered" }));
  assert.equal((await withdrawSteer(repo, busy)).status, "delivered");
  assert.equal(readSteerReceipt(repo, busy).status, "delivered");

  const refused = queueSteer(repo, { kind: "steer", taskId: "run-1", instruction: "three" });
  fs.writeFileSync(path.join(receipts, `${refused}.json`), JSON.stringify({ requestId: refused, status: "failed", error: "not sent: a menu is open in its pane (Esc closes it)", code: "menu_open" }));
  assert.deepEqual(await withdrawSteer(repo, refused), { status: "failed", error: "not sent: a menu is open in its pane (Esc closes it)", code: "menu_open" });

  await assert.rejects(withdrawSteer(repo, "../x"), /bad request id/);
});

// The phone gave up on a new task after a minute while the server went on to
// deliver it; Try again then started a second agent. The app now sends the
// retry under the first send's id, and that id is queued once.
test("a send repeated after no answer is one request", (t) => {
  const repo = fs.mkdtempSync(path.join(os.tmpdir(), "steer-retry-"));
  t.after(() => fs.rmSync(repo, { recursive: true, force: true }));
  const inbox = path.join(repo, ".rudder", "steer");
  const receipts = path.join(repo, ".rudder", "steer-receipts");
  const task = { kind: "task", taskId: "conductor", instruction: "add a health check" };

  // The app: no answer keeps the id, an answer or other words end it.
  const starts = sendAttempts();
  const id = starts.id(task.instruction);
  starts.settle(Object.assign(new Error("Can't reach Rudder. Check your connection."), { status: 0 }));
  assert.equal(starts.id(task.instruction), id, "a dropped connection: the retry is the same request");
  starts.settle(Object.assign(new Error("home machine did not answer task in time"), { status: 504 }));
  assert.equal(starts.id(task.instruction), id);
  assert.notEqual(sendAttempts().id("something else"), id);

  // The home: still in the inbox, claimed by the dashboard, or answered.
  assert.equal(queueSteer(repo, { ...task, requestId: id }), id);
  assert.equal(queueSteer(repo, { ...task, requestId: id }), id);
  assert.equal(fs.readdirSync(inbox).length, 1, "the retry found the first one waiting");
  fs.mkdirSync(receipts, { recursive: true });
  fs.writeFileSync(path.join(receipts, `${id}.json`), JSON.stringify({ requestId: id, status: "processing" }));
  fs.rmSync(path.join(inbox, fs.readdirSync(inbox)[0]));
  queueSteer(repo, { ...task, requestId: id });
  fs.writeFileSync(path.join(receipts, `${id}.json`), JSON.stringify({ requestId: id, status: "delivered" }));
  queueSteer(repo, { ...task, requestId: id });
  assert.deepEqual(fs.readdirSync(inbox), [], "a request the dashboard took is not queued again");
  assert.equal(readSteerReceipt(repo, id).status, "delivered", "the retry reads the first one's answer");

  // An answer ends the attempt: sending again is a new request.
  starts.settle(Object.assign(new Error("the dashboard is starting"), { status: 400 }));
  const next = starts.id(task.instruction);
  assert.notEqual(next, id);
  queueSteer(repo, { ...task, requestId: next });
  assert.equal(fs.readdirSync(inbox).length, 1);
  assert.throws(() => queueSteer(repo, { ...task, requestId: "../x" }), /bad request id/);
});
