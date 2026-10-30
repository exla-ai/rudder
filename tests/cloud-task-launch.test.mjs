import assert from "node:assert/strict";
import test from "node:test";
import { createWorkerLaunchResolver } from "../cloud/worker/task-launch.mjs";
import { classifyTaskRoute } from "../dist/task-routing.js";

const deterministic = () => ({ classifyTaskRoute });
const decision = (route) => ({ route, source: "deterministic", reason: "test routing decision" });

test("finite execute sails keep their chosen backend and original task", async () => {
  for (const backend of ["claude", "codex", "acpx"]) {
    const task = "Fix the login bug, but do not change the public API";
    const launch = await createWorkerLaunchResolver({ task, backend, env: {}, loadRouting: deterministic })();
    assert.equal(launch.command, "rudder");
    assert.deepEqual(launch.args, [backend, "--worktree", task]);
    assert.equal(launch.route.route, "execute");
  }
});

test("legacy sails fail closed for discussion and planning before an implementation worker can start", async () => {
  for (const task of [
    "Let's just discuss the architecture; do not implement yet",
    "Do not edit any files; inspect the caching configuration",
    "Write an implementation plan for the migration",
    "Implement the API and UI in parallel",
    "Rewrite the whole workspace architecture",
  ]) {
    const resolve = createWorkerLaunchResolver({ task, backend: "claude", env: {}, loadRouting: deterministic });
    await assert.rejects(resolve(), (error) => {
      assert.match(error.message, /No implementation worker was started/);
      assert.match(error.message, /Cloud home workspace using the web or phone app/);
      return true;
    }, task);
  }
});

test("workspace migration and no-task handoff keep the existing dashboard without classifying again", async () => {
  const unavailable = () => { assert.fail("existing sessions must not need a task classifier"); };
  for (const options of [
    { isWorkspaceMode: true, task: "Do not edit files; inspect the current work" },
    { isWorkspaceMode: true, task: "Implement a multi-agent rewrite" },
    { isWorkspaceMode: false, task: "" },
  ]) {
    const launch = await createWorkerLaunchResolver({ ...options, backend: "codex", loadRouting: unavailable })();
    assert.deepEqual(launch, { command: "rudder", args: [], route: null });
  }
});

test("one supervisor startup classifies once across concurrent calls and reconnects", async () => {
  let imports = 0;
  let classifications = 0;
  const env = { RUDDER_TASK_ROUTING: "decisions" };
  const resolve = createWorkerLaunchResolver({
    task: "fix the sidebar typo", backend: "codex", env,
    loadRouting: async () => {
      imports += 1;
      return { classifyTaskRoute: async (task, options) => {
        classifications += 1;
        assert.equal(task, "fix the sidebar typo");
        assert.equal(options.env, env);
        return decision("execute");
      } };
    },
  });
  const first = resolve();
  assert.equal(resolve(), first);
  const launch = await first;
  assert.equal(await resolve(), launch);
  assert.equal(imports, 1);
  assert.equal(classifications, 1);
});

test("missing or failed classifier cannot become an uncertain execute request or leak its error", async () => {
  for (const loadRouting of [
    async () => { throw new Error("secret-from-upstream-error"); },
    async () => ({}),
    async () => ({ classifyTaskRoute: async () => { throw new Error("secret-from-upstream-error"); } }),
  ]) {
    const resolve = createWorkerLaunchResolver({ task: "read the design", backend: "claude", loadRouting });
    await assert.rejects(resolve(), (error) => {
      assert.match(error.message, /could not load or run the packaged task router/);
      assert.match(error.message, /No implementation worker was started/);
      assert.ok(!error.message.includes("secret-from-upstream-error"));
      return true;
    });
    await assert.rejects(resolve(), /No implementation worker was started/, "failure is retained rather than reclassified on reconnect");
  }
});

test("malformed and unknown decisions fail closed", async () => {
  for (const result of [null, [], {}, { route: "execute" }, decision("deploy"),
    { ...decision("execute"), source: "untrusted" },
    { ...decision("execute"), reason: "bad\nreason" },
    { ...decision("execute"), reason: "x".repeat(257) }]) {
    const resolve = createWorkerLaunchResolver({ task: "fix a typo", backend: "claude",
      loadRouting: async () => ({ classifyTaskRoute: async () => result }) });
    await assert.rejects(resolve(), /invalid task-routing result/);
  }
});
