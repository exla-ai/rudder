import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";
import { codexAskSteps, createCodexAnswerQueue } from "../cloud/worker/asks.mjs";

const askId = "1-12345678";
const runId = "run-codex";
const ask = { kind: "question", tool: "request_user_input_async", input: { questions: [
  { question: "Keep going?", options: [{ label: "Yes" }, { label: "No" }] },
] } };
const steps = codexAskSteps(ask, { answers: { "Keep going?": "No" } });

function fixture(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-answer-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const project = { dir };
  const input = [];
  const receipts = [];
  const dependencies = {
    changed() {},
    queue(repoDir, request) {
      assert.equal(repoDir, dir);
      input.push(request);
      return String(input.length);
    },
    wait(repoDir, requestId) {
      assert.equal(repoDir, dir);
      assert.equal(requestId, String(input.length));
      return new Promise((resolve) => { receipts.push(resolve); });
    },
  };
  const worker = createCodexAnswerQueue(dependencies);
  const read = () => worker.status(dir, runId);
  const submit = (id = askId, answer = steps) => worker.submit(project, runId, id, answer, () => true);
  const move = async (receipt) => {
    receipts.shift()(receipt);
    await new Promise((resolve) => setImmediate(resolve));
  };
  return { dir, worker, project, input, read, submit, move, dependencies };
}

test("the next Codex answer step waits for the prior receipt and a duplicate cannot change its keys", async (t) => {
  const { input, read, submit, move, dir, dependencies } = fixture(t);
  assert.equal(submit().status, "queued");
  assert.deepEqual(read(), { askId, status: "queued" });
  assert.deepEqual(input, [{ ...steps[0], taskId: runId }]);
  assert.equal(submit(askId, [{ kind: "keys", instruction: "enter" }]).status, "queued");
  assert.throws(() => submit("2-12345678"), /already queued/);
  assert.equal(input.length, 1);

  await move({ status: "delivered" });
  assert.deepEqual(input, steps.map((step) => ({ ...step, taskId: runId })));
  assert.equal(read().status, "queued");
  const interrupted = createCodexAnswerQueue(dependencies);
  assert.equal(interrupted.status(dir, runId).status, "unknown");
  assert.equal(input.length, 2);
  await move({ status: "delivered" });
  assert.equal(read().status, "delivered");
  assert.equal(submit().status, "delivered");
  assert.equal(input.length, 2);
  const restarted = createCodexAnswerQueue(dependencies);
  assert.deepEqual(restarted.status(dir, runId), { askId, status: "delivered" });
});

test("a failed or unconfirmed earlier step does not send later steps or reopen the same sequence", async (t) => {
  for (const [receipt, expected] of [[{ status: "failed", error: "Pane is closed" }, "failed"], [null, "unknown"]]) {
    const { dir, project, input, read, submit, move, dependencies } = fixture(t);
    submit();
    const restarted = createCodexAnswerQueue(dependencies);
    assert.equal(restarted.status(dir, runId).status, "unknown");
    assert.equal(restarted.submit(project, runId, askId, steps, () => true).status, "unknown");
    await move(receipt);
    assert.equal(read().status, expected);
    assert.equal(submit().status, expected);
    assert.equal(input.length, 1);
  }
});

test("a terminal switch fails the remaining answer after a confirmed step", async (t) => {
  const { worker, project, input, read, move } = fixture(t);
  let current = true;
  worker.submit(project, runId, askId, steps, () => current);
  current = false;
  await move({ status: "delivered" });
  assert.match(read().error, /terminal changed/);
  assert.equal(read().status, "failed");
  assert.equal(input.length, 1);
});
