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

const rudderState = (agents, plan = null) => [
  { workspaceId: "ws-libra", name: "libra", repo: "exla-ai/libra", agents: [] },
  { workspaceId: "ws-rudder", name: "exla-ai-rudder", repo: "exla-ai/rudder", agents, plan },
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
    assert.match(sent.instructions, /skip only when the task is not work/);
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

// ---- the classifier leans toward creating ----

test("track: a small real fix creates its own issue; help and summarize the README still skip", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    ctx.upstream.handlers.classify = (body) => (body.input.split("<agent_task>")[1].includes("typo")
      ? trackDecision({ action: "create", title: "Fix the signup button typo" })
      : trackDecision({ action: "skip", reason: "A read-only request." }));
    const agent = agentAt(ctx);
    await ctx.integrations.onHomeState("alice", rudderState([
      agent("run-typo", "fix the typo in the signup button"),
      agent("run-help", "help"),
      agent("run-readme", "summarize the README"),
    ]));
    await ctx.integrations.idle();
    const [call] = classifyCalls(ctx);
    const instructions = JSON.parse(call.body).instructions;
    assert.match(instructions, /Small fixes count: a small fix gets its own small issue/);
    assert.match(instructions, /create for any other real work: changing code, config, docs or infrastructure/);
    assert.match(instructions, /skip only when the task is not work: a question, an explanation, chat or a request for help, an empty or one-word prompt/);
    assert.match(instructions, /"summarize the README"/);
    assert.equal(classifyCalls(ctx).length, 2, "a one-word prompt never reaches the model");
    assert.deepEqual(ctx.upstream.linearState.created.map((input) => input.title), ["Fix the signup button typo"]);
    assert.equal(trackedRow(ctx, "run-typo").action, "create");
    assert.equal(trackedRow(ctx, "run-help").action, "skip");
    assert.match(trackedRow(ctx, "run-help").reason, /one-word/);
    assert.equal(trackedRow(ctx, "run-readme").action, "skip");
  } finally {
    await ctx.close();
  }
});

// ---- plans: one issue per plan ----

const PLAN_GOAL = "Harden the cloud home: retries, health checks and alerting";
const workerTask = (node, title, goal = PLAN_GOAL) =>
  `This task was spawned by Rudder from a /rudder-plan coordinator.\n\nWorker node: ${node}\n\nOriginal request:\n${goal}\n\nWorker task: ${title}\n\nImplement ${title.toLowerCase()}.`;
const planRowOf = (ctx, planId) => ctx.database.prepare("select * from rudder_linear_tracked_plans where plan_id = ?").get(planId);

test("track plans: one decision from the plan's goal, every worker attaches, and the issue closes when the last worker merges", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    const prompts = [];
    ctx.upstream.handlers.classify = (body) => {
      prompts.push(body.input);
      return trackDecision({ action: "create", title: "Harden the cloud home" });
    };
    const agent = agentAt(ctx);
    const orchestrator = agent("run-orch", PLAN_GOAL, { mode: "rudder-plan", planId: "plan-a" });
    const worker = (id, node, title, extra) => agent(id, workerTask(node, title), { planId: "plan-a", nodeId: node, ...extra });
    const plan = (counts) => ({ nodes: [], plans: [{ id: "plan-a", request: PLAN_GOAL, summary: null, ...counts }] });
    const workers = (done = {}) => [
      worker("w-1", "harden-10", "Retry home reconnects", done["w-1"]),
      worker("w-2", "harden-11", "Add health checks", done["w-2"]),
      worker("w-3", "harden-12", "Alert on failed homes", done["w-3"]),
    ];
    await ctx.integrations.onHomeState("alice", rudderState([orchestrator, ...workers()], plan({ pending: 1, launched: 3, merged: 0 })));
    await ctx.integrations.idle();

    assert.equal(classifyCalls(ctx).length, 1, "one model call for the whole plan");
    assert.match(prompts[0], /This is a Rudder plan/);
    assert.match(prompts[0], /Pieces:\n- Retry home reconnects\n- Add health checks\n- Alert on failed homes/);
    assert.match(prompts[0], /<plan_goal>\nHarden the cloud home: retries, health checks and alerting\n<\/plan_goal>/);
    assert.doesNotMatch(prompts[0], /spawned by Rudder/, "the model sees the goal, not the worker framing");
    const { created, stateUpdates, comments } = ctx.upstream.linearState;
    assert.equal(created.length, 1);
    assert.equal(created[0].title, "Harden the cloud home");
    assert.equal(created[0].labelIds, undefined);
    assert.match(created[0].description, /Tracked from a Rudder plan in exla-ai-rudder\.\n\nhttps:\/\/cloud\.test\/app\/p\/ws-rudder\/a\/run-orch$/);
    const planRow = planRowOf(ctx, "plan-a");
    assert.equal(planRow.action, "create");
    assert.equal(planRow.created_issue, 1);
    for (const id of ["w-1", "w-2", "w-3"]) {
      const row = trackedRow(ctx, id);
      assert.deepEqual([row.action, row.issue_id, row.plan_id, row.created_issue], ["attach", planRow.issue_id, "plan-a", 0], id);
    }
    assert.equal(trackedRow(ctx, "run-orch"), undefined, "the orchestrator row is the plan itself");
    assert.deepEqual(stateUpdates, [{ id: planRow.issue_id, stateId: "st-progress" }], "started once for the plan");

    // A worker launched later joins the same issue without a model call.
    const later = (extra) => worker("w-4", "harden-13", "Document the runbook", extra);
    await ctx.integrations.onHomeState("alice", rudderState([orchestrator, ...workers(), later()], plan({ pending: 0, launched: 4, merged: 0 })));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 1);
    assert.equal(trackedRow(ctx, "w-4").issue_id, planRow.issue_id);

    // One comment per worker merge; the plan's issue stays open until the last one.
    const merged = { status: "merged", summary: "Merged this piece." };
    await ctx.integrations.onHomeState("alice", rudderState([orchestrator, ...workers({ "w-1": merged, "w-2": merged }), later()], plan({ pending: 0, launched: 4, merged: 2 })));
    await ctx.integrations.idle();
    assert.equal(comments.length, 2);
    assert.equal(stateUpdates.length, 1, "not completed on the first merge");
    const allMerged = () => rudderState([orchestrator, ...workers({ "w-1": merged, "w-2": merged, "w-3": merged }), later(merged)], plan({ pending: 0, launched: 4, merged: 4 }));
    await ctx.integrations.onHomeState("alice", allMerged());
    await ctx.integrations.onHomeState("alice", allMerged());
    await ctx.integrations.idle();
    assert.equal(comments.length, 4);
    assert.ok(comments.every((comment) => comment.issueId === planRow.issue_id && comment.body.startsWith("Merged: Merged this piece.")));
    assert.deepEqual(stateUpdates.slice(1), [{ id: planRow.issue_id, stateId: "st-done" }], "completed once, after the last merge");
    assert.equal(planRowOf(ctx, "plan-a").closed, 1);
  } finally {
    await ctx.close();
  }
});

test("track plans: at most 10 merge comments per issue; without counts from an older home, the issue closes once every worker merged and nothing waits to launch", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    ctx.upstream.handlers.classify = () => trackDecision({ action: "create", title: "Harden the cloud home" });
    const agent = agentAt(ctx);
    const ids = Array.from({ length: 12 }, (_, i) => `w-${i + 1}`);
    const workers = (extra) => ids.map((id, i) => agent(id, workerTask(`n${i}`, `Piece ${i + 1} of the hardening`), { planId: "plan-b", nodeId: `n${i}`, ...extra }));
    const waiting = { nodes: [{ id: "n12", title: "Last piece", status: "planned", runId: null }], plans: [] };
    await ctx.integrations.onHomeState("alice", rudderState(workers(), waiting));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 1);
    const issueId = planRowOf(ctx, "plan-b").issue_id;
    assert.equal(ctx.database.prepare("select count(*) as n from rudder_linear_tracked_runs where plan_id = 'plan-b' and issue_id = ?").get(issueId).n, 12);

    const merged = { status: "merged", summary: "Merged this piece." };
    await ctx.integrations.onHomeState("alice", rudderState(workers(merged), waiting));
    await ctx.integrations.idle();
    const { comments, stateUpdates } = ctx.upstream.linearState;
    assert.equal(comments.length, 10, "capped at 10 per issue");
    assert.equal(stateUpdates.length, 1, "a node still waits to launch: not completed");

    await ctx.integrations.onHomeState("alice", rudderState(workers(merged), { nodes: [], plans: [] }));
    await ctx.integrations.idle();
    assert.equal(comments.length, 10);
    assert.deepEqual(stateUpdates.at(-1), { id: issueId, stateId: "st-done" });
    assert.equal(stateUpdates.length, 2);
  } finally {
    await ctx.close();
  }
});

test("track plans: a skipped plan skips its workers; a plan attached to the user's issue is never closed", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    seedBareMetal(ctx);
    ctx.upstream.handlers.classify = (body) => (body.input.split("<plan_goal>")[1].includes("bare metal")
      ? trackDecision({ action: "attach", issue: "OPS-50" })
      : trackDecision({ action: "skip", reason: "Only questions." }));
    const agent = agentAt(ctx);
    const goalQ = "Answer questions about how the scheduler works";
    const goalM = "Cut Rudder over to the bare metal hosts";
    const q = (id, node) => agent(id, workerTask(node, `Question ${node}`, goalQ), { planId: "plan-q", nodeId: node });
    const m = (id, node, extra) => agent(id, workerTask(node, `Move ${node}`, goalM), { planId: "plan-m", nodeId: node, ...extra });
    await ctx.integrations.onHomeState("alice", rudderState([q("q-1", "n0"), q("q-2", "n1"), m("m-1", "n0"), m("m-2", "n1")]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 2);
    assert.equal(planRowOf(ctx, "plan-q").action, "skip");
    assert.equal(trackedRow(ctx, "q-1").action, "skip");
    assert.match(trackedRow(ctx, "q-1").reason, /part of plan plan-q, which is not tracked/);
    assert.equal(trackedRow(ctx, "m-2").identifier, "OPS-50");
    const merged = { status: "merged", summary: "Moved it." };
    await ctx.integrations.onHomeState("alice", rudderState([m("m-1", "n0", merged), m("m-2", "n1", merged)], { nodes: [], plans: [{ id: "plan-m", pending: 0, launched: 2, merged: 2 }] }));
    await ctx.integrations.idle();
    assert.equal(ctx.upstream.linearState.comments.length, 2);
    assert.deepEqual(ctx.upstream.linearState.stateUpdates, [{ id: "iss-bm", stateId: "st-progress" }], "the user's own issue is never completed");
    assert.equal(ctx.upstream.linearState.created.length, 0);
  } finally {
    await ctx.close();
  }
});

// ---- branches follow their parent ----

test("track branches: branches attach to the parent's issue without a model call; a skipped or missing parent skips them", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    ctx.upstream.handlers.classify = (body) => (body.input.split("<agent_task>")[1].includes("Explain")
      ? trackDecision({ action: "skip", reason: "A question." })
      : trackDecision({ action: "create", title: "Fix Rudder's Slack replies" }));
    const agent = agentAt(ctx);
    // The record keeps the parent's task as typed; the home strips Rudder's framing from the parent's own row.
    const raw = "Objective: Fix replies\nDone when: replies stay on topic\n\nRudder's Slack replies drift off topic and out of character; fix the reply prompt";
    const parent = agent("run-p", "Rudder's Slack replies drift off topic and out of character; fix the reply prompt");
    const branch = (id, at, extra) => agent(id, `Branch of: ${raw}`, { createdAt: ctx.clock.t + at, ...extra });
    const question = agent("run-q", "Explain how the Slack reply prompt is built");
    // Newest first, as the home sends them: every branch is listed before its parent.
    await ctx.integrations.onHomeState("alice", rudderState([
      branch("b-3", 4000),
      branch("b-2", 3000),
      branch("b-1", 2000),
      agent("b-q", "Branch of: Explain how the Slack reply prompt is built", { createdAt: ctx.clock.t + 2500 }),
      agent("b-lost", "Branch of: Some task whose run was deleted long ago", { createdAt: ctx.clock.t + 2500 }),
      parent,
      question,
    ]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 2, "only the two parents reach the model");
    assert.equal(ctx.upstream.linearState.created.length, 1, "one issue, not one per branch");
    const issueId = trackedRow(ctx, "run-p").issue_id;
    for (const id of ["b-1", "b-2", "b-3"]) {
      const row = trackedRow(ctx, id);
      assert.deepEqual([row.action, row.issue_id, row.created_issue], ["attach", issueId, 0], id);
    }
    assert.match(trackedRow(ctx, "b-1").reason, /branch of run-p/);
    assert.equal(trackedRow(ctx, "b-q").action, "skip");
    assert.match(trackedRow(ctx, "b-q").reason, /branch of run-q, which is not tracked/);
    assert.equal(trackedRow(ctx, "b-lost").action, "skip");

    // A branch of a branch follows it too; a branch merge comments but never closes the parent's issue.
    await ctx.integrations.onHomeState("alice", rudderState([
      agent("b-b", `Branch of: Branch of: ${raw}`, { createdAt: ctx.clock.t + 6000 }),
      branch("b-1", 2000, { status: "merged", summary: "Kept replies on topic." }),
      parent,
    ]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 2);
    assert.equal(trackedRow(ctx, "b-b").issue_id, issueId);
    assert.deepEqual(ctx.upstream.linearState.comments, [{ issueId, body: `Merged: Kept replies on topic.\n\n${AGENT_URL("b-1")}` }]);
    assert.deepEqual(ctx.upstream.linearState.stateUpdates, [{ id: issueId, stateId: "st-progress" }]);
  } finally {
    await ctx.close();
  }
});

// ---- long sessions (main rows): one decision per user turn ----

const MAIN_ID = "1793356272010755150-main-branch-24944";
const offstageState = (agents) => [{ workspaceId: "ws-offstage", name: "offstage", repo: "exla-ai/offstage", agents }];
async function linkOffstage(ctx) {
  const res = await ctx.call("POST", "/api/app/linear/mapping", { account: "alice", body: { kind: "project", linearId: "lp-offstage", workspace: "ws-offstage" } });
  assert.equal(res.status, 200, JSON.stringify(res.data));
}
const mainRow = (ctx, turns, prompt, extra = {}) => ({
  id: MAIN_ID,
  title: "Read RUDDER.md if it exists",
  task: "Read RUDDER.md if it exists, then briefly tell me what this project does",
  status: "working",
  mode: "main",
  createdAt: Date.parse("2026-10-30T10:00:00Z"),
  userTurns: turns,
  lastUserPrompt: prompt,
  lastUserAt: ctx.clock.t + 1000,
  ...extra,
});
const turnRows = (ctx) => ctx.database.prepare("select turn, action, identifier from rudder_linear_tracked_turns order by turn").all().map((row) => ({ ...row }));

test("track sessions: each new user turn of a main row is decided once, at most once a minute, follow-ups attach, and nothing closes", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    await linkOffstage(ctx);
    const prompts = [];
    ctx.upstream.handlers.classify = (body) => {
      prompts.push(body.input);
      return prompts.length === 1
        ? trackDecision({ action: "create", title: "Build the Offstage dashboard" })
        : trackDecision({ action: "attach", issue: "ENG-1" });
    };
    const dashboard = "Build the Offstage dashboard: the outreach funnel and reply rates per campaign";
    await ctx.integrations.onHomeState("alice", offstageState([mainRow(ctx, 230, dashboard)]));
    await ctx.integrations.onHomeState("alice", offstageState([mainRow(ctx, 230, dashboard)]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 1);
    assert.match(prompts[0], /<agent_task>\nBuild the Offstage dashboard: the outreach funnel/);
    const { created, stateUpdates, comments } = ctx.upstream.linearState;
    assert.deepEqual(created.map((input) => [input.title, input.projectId, input.teamId]), [["Build the Offstage dashboard", "lp-offstage", "team-eng"]]);
    assert.match(created[0].description, /Tracked from a Rudder session in offstage\.\n\nhttps:\/\/cloud\.test\/app\/p\/ws-offstage\/a\/1793356272010755150-main-branch-24944$/);
    assert.deepEqual(stateUpdates, [{ id: "iss-1", stateId: "st-progress" }]);

    // A follow-up 30 seconds later waits out the minute, then attaches to the same issue.
    ctx.clock.t += 30_000;
    const chart = mainRow(ctx, 231, "Add a replies per day chart to the dashboard");
    await ctx.integrations.onHomeState("alice", offstageState([chart]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 1, "rate limited");
    ctx.clock.t += 31_000;
    await ctx.integrations.onHomeState("alice", offstageState([chart]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 2);
    assert.match(prompts[1], /Agents already tracked in this project:\n- "Build the Offstage dashboard: the outreach funnel and reply rates per campaign" -> ENG-1/);
    assert.equal(created.length, 1);

    // A one-word turn is skipped without the model; a restart decides nothing again.
    await ctx.integrations.onHomeState("alice", offstageState([mainRow(ctx, 232, "thanks!")]));
    await ctx.integrations.idle();
    const again = ctx.restart();
    await again.onHomeState("alice", offstageState([mainRow(ctx, 232, "thanks!", { status: "waiting" })]));
    await again.idle();
    assert.equal(classifyCalls(ctx).length, 2);
    assert.deepEqual(turnRows(ctx), [
      { turn: 230, action: "create", identifier: "ENG-1" },
      { turn: 231, action: "attach", identifier: "ENG-1" },
      { turn: 232, action: "skip", identifier: null },
    ]);
    assert.equal(comments.length, 0, "a session has no merge comment");
    assert.equal(stateUpdates.length, 1, "and is never completed by Rudder");
  } finally {
    await ctx.close();
  }
});

test("track sessions: once, at start, a running main row's newest prompt is backfilled; idle rows and older homes are not", async () => {
  const ctx = await setup();
  try {
    await linked(ctx);
    await linkOffstage(ctx);
    ctx.upstream.handlers.classify = () => trackDecision({ action: "create", title: "Build the Offstage dashboard" });
    const before = ctx.clock.t - 3 * 60 * 60_000;
    const running = mainRow(ctx, 233, "Build the Offstage dashboard with the outreach funnel", { lastUserAt: before });
    const idle = { ...mainRow(ctx, 40, "Add Stripe webhooks to Libra", { lastUserAt: before, status: "paused" }), id: "libra-main" };
    const old = { ...mainRow(ctx, 12, "Rewrite the pricing page"), id: "old-home-main", userTurns: undefined, lastUserPrompt: undefined, lastUserAt: undefined };
    await ctx.integrations.onHomeState("alice", [
      ...offstageState([running, old]),
      { workspaceId: "ws-libra", name: "libra", agents: [idle] },
    ]);
    await ctx.integrations.onHomeState("alice", offstageState([running]));
    await ctx.integrations.idle();
    assert.equal(classifyCalls(ctx).length, 1, "only the running row's newest prompt");
    assert.deepEqual(ctx.database.prepare("select run_id, turn, action from rudder_linear_tracked_turns").all().map((row) => ({ ...row })), [
      { run_id: MAIN_ID, turn: 233, action: "create" },
    ]);
  } finally {
    await ctx.close();
  }

  // Seen only after the backfill window: an old prompt is never decided, a new one is.
  const late = await setup();
  try {
    await linked(late);
    await linkOffstage(late);
    late.upstream.handlers.classify = () => trackDecision({ action: "create", title: "Build the Offstage dashboard" });
    late.clock.t += 13 * 60 * 60_000;
    await late.integrations.onHomeState("alice", offstageState([mainRow(late, 233, "Build the dashboard", { lastUserAt: late.clock.t - 14 * 60 * 60_000 })]));
    await late.integrations.idle();
    assert.equal(classifyCalls(late).length, 0);
    await late.integrations.onHomeState("alice", offstageState([mainRow(late, 234, "Add the reply rate chart to the dashboard")]));
    await late.integrations.idle();
    assert.equal(classifyCalls(late).length, 1);
  } finally {
    await late.close();
  }
});

test("track: a database from the first tracking release gains the plan and comment columns", async () => {
  const ctx = await setup();
  try {
    ctx.database.exec(`
      drop table rudder_linear_tracked_runs;
      create table rudder_linear_tracked_runs (
        workspace_id text not null, run_id text not null, account_id text not null, owner_kind text not null,
        owner_id text not null, action text not null, issue_id text, identifier text,
        created_issue integer not null default 0, started_synced integer not null default 0,
        merged_synced integer not null default 0, agent_title text, reason text,
        created_at integer not null, updated_at integer not null, primary key (workspace_id, run_id)
      );
      insert into rudder_linear_tracked_runs (workspace_id, run_id, account_id, owner_kind, owner_id, action, created_at, updated_at)
        values ('ws-rudder', 'run-old', 'alice', 'account', 'alice', 'skip', 1, 1);
    `);
    ctx.restart();
    const columns = ctx.database.prepare("pragma table_info(rudder_linear_tracked_runs)").all().map((column) => column.name);
    assert.ok(columns.includes("plan_id") && columns.includes("merge_commented"));
    assert.deepEqual({ ...ctx.database.prepare("select action, plan_id, merge_commented from rudder_linear_tracked_runs").get() }, { action: "skip", plan_id: null, merge_commented: 0 });
  } finally {
    await ctx.close();
  }
});
