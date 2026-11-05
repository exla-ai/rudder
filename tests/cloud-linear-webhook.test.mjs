import assert from "node:assert/strict";
import {
  BASE_ENV,
  LINEAR_ORG,
  WEBHOOK_SECRET,
  assertNoSecrets,
  connectLinear,
  issuePayload,
  seedIssue,
  sendWebhook,
  setup,
  signWebhook,
  test,
} from "./helpers/cloud-linear.mjs";

async function linked(ctx, account = "alice", org) {
  await connectLinear(ctx, account, org);
  for (const [linearId, workspace] of [["lp-libra", "ws-libra"], ["lp-rudder", "ws-rudder"]]) {
    const res = await ctx.call("POST", "/api/app/linear/mapping", { account, body: { ...(org ? { org } : {}), kind: "project", linearId, workspace } });
    assert.equal(res.status, 200, JSON.stringify(res.data));
  }
}

test("webhook: signature, secret and timestamp checks", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    seedIssue(ctx, { id: "iss-100", identifier: "ENG-100", title: "Add retry" });
    const payload = issuePayload(ctx, {});
    const body = JSON.stringify(payload);
    assert.equal((await sendWebhook(ctx, payload, { signature: "deadbeef" })).status, 401);
    assert.equal((await sendWebhook(ctx, payload, { signature: signWebhook(body, "wrong-secret") })).status, 401);
    assert.equal((await sendWebhook(ctx, payload, { signature: "zz-not-hex" })).status, 401);
    assert.equal((await sendWebhook(ctx, payload, { signature: signWebhook(`${body} `) })).status, 401, "the signature covers the raw body");
    const stale = issuePayload(ctx, {}, { webhookTimestamp: ctx.clock.t - 61_000 });
    assert.equal((await sendWebhook(ctx, stale)).status, 401);
    const future = issuePayload(ctx, {}, { webhookTimestamp: ctx.clock.t + 61_000 });
    assert.equal((await sendWebhook(ctx, future)).status, 401);
    assert.equal((await sendWebhook(ctx, null, { raw: "{nope", signature: "deadbeef" })).status, 401, "bad JSON with a bad signature is still a 401");
    assert.equal((await sendWebhook(ctx, null, { raw: "{nope", signature: signWebhook("{nope") })).status, 400);
    await ctx.integrations.idle();
    assert.equal(ctx.delivered.length, 0);

    const ok = await sendWebhook(ctx, payload);
    assert.equal(ok.status, 200);
    await ctx.integrations.idle();
    assert.equal(ctx.delivered.length, 1);
    assertNoSecrets(ctx.logs);
  } finally {
    await ctx.close();
  }
  const noSecret = await setup({ env: { ...BASE_ENV, RUDDER_LINEAR_WEBHOOK_SECRET: "" } });
  try {
    const payload = issuePayload(noSecret, {});
    const res = await sendWebhook(noSecret, payload, { signature: signWebhook(JSON.stringify(payload), WEBHOOK_SECRET) });
    assert.equal(res.status, 503);
  } finally {
    await noSecret.close();
  }
});

test("webhook: a rudder-labeled issue starts once in the Rudder project its Linear project maps to", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    seedIssue(ctx, { id: "iss-100", identifier: "ENG-100", title: "Add retry", description: "Uploads fail." });
    const payload = issuePayload(ctx, {});
    assert.equal((await sendWebhook(ctx, payload)).status, 200);
    assert.equal((await sendWebhook(ctx, payload)).status, 200, "Linear retries are fine");
    assert.equal((await sendWebhook(ctx, issuePayload(ctx, { title: "Add retry (edited)" }, { action: "update" }))).status, 200);
    await ctx.integrations.idle();
    assert.deepEqual(ctx.delivered, [{
      accountId: "alice",
      workspaceId: "ws-rudder",
      text: "Linear ENG-100: Add retry\n\nUploads fail.\n\nhttps://linear.app/acme/issue/ENG-100",
    }]);
    assert.equal(ctx.upstream.linearState.comments.length, 0, "the start comment waits for the run");
    const row = ctx.database.prepare("select * from rudder_linear_issue_runs where issue_id = 'iss-100'").get();
    assert.equal(row.phase, "delivered");
    assert.equal(row.identifier, "ENG-100");
    assert.ok(ctx.events.some((e) => e.name === "linear_issue_started" && e.props.source === "webhook"));

    // Routed by the Linear project, not the team: a Libra issue goes to libra.
    seedIssue(ctx, { id: "iss-101", identifier: "ENG-101", title: "Libra fix" });
    await sendWebhook(ctx, issuePayload(ctx, { id: "iss-101", identifier: "ENG-101", projectId: "lp-libra" }));
    await ctx.integrations.idle();
    assert.equal(ctx.delivered.at(-1).workspaceId, "ws-libra");
  } finally {
    await ctx.close();
  }
});

test("webhook: unlabeled, unmapped, done, other-org and non-issue events are ignored with 200", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    for (const id of ["iss-1", "iss-2", "iss-3", "iss-4", "iss-5", "iss-6", "iss-7"]) {
      seedIssue(ctx, { id, identifier: `ENG-${id.slice(4)}`, title: id });
    }
    const cases = [
      issuePayload(ctx, { id: "iss-1", labels: [{ id: "l2", name: "bug" }], labelIds: ["l2"] }),
      issuePayload(ctx, { id: "iss-2", projectId: "lp-offstage" }),
      issuePayload(ctx, { id: "iss-3", projectId: null, teamId: "team-ops" }),
      issuePayload(ctx, { id: "iss-4", state: { type: "completed" } }),
      issuePayload(ctx, { id: "iss-5" }, { organizationId: "org-someone-else" }),
      issuePayload(ctx, { id: "iss-6" }, { type: "Comment" }),
      issuePayload(ctx, { id: "iss-7" }, { action: "remove" }),
    ];
    for (const payload of cases) {
      assert.equal((await sendWebhook(ctx, payload)).status, 200);
    }
    await ctx.integrations.idle();
    assert.deepEqual(ctx.delivered, []);
    assert.equal(ctx.database.prepare("select count(*) as n from rudder_linear_issue_runs").get().n, 0);
  } finally {
    await ctx.close();
  }
});

test("webhook: team mapping is the fallback; label ids alone are checked through the API", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    const team = await ctx.call("POST", "/api/app/linear/mapping", { account: "alice", body: { kind: "team", linearId: "team-ops", workspace: "ws-offstage" } });
    assert.equal(team.status, 200, JSON.stringify(team.data));
    seedIssue(ctx, { id: "iss-200", identifier: "OPS-200", title: "Ops thing" });
    seedIssue(ctx, { id: "iss-201", identifier: "OPS-201", title: "Not ours" });
    ctx.upstream.linearState.labelNames.set("iss-200", ["Rudder"]);
    ctx.upstream.linearState.labelNames.set("iss-201", ["bug"]);
    await sendWebhook(ctx, issuePayload(ctx, { id: "iss-200", projectId: null, teamId: "team-ops", labels: undefined, labelIds: ["lab-x"] }));
    await sendWebhook(ctx, issuePayload(ctx, { id: "iss-201", projectId: null, teamId: "team-ops", labels: undefined, labelIds: ["lab-y"] }));
    await ctx.integrations.idle();
    assert.deepEqual(ctx.delivered.map((d) => [d.workspaceId, d.text.split("\n")[0]]), [["ws-offstage", "Linear OPS-200: Ops thing"]]);
  } finally {
    await ctx.close();
  }
});

test("webhook: an install connected before webhooks learns its Linear workspace id from the setup screen", async () => {
  const ctx = await setup();
  try {
    await linked(ctx, "alice", "org1");
    ctx.database.prepare("update rudder_integrations set team_id = null where provider = 'linear'").run();
    seedIssue(ctx, { id: "iss-300", identifier: "ENG-300", title: "Old install" });
    await sendWebhook(ctx, issuePayload(ctx, { id: "iss-300" }));
    await ctx.integrations.idle();
    assert.equal(ctx.delivered.length, 0, "no workspace id, no install");
    await ctx.call("GET", "/api/app/linear/setup?org=org1", { account: "bob" });
    assert.equal(ctx.database.prepare("select team_id from rudder_integrations where provider = 'linear'").get().team_id, LINEAR_ORG);
    await sendWebhook(ctx, issuePayload(ctx, { id: "iss-300" }));
    await ctx.integrations.idle();
    assert.deepEqual(ctx.delivered.map((d) => [d.accountId, d.workspaceId]), [["alice", "ws-rudder"]], "an org link runs on the home of the admin who set it");
  } finally {
    await ctx.close();
  }
});

test("sync: run start moves the issue to started with one comment, needs-you comments once, merge completes it", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    seedIssue(ctx, { id: "iss-100", identifier: "ENG-100", title: "Add retry", stateType: "unstarted" });
    await sendWebhook(ctx, issuePayload(ctx, {}));
    await ctx.integrations.idle();
    const created = ctx.clock.t;
    const state = (agents) => [
      { workspaceId: "ws-libra", name: "libra", agents: [] },
      { workspaceId: "ws-rudder", name: "exla-ai-rudder", agents },
    ];
    const agent = (status, extra = {}) => ({ id: "run-1", title: "Add retry", task: "Linear ENG-100: Add retry\n\nhttps://linear.app/acme/issue/ENG-100", status, createdAt: created + 500, ...extra });
    const { stateUpdates, comments } = ctx.upstream.linearState;

    // Not yet a run: a pending row, an older run for the same issue, an unrelated one.
    await ctx.integrations.onHomeState("alice", state([
      { id: "pending-1", task: "Linear ENG-100: Add retry", status: "starting", pending: true },
      { id: "old-run", task: "Linear ENG-100: Add retry", status: "done", createdAt: created - 10 * 60_000 },
      { id: "other", task: "Linear ENG-1000: Something else", status: "working", createdAt: created + 100 },
    ]));
    await ctx.integrations.idle();
    assert.equal(stateUpdates.length, 0);

    await ctx.integrations.onHomeState("alice", state([agent("working")]));
    await ctx.integrations.onHomeState("alice", state([agent("working")]));
    await ctx.integrations.idle();
    assert.deepEqual(stateUpdates, [{ id: "iss-100", stateId: "st-progress" }], "the team's first started state by position");
    assert.equal(comments.length, 1);
    assert.equal(comments[0].issueId, "iss-100");
    assert.equal(comments[0].body, "Rudder started an agent on this in exla-ai-rudder.\n\nhttps://cloud.test/app/p/ws-rudder/a/run-1");
    assert.equal(ctx.database.prepare("select run_id, phase from rudder_linear_issue_runs").get().run_id, "run-1");

    await ctx.integrations.onHomeState("alice", state([agent("waiting", { attention: "permission", needsYou: true, reason: "permission" })]));
    await ctx.integrations.onHomeState("alice", state([agent("working")]));
    await ctx.integrations.onHomeState("alice", state([agent("waiting", { needsYou: true, reason: "question" })]));
    await ctx.integrations.idle();
    assert.equal(comments.length, 2, "needs-you comments once");
    assert.equal(comments[1].body, "The Rudder agent on this needs your permission.\n\nhttps://cloud.test/app/p/ws-rudder/a/run-1");

    await ctx.integrations.onHomeState("alice", state([agent("review", { needsYou: true, reason: "review" })]));
    await ctx.integrations.idle();
    assert.equal(stateUpdates.length, 1, "review is not merged");
    await ctx.integrations.onHomeState("alice", state([agent("merged")]));
    await ctx.integrations.onHomeState("alice", state([agent("merged")]));
    await ctx.integrations.idle();
    assert.deepEqual(stateUpdates.at(-1), { id: "iss-100", stateId: "st-done" });
    assert.equal(stateUpdates.length, 2);
    assert.equal(ctx.database.prepare("select phase from rudder_linear_issue_runs").get().phase, "merged");
    // A merged row is no longer watched.
    await ctx.integrations.onHomeState("alice", state([agent("waiting", { needsYou: true, reason: "input" })]));
    await ctx.integrations.idle();
    assert.equal(comments.length, 2);
  } finally {
    await ctx.close();
  }
});

test("sync: a done PR state counts as merged; an issue already in progress is not moved back", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    seedIssue(ctx, { id: "iss-100", identifier: "ENG-100", title: "Add retry", stateType: "started" });
    await sendWebhook(ctx, issuePayload(ctx, { state: { type: "started" } }));
    await ctx.integrations.idle();
    const state = (agent) => [{ workspaceId: "ws-rudder", name: "exla-ai-rudder", agents: [agent] }];
    await ctx.integrations.onHomeState("alice", state({ id: "run-9", task: "Linear ENG-100: Add retry", status: "working" }));
    await ctx.integrations.idle();
    assert.equal(ctx.upstream.linearState.stateUpdates.length, 0, "already started: left alone");
    assert.equal(ctx.upstream.linearState.comments.length, 1);
    await ctx.integrations.onHomeState("alice", state({ id: "run-9", task: "Linear ENG-100: Add retry", status: "done", pr: { state: "merged" } }));
    await ctx.integrations.idle();
    assert.deepEqual(ctx.upstream.linearState.stateUpdates, [{ id: "iss-100", stateId: "st-done" }]);
  } finally {
    await ctx.close();
  }
});

test("sync: a start from the app is remembered (no webhook double start, no second start comment)", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    seedIssue(ctx, { id: "iss-100", identifier: "ENG-100", title: "Add retry" });
    const started = await ctx.call("POST", "/api/app/linear/start", { account: "alice", body: { issueId: "iss-100", workspace: "ws-rudder" } });
    assert.equal(started.status, 200, JSON.stringify(started.data));
    await sendWebhook(ctx, issuePayload(ctx, {}, { action: "update" }));
    await ctx.integrations.idle();
    assert.equal(ctx.delivered.length, 1);
    assert.equal(ctx.upstream.linearState.comments.length, 1);
    await ctx.integrations.onHomeState("alice", [{ workspaceId: "ws-rudder", name: "exla-ai-rudder", agents: [{ id: "run-2", task: "Linear ENG-100: Add retry", status: "working" }] }]);
    await ctx.integrations.idle();
    assert.deepEqual(ctx.upstream.linearState.stateUpdates, [{ id: "iss-100", stateId: "st-progress" }]);
    assert.equal(ctx.upstream.linearState.comments.length, 1);
  } finally {
    await ctx.close();
  }
});

test("sync and webhook failures are logged, never thrown, and a failed start can be retried", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    // The issue lookup fails: nothing delivered, the claim is released.
    await sendWebhook(ctx, issuePayload(ctx, { id: "iss-missing" }));
    await ctx.integrations.idle();
    assert.equal(ctx.delivered.length, 0);
    assert.equal(ctx.database.prepare("select count(*) as n from rudder_linear_issue_runs").get().n, 0);
    assert.ok(ctx.logs.some((line) => /linear webhook start failed/.test(line)));
    seedIssue(ctx, { id: "iss-missing", identifier: "ENG-404", title: "Now it exists" });
    await sendWebhook(ctx, issuePayload(ctx, { id: "iss-missing" }, { action: "update" }));
    await ctx.integrations.idle();
    assert.equal(ctx.delivered.length, 1);

    // Linear down during sync: logged, the run is still remembered, Slack-side
    // onHomeState still resolves.
    ctx.upstream.handlers.linearDown = true;
    await ctx.integrations.onHomeState("alice", [{ workspaceId: "ws-rudder", name: "exla-ai-rudder", agents: [{ id: "run-3", task: "Linear ENG-404: Now it exists", status: "working" }] }]);
    await ctx.integrations.idle();
    assert.equal(ctx.database.prepare("select run_id from rudder_linear_issue_runs").get().run_id, "run-3");
    assert.ok(ctx.logs.some((line) => /linear move to started failed: network down/.test(line)));
    assert.ok(ctx.logs.some((line) => /linear start comment failed: network down/.test(line)));
    assertNoSecrets(ctx.logs);
  } finally {
    await ctx.close();
  }
});
