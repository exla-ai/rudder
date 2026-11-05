// Rudder tracks your work in Linear: new agents in a linked project are
// decided once (attach / create / skip) and synced quietly. Only the fake
// Linear and OpenAI upstreams in helpers/cloud-linear.mjs are ever called.
import assert from "node:assert/strict";
import {
  BASE_ENV,
  assertNoSecrets,
  connectLinear,
  issuePayload,
  json,
  seedIssue,
  sendWebhook,
  setup,
  test,
  trackDecision,
} from "./helpers/cloud-linear.mjs";

const AGENT_URL = (run) => `https://cloud.test/app/p/ws-rudder/a/${run}`;

async function linked(ctx, account = "alice", org) {
  await connectLinear(ctx, account, org);
  for (const [linearId, workspace] of [["lp-libra", "ws-libra"], ["lp-rudder", "ws-rudder"]]) {
    const res = await ctx.call("POST", "/api/app/linear/mapping", { account, body: { ...(org ? { org } : {}), kind: "project", linearId, workspace } });
    assert.equal(res.status, 200, JSON.stringify(res.data));
  }
}

// The user's real open issue in the Rudder Linear project (team OPS).
function seedBareMetal(ctx, extra = {}) {
  seedIssue(ctx, { id: "iss-bm", identifier: "OPS-50", title: "Move Rudder to bare metal", projectId: "lp-rudder", teamId: "team-ops", stateType: "unstarted", ...extra });
}

const rudderState = (agents) => [
  { workspaceId: "ws-libra", name: "libra", repo: "exla-ai/libra", agents: [] },
  { workspaceId: "ws-rudder", name: "exla-ai-rudder", repo: "exla-ai/rudder", agents },
];

const agentAt = (ctx) => (id, task, extra = {}) => ({
  id,
  title: task.split("\n")[0].slice(0, 56),
  task,
  status: "working",
  mode: "execute",
  createdAt: ctx.clock.t + 1000,
  ...extra,
});

const classifyCalls = (ctx) =>
  ctx.upstream.calls.filter((c) => c.url === "https://api.openai.com/v1/responses" && JSON.parse(c.body).text.format.name === "linear_track");
const linearWrites = (ctx) =>
  ctx.upstream.calls.filter((c) => c.url === "https://api.linear.app/graphql" && /issueCreate|issueUpdate|commentCreate|issueLabelCreate/.test(JSON.parse(c.body).query));
const trackedRow = (ctx, run) => ctx.database.prepare("select * from rudder_linear_tracked_runs where run_id = ?").get(run);

test("track: attach moves a related open issue to started, with no comment", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    seedBareMetal(ctx);
    ctx.upstream.handlers.classify = () => trackDecision({ action: "attach", issue: "OPS-50", reason: "Provisioning hosts is part of the move." });
    const agent = agentAt(ctx);
    // Started from the task bar: Rudder's framing is stripped before the model sees it.
    const task = "Objective: Provision hosts\nDone when: hosts answer\n\nProvision the bare metal hosts with ansible";
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-1", task)]));
    await ctx.integrations.idle();

    const [call] = classifyCalls(ctx);
    const sent = JSON.parse(call.body);
    assert.equal(sent.model, "gpt-6-luna");
    assert.deepEqual(sent.reasoning, { effort: "none" });
    assert.equal(sent.text.format.strict, true);
    assert.deepEqual(sent.text.format.schema.required, ["action", "issue_identifier", "title", "reason"]);
    assert.deepEqual(sent.text.format.schema.properties.action.enum, ["attach", "create", "skip"]);
    assert.match(sent.instructions, /Prefer attaching/);
    assert.match(sent.instructions, /summarize X/);
    assert.match(sent.instructions, /never the prompt/);
    assert.match(sent.input, /Rudder project: exla-ai-rudder \(exla-ai\/rudder\)\nLinear project: Rudder/);
    assert.match(sent.input, /- OPS-50 \[Todo\] Move Rudder to bare metal/);
    assert.match(sent.input, /<agent_task>\nProvision the bare metal hosts with ansible\n<\/agent_task>/);

    const { stateUpdates, comments, created } = ctx.upstream.linearState;
    assert.deepEqual(stateUpdates, [{ id: "iss-bm", stateId: "st-progress" }], "the team's first started state");
    assert.equal(comments.length, 0, "no comment at start");
    assert.equal(created.length, 0);
    const row = trackedRow(ctx, "run-1");
    assert.equal(row.action, "attach");
    assert.equal(row.identifier, "OPS-50");
    assert.equal(row.issue_id, "iss-bm");
    assert.equal(row.created_issue, 0);
    assert.equal(row.started_synced, 1);

    // An issue already in progress is not moved again by a second attached agent.
    ctx.upstream.linearState.issues.get("iss-bm").stateType = "started";
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-1", task), agent("run-2", "Move the build cache to the new hosts")]));
    await ctx.integrations.idle();
    assert.equal(stateUpdates.length, 1);
    assert.equal(comments.length, 0);
  } finally {
    await ctx.close();
  }
});

test("track: create adds an unlabeled issue in the linked project and team, and nothing starts from it", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    seedBareMetal(ctx);
    ctx.upstream.handlers.classify = () => trackDecision({ action: "create", title: "Onboard Bild.", reason: "A customer onboarding." });
    const agent = agentAt(ctx);
    await ctx.integrations.onHomeState("alice", rudderState([
      agent("run-2", "Set up Bild's workspace: import their repos, configure SSO and invite their team.\n\nThey use Okta."),
    ]));
    await ctx.integrations.idle();

    const { created, stateUpdates, comments } = ctx.upstream.linearState;
    assert.equal(created.length, 1);
    assert.deepEqual(created[0], {
      teamId: "team-ops",
      projectId: "lp-rudder",
      title: "Onboard Bild",
      description: [
        "Set up Bild's workspace: import their repos, configure SSO and invite their team.",
        "Tracked from a Rudder agent in exla-ai-rudder.",
        AGENT_URL("run-2"),
      ].join("\n\n"),
    });
    assert.equal("labelIds" in created[0], false, "never the rudder label");
    assert.equal(ctx.upstream.calls.filter((c) => c.url === "https://api.linear.app/graphql" && JSON.parse(c.body).query.includes("issueLabel")).length, 0);
    const row = trackedRow(ctx, "run-2");
    assert.equal(row.action, "create");
    assert.equal(row.created_issue, 1);
    assert.match(row.identifier, /^OPS-\d+$/);
    assert.deepEqual(stateUpdates, [{ id: row.issue_id, stateId: "st-progress" }]);
    assert.equal(comments.length, 0);

    // Linear reports the new issue to the webhook: it has no rudder label, so no agent starts (no loop).
    await sendWebhook(ctx, issuePayload(ctx, { id: row.issue_id, identifier: row.identifier, teamId: "team-ops", labels: [], labelIds: [] }));
    await ctx.integrations.idle();
    assert.deepEqual(ctx.delivered, []);
  } finally {
    await ctx.close();
  }
});

test("track: skip writes nothing to Linear and is remembered", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    seedBareMetal(ctx);
    ctx.upstream.handlers.classify = () => trackDecision({ action: "skip", reason: "A summary changes nothing." });
    const agent = agentAt(ctx);
    const state = rudderState([agent("run-3", "Summarize the README")]);
    await ctx.integrations.onHomeState("alice", state);
    await ctx.integrations.idle();
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-3", "Summarize the README", { status: "merged" })]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 1);
    assert.deepEqual(linearWrites(ctx), []);
    const row = trackedRow(ctx, "run-3");
    assert.equal(row.action, "skip");
    assert.equal(row.reason, "A summary changes nothing.");
    assert.equal(row.issue_id, null);
  } finally {
    await ctx.close();
  }
});

test("track: one classification per run across repeated states and a restart; state handling never waits for the model", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    let release;
    const gate = new Promise((resolve) => { release = resolve; });
    ctx.upstream.handlers.classify = async () => {
      await gate;
      return trackDecision({ action: "create", title: "Add usage billing" });
    };
    const agent = agentAt(ctx);
    const state = rudderState([agent("run-4", "Bill customers for agent minutes with Stripe metered billing")]);
    // Each call returns while the model is still answering.
    await ctx.integrations.onHomeState("alice", state);
    await ctx.integrations.onHomeState("alice", state);
    await ctx.integrations.onHomeState("alice", state);
    assert.equal(trackedRow(ctx, "run-4").action, "deciding");
    release();
    await ctx.integrations.idle();
    await ctx.integrations.onHomeState("alice", state);
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 1);
    assert.equal(ctx.upstream.linearState.created.length, 1);

    const again = ctx.restart();
    await again.onHomeState("alice", state);
    await again.idle();
    assert.equal(classifyCalls(ctx).length, 1, "the decision is stored, not remembered in memory");
    assert.equal(ctx.upstream.linearState.created.length, 1);
    assert.equal(ctx.upstream.linearState.stateUpdates.length, 1);
  } finally {
    await ctx.close();
  }
});

test("track: decisions in one project run in order, so the second agent sees the issue the first created", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    const prompts = [];
    ctx.upstream.handlers.classify = (body) => {
      prompts.push(body.input);
      return prompts.length === 1
        ? trackDecision({ action: "create", title: "Add usage billing" })
        : trackDecision({ action: "attach", issue: "OPS-1" });
    };
    const agent = agentAt(ctx);
    await ctx.integrations.onHomeState("alice", rudderState([
      agent("run-5", "Bill customers for agent minutes with Stripe metered billing"),
      agent("run-6", "Show the billed minutes on the usage page"),
    ]));
    await ctx.integrations.idle();
    assert.equal(prompts.length, 2);
    assert.match(prompts[1], /Open issues:\n- OPS-1 \[started\] Add usage billing/);
    assert.match(prompts[1], /Agents already tracked in this project:\n- "Bill customers for agent minutes with Stripe metered bil" -> OPS-1/);
    assert.equal(ctx.upstream.linearState.created.length, 1, "one issue per piece of work, not one per agent");
    assert.equal(trackedRow(ctx, "run-6").identifier, "OPS-1");
    assert.equal(trackedRow(ctx, "run-6").created_issue, 0);
    // The project's open issues were read once (cached for five minutes).
    const reads = () => ctx.upstream.calls.filter((c) => c.url === "https://api.linear.app/graphql" && JSON.parse(c.body).query.includes("$filter: IssueFilter")).length;
    assert.equal(reads(), 1);
    ctx.clock.t += 6 * 60_000;
    ctx.upstream.handlers.classify = () => trackDecision({ action: "skip" });
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-7", "Rename the billing module")]));
    await ctx.integrations.idle();
    assert.equal(reads(), 2);
  } finally {
    await ctx.close();
  }
});

test("track: merge comments once; only an issue Rudder created is completed; needs-you and unmerged finishes stay quiet", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    seedBareMetal(ctx);
    ctx.upstream.handlers.classify = (body) => (body.input.split("<agent_task>")[1].includes("bare metal")
      ? trackDecision({ action: "attach", issue: "OPS-50" })
      : trackDecision({ action: "create", title: "Onboard Bild" }));
    const agent = agentAt(ctx);
    const metal = (extra) => agent("run-a", "Cut the control plane over to the bare metal hosts", extra);
    const bild = (extra) => agent("run-c", "Import Bild's repos and configure their SSO", extra);
    const finished = (extra) => agent("run-d", "Cut the billing service over to bare metal too", extra);
    await ctx.integrations.onHomeState("alice", rudderState([metal(), bild()]));
    await ctx.integrations.idle();
    const created = trackedRow(ctx, "run-c").issue_id;
    const { stateUpdates, comments } = ctx.upstream.linearState;
    assert.deepEqual(stateUpdates, [{ id: "iss-bm", stateId: "st-progress" }, { id: created, stateId: "st-progress" }]);

    await ctx.integrations.onHomeState("alice", rudderState([
      metal({ status: "waiting", needsYou: true, reason: "permission" }),
      bild({ status: "review", needsYou: true, reason: "review" }),
      finished({ status: "done", summary: "Done, not merged." }),
    ]));
    await ctx.integrations.idle();
    assert.equal(comments.length, 0, "no comment for needs you, review or a finish without a merge");

    const merged = () => rudderState([
      metal({ status: "merged", summary: "## Summary\nMoved the control plane to the new hosts.\n\nDetails follow." }),
      bild({ status: "done", pr: { state: "merged" }, summary: null, lastMessage: "**Onboarded Bild**: repos imported, SSO on. See [the doc](https://x.test)." }),
      finished({ status: "done", summary: "Done, not merged." }),
    ]);
    await ctx.integrations.onHomeState("alice", merged());
    await ctx.integrations.onHomeState("alice", merged());
    await ctx.integrations.idle();
    await ctx.integrations.onHomeState("alice", merged());
    await ctx.integrations.idle();
    assert.deepEqual(comments, [
      { issueId: "iss-bm", body: `Merged: Moved the control plane to the new hosts.\n\n${AGENT_URL("run-a")}` },
      { issueId: created, body: `Merged: Onboarded Bild: repos imported, SSO on. See the doc.\n\n${AGENT_URL("run-c")}` },
    ]);
    // Completed only for the issue Rudder created; the user's own issue stays open.
    assert.deepEqual(stateUpdates.slice(2), [{ id: created, stateId: "st-done" }]);
    assert.equal(trackedRow(ctx, "run-a").merged_synced, 1);
    assert.equal(trackedRow(ctx, "run-d").action, "attach");
    assert.equal(trackedRow(ctx, "run-d").merged_synced, 0);
  } finally {
    await ctx.close();
  }
});

test("track: a merge that lands while the model answers is still synced", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    let release;
    const gate = new Promise((resolve) => { release = resolve; });
    ctx.upstream.handlers.classify = async () => {
      await gate;
      return trackDecision({ action: "create", title: "Fix login on Safari" });
    };
    const agent = agentAt(ctx);
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-f", "Fix the Safari login redirect loop")]));
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-f", "Fix the Safari login redirect loop", { status: "merged", summary: "Fixed the redirect loop." })]));
    release();
    await ctx.integrations.idle();
    const id = trackedRow(ctx, "run-f").issue_id;
    assert.deepEqual(ctx.upstream.linearState.stateUpdates, [{ id, stateId: "st-progress" }, { id, stateId: "st-done" }]);
    assert.deepEqual(ctx.upstream.linearState.comments, [{ issueId: id, body: `Merged: Fixed the redirect loop.\n\n${AGENT_URL("run-f")}` }]);
  } finally {
    await ctx.close();
  }
});

test("track: runs started from Linear, swarm experiments, main and orchestrator rows are never classified", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    seedIssue(ctx, { id: "iss-100", identifier: "ENG-100", title: "Add retry" });
    await sendWebhook(ctx, issuePayload(ctx, {}));
    await ctx.integrations.idle();
    // An app start whose run id is mapped, even if its task text lost the prefix.
    ctx.database.prepare("insert into rudder_linear_issue_runs (issue_id, identifier, owner_kind, owner_id, account_id, workspace_id, project_name, source, phase, run_id, created_at, updated_at) values ('iss-9', 'ENG-9', 'account', 'alice', 'alice', 'ws-rudder', 'exla-ai-rudder', 'app', 'running', 'run-mapped', ?, ?)").run(ctx.clock.t, ctx.clock.t);
    const agent = agentAt(ctx);
    await ctx.integrations.onHomeState("alice", rudderState([
      agent("run-lin", "Linear ENG-100: Add retry\n\nhttps://linear.app/acme/issue/ENG-100"),
      agent("run-mapped", "Add retry to uploads"),
      agent("run-swarm", "Try a momentum signal for the weather model", { swarmExperiment: true }),
      agent("__main__", "Read RUDDER.md", { mode: "main" }),
      agent("run-main", "Help me with this project", { mode: "main" }),
      agent("run-orch", "Plan the billing work", { mode: "rudder-plan" }),
      agent("run-ask", "What does the scheduler do?", { mode: "one-off" }),
      agent("run-gam", "Review the generator", { mode: "gam-adversarial" }),
      agent("pending-1", "Add retry to uploads", { pending: true }),
      agent("run-empty", "", { task: "" }),
    ]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 0);
    assert.equal(ctx.database.prepare("select count(*) as n from rudder_linear_tracked_runs").get().n, 0);
    assert.equal(ctx.upstream.linearState.created.length, 0);
  } finally {
    await ctx.close();
  }
});

test("track: runs that existed before tracking started, or before the project was linked, are never backfilled", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    ctx.upstream.handlers.classify = () => trackDecision({ action: "create", title: "Something new" });
    const agent = agentAt(ctx);
    const bootedAt = ctx.clock.t;
    await ctx.integrations.onHomeState("alice", rudderState([
      agent("run-old", "Build the importer", { createdAt: bootedAt - 60_000 }),
      agent("run-nodate", "Build the exporter", { createdAt: null }),
    ]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 0);

    // A restart later: the start time is stored, so a run made after the first
    // boot is new, and one made before it is still old.
    ctx.clock.t += 60 * 60_000;
    const again = ctx.restart();
    await again.onHomeState("alice", rudderState([
      agent("run-old", "Build the importer", { createdAt: bootedAt - 60_000 }),
      agent("run-mid", "Build the scheduler", { createdAt: bootedAt + 30 * 60_000 }),
    ]));
    await again.idle();
    assert.deepEqual(ctx.database.prepare("select run_id from rudder_linear_tracked_runs").all().map((r) => r.run_id), ["run-mid"]);

    // Offstage is linked an hour after boot: its earlier runs are not backfilled.
    const res = await ctx.call("POST", "/api/app/linear/mapping", { account: "alice", body: { kind: "project", linearId: "lp-offstage", workspace: "ws-offstage" } });
    assert.equal(res.status, 200);
    const offstage = (agents) => [{ workspaceId: "ws-offstage", name: "offstage", agents }];
    await again.onHomeState("alice", offstage([
      agent("off-old", "Do outbound for offstage", { createdAt: bootedAt + 10 * 60_000 }),
      agent("off-new", "Write the outbound sequence for offstage", { createdAt: ctx.clock.t + 1000 }),
    ]));
    await again.idle();
    assert.deepEqual(ctx.database.prepare("select run_id from rudder_linear_tracked_runs order by run_id").all().map((r) => r.run_id), ["off-new", "run-mid"]);
    assert.deepEqual(ctx.upstream.linearState.created.at(-1).projectId, "lp-offstage");
  } finally {
    await ctx.close();
  }
});

test("track: generated tasks are unlabeled; a rudder label the user adds still starts an agent", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    const res = await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: "Make uploads reliable", linearProjectId: "lp-rudder" } });
    assert.equal(res.status, 200, JSON.stringify(res.data));
    const created = ctx.upstream.linearState.created;
    assert.equal(created.length, 3);
    assert.ok(created.every((input) => input.labelIds === undefined), "no label on generated issues");
    const [first] = res.data.issues;
    await sendWebhook(ctx, issuePayload(ctx, { id: first.id, identifier: first.identifier, teamId: "team-ops", labels: [], labelIds: [] }));
    await ctx.integrations.idle();
    assert.deepEqual(ctx.delivered, [], "an unlabeled generated issue starts nothing");
    await sendWebhook(ctx, issuePayload(ctx, { id: first.id, identifier: first.identifier, teamId: "team-ops" }, { action: "update" }));
    await ctx.integrations.idle();
    assert.equal(ctx.delivered.length, 1);
    assert.match(ctx.delivered[0].text, new RegExp(`^Linear ${first.identifier}: `));
  } finally {
    await ctx.close();
  }
});

test("track: no model key logs once and tracks nothing; a model failure is a remembered skip", async () => {
  const keyless = await setup({ env: { ...BASE_ENV, OPENAI_API_KEY: "" } });
  try {
    await linked(keyless);
    const agent = agentAt(keyless);
    await keyless.integrations.onHomeState("alice", rudderState([agent("run-k1", "Build the importer")]));
    await keyless.integrations.onHomeState("alice", rudderState([agent("run-k1", "Build the importer"), agent("run-k2", "Build the exporter")]));
    await keyless.integrations.idle();
    assert.equal(keyless.logs.filter((line) => /linear tracking is off/.test(line)).length, 1);
    assert.equal(keyless.database.prepare("select count(*) as n from rudder_linear_tracked_runs").get().n, 0);
    assert.deepEqual(linearWrites(keyless), []);
  } finally {
    await keyless.close();
  }

  const ctx = await setup();
  try {
    await linked(ctx);
    ctx.upstream.handlers.classify = () => json({ error: { message: "overloaded" } }, 500);
    const agent = agentAt(ctx);
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-x", "Build the importer")]));
    await ctx.integrations.idle();
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-x", "Build the importer")]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 1);
    assert.equal(trackedRow(ctx, "run-x").action, "skip");
    assert.equal(ctx.logs.filter((line) => /linear tracking skipped run-x/.test(line)).length, 1);
    assert.deepEqual(linearWrites(ctx), []);

    // An attach naming an issue that is not open is a skip too.
    ctx.upstream.handlers.classify = () => trackDecision({ action: "attach", issue: "OPS-999" });
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-y", "Build the exporter")]));
    await ctx.integrations.idle();
    assert.equal(trackedRow(ctx, "run-y").action, "skip");
    assert.deepEqual(linearWrites(ctx), []);

    // Linear down while syncing: logged, never thrown.
    ctx.upstream.handlers.classify = () => trackDecision({ action: "create", title: "Build the scheduler" });
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-z", "Build the scheduler")]));
    await ctx.integrations.idle();
    ctx.upstream.handlers.linearDown = true;
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-z", "Build the scheduler", { status: "merged", summary: "Built it." })]));
    await ctx.integrations.idle();
    assert.ok(ctx.logs.some((line) => /linear merge comment failed: network down/.test(line)));
    assertNoSecrets(ctx.logs);
  } finally {
    await ctx.close();
  }
});

test("track: a private agent stays out of an org's Linear; a personal install tracks it", async () => {
  const ctx = await setup();
  try {
    await linked(ctx, "alice", "org1");
    ctx.upstream.handlers.classify = () => trackDecision({ action: "create", title: "Onboard Bild" });
    const agent = agentAt(ctx);
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-p", "Onboard Bild", { visibility: "private" })]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 0);
    await ctx.integrations.onHomeState("alice", rudderState([agent("run-t", "Onboard Bild", { visibility: "team" })]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 1);
    assert.equal(trackedRow(ctx, "run-t").owner_kind, "org");
  } finally {
    await ctx.close();
  }
  const personal = await setup();
  try {
    await linked(personal);
    personal.upstream.handlers.classify = () => trackDecision({ action: "create", title: "Onboard Bild" });
    await personal.integrations.onHomeState("alice", rudderState([agentAt(personal)("run-p", "Onboard Bild", { visibility: "private" })]));
    await personal.integrations.idle();
    assert.equal(classifyCalls(personal).length, 1);
  } finally {
    await personal.close();
  }
});
