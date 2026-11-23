import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { queueSteer, readSteerReceipt, withdrawSteer } from "../cloud/worker/steer-inbox.mjs";

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
