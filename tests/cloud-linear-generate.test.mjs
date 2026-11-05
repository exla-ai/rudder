import assert from "node:assert/strict";
import {
  ANTHROPIC_KEY,
  BASE_ENV,
  OPENAI_KEY,
  TASKS,
  anthropicResponse,
  assertNoSecrets,
  connectLinear,
  json,
  linear,
  openaiResponse,
  setup,
  taskgen,
  test,
} from "./helpers/cloud-linear.mjs";

const GOAL = "Make uploads reliable";
const openaiCalls = (ctx) => ctx.upstream.calls.filter((c) => c.url === "https://api.openai.com/v1/responses");
const anthropicCalls = (ctx) => ctx.upstream.calls.filter((c) => c.url.startsWith("https://api.anthropic.com/"));
const gqlCalls = (ctx, needle) => ctx.upstream.calls.filter((c) => c.url === "https://api.linear.app/graphql" && JSON.parse(c.body).query.includes(needle));

test("generate: gpt-6-luna on the Responses API with a strict schema, then rudder-labeled issues in the Linear project", async () => {
  const ctx = await setup();
  try {
    await connectLinear(ctx, "alice");
    const res = await ctx.call("POST", "/api/app/linear/generate", {
      account: "alice",
      body: { goal: GOAL, linearProjectId: "lp-rudder", projectId: "ws-rudder" },
    });
    assert.equal(res.status, 200, JSON.stringify(res.data));
    assert.equal(res.data.provider, "openai");
    assert.equal(res.data.model, "gpt-6-luna");
    assert.deepEqual(res.data.issues.map((i) => i.title), TASKS.map((t) => t.title));
    assert.match(res.data.issues[0].identifier, /^OPS-\d+$/, "a project's first team takes the issues");
    assert.equal(res.data.autoStart, true);
    assert.deepEqual(res.data.link, { workspaceId: "ws-rudder", projectName: "exla-ai-rudder", teamId: "team-ops", mine: true });

    const [call] = openaiCalls(ctx);
    assert.equal(call.headers.authorization, `Bearer ${OPENAI_KEY}`);
    const sent = JSON.parse(call.body);
    assert.equal(sent.model, "gpt-6-luna");
    assert.deepEqual(sent.reasoning, { effort: "none" });
    assert.equal(sent.text.format.type, "json_schema");
    assert.equal(sent.text.format.strict, true);
    assert.deepEqual(sent.text.format.schema.required, ["tasks"]);
    assert.equal(sent.text.format.schema.properties.tasks.items.additionalProperties, false);
    assert.match(sent.input, /Project: exla-ai-rudder \(exla-ai\/rudder\)/);
    assert.match(sent.input, /<goal>\nMake uploads reliable\n<\/goal>/);
    assert.equal(anthropicCalls(ctx).length, 0);

    // issueCreate: team + project + rudder label, description ends with the done-when line.
    const created = ctx.upstream.linearState.created;
    assert.equal(created.length, 3);
    assert.deepEqual(created[0], {
      teamId: "team-ops",
      projectId: "lp-rudder",
      title: TASKS[0].title,
      description: `${TASKS[0].description}\n\nDone when: ${TASKS[0].done_when}`,
      labelIds: ["lab-new-1"],
    });
    const [labelCreate] = gqlCalls(ctx, "issueLabelCreate");
    assert.deepEqual(JSON.parse(labelCreate.body).variables.input, { name: "rudder", teamId: "team-ops", color: "#5E6AD2" });
    const [createCall] = gqlCalls(ctx, "issueCreate");
    assert.match(JSON.parse(createCall.body).query, /issueCreate\(input: \$input\) \{ success issue \{ id identifier title url \} \}/);

    // A second run finds the label instead of creating another.
    const again = await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: GOAL, linearProjectId: "lp-rudder" } });
    assert.equal(again.status, 200, JSON.stringify(again.data));
    assert.equal(gqlCalls(ctx, "issueLabelCreate").length, 1);
    assert.deepEqual(ctx.upstream.linearState.created.at(-1).labelIds, ["lab-new-1"]);
    assert.ok(ctx.events.some((e) => e.name === "linear_tasks_generated" && e.props.count === 3 && e.props.provider === "openai"));
    assertNoSecrets([...ctx.logs, JSON.stringify(res.data), JSON.stringify(ctx.events)]);
  } finally {
    await ctx.close();
  }
});

test("generate: RUDDER_TASKGEN_MODEL overrides the model; a team works without a Linear project", async () => {
  const ctx = await setup({ env: { ...BASE_ENV, RUDDER_TASKGEN_MODEL: "gpt-6-luna-mini" } });
  try {
    await connectLinear(ctx, "alice");
    const res = await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: GOAL, teamId: "team-eng" } });
    assert.equal(res.status, 200, JSON.stringify(res.data));
    assert.equal(JSON.parse(openaiCalls(ctx)[0].body).model, "gpt-6-luna-mini");
    assert.equal(ctx.upstream.linearState.created[0].teamId, "team-eng");
    assert.equal("projectId" in ctx.upstream.linearState.created[0], false);
    assert.equal(res.data.link, null);
    assert.equal(res.data.autoStart, false, "nothing starts without a linked Rudder project");
  } finally {
    await ctx.close();
  }
});

test("generate: invalid model output is rejected and creates no issues", async () => {
  const ctx = await setup();
  try {
    await connectLinear(ctx, "alice");
    const bad = [
      "not json at all",
      { tasks: [] },
      { tasks: TASKS, extra: true },
      { tasks: [{ ...TASKS[0], priority: 1 }, TASKS[1], TASKS[2]] },
      { tasks: [{ ...TASKS[0], title: "  " }, TASKS[1], TASKS[2]] },
      { tasks: [TASKS[0], { ...TASKS[1], title: TASKS[0].title.toUpperCase() }, TASKS[2]] },
      { tasks: [{ title: "x", description: "y" }, TASKS[1], TASKS[2]] },
    ];
    for (const payload of bad) {
      ctx.upstream.handlers.openai = () => openaiResponse(payload);
      const res = await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: GOAL, teamId: "team-eng" } });
      assert.equal(res.status, 502, `${JSON.stringify(payload).slice(0, 60)} -> ${JSON.stringify(res.data)}`);
    }
    assert.equal(ctx.upstream.linearState.created.length, 0);
    // A refusal is its own message.
    ctx.upstream.handlers.openai = () => json({ status: "completed", output: [{ type: "message", content: [{ type: "refusal", refusal: "no" }] }] });
    const refused = await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: GOAL, teamId: "team-eng" } });
    assert.equal(refused.status, 422);
    assert.match(refused.data.error, /declined/);
  } finally {
    await ctx.close();
  }
});

test("parseGeneratedTasks is strict and keeps titles, descriptions and done-when lines", () => {
  // A small goal can be one task; past the cap the first ten are kept.
  assert.equal(taskgen.parseGeneratedTasks(JSON.stringify({ tasks: TASKS.slice(0, 1) })).length, 1);
  const many = Array.from({ length: 12 }, (_, i) => ({ ...TASKS[0], title: `task ${i}` }));
  assert.equal(taskgen.parseGeneratedTasks(JSON.stringify({ tasks: many })).length, taskgen.MAX_TASKS);
  const ok = taskgen.parseGeneratedTasks(JSON.stringify({ tasks: TASKS }));
  assert.deepEqual(ok[0], { title: TASKS[0].title, description: TASKS[0].description, doneWhen: TASKS[0].done_when });
  assert.throws(() => taskgen.parseGeneratedTasks("[]"), /task list/);
  assert.throws(() => taskgen.parseGeneratedTasks(JSON.stringify({ tasks: [TASKS[0], TASKS[1], { ...TASKS[2], done_when: "x".repeat(1001) }] })), /longer than/);
  assert.equal(taskgen.linearIssueDescription(ok[0]), `${TASKS[0].description}\n\nDone when: ${TASKS[0].done_when}`);
});

test("generate: no OpenAI or Anthropic key is a 503 with a plain message", async () => {
  const ctx = await setup({ env: { ...BASE_ENV, OPENAI_API_KEY: "" } });
  try {
    await connectLinear(ctx, "alice");
    const res = await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: GOAL, teamId: "team-eng" } });
    assert.equal(res.status, 503);
    assert.equal(res.data.error, "Task generation is not set up on this server (OPENAI_API_KEY is missing)");
    assert.equal((await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { teamId: "team-eng" } })).status, 400);
    assert.equal((await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: GOAL } })).status, 400);
    const setupRes = await ctx.call("GET", "/api/app/linear/setup", { account: "alice" });
    assert.equal(setupRes.data.generate, false);
  } finally {
    await ctx.close();
  }
});

test("generate: without OPENAI_API_KEY, Claude Haiku 5.5 with the workspace header and a json_schema output", async () => {
  const ctx = await setup({ env: { ...BASE_ENV, OPENAI_API_KEY: "", ANTHROPIC_API_KEY: ANTHROPIC_KEY, ANTHROPIC_WORKSPACE_ID: "wrkspc_123" } });
  try {
    await connectLinear(ctx, "alice");
    const res = await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: GOAL, teamId: "team-eng" } });
    assert.equal(res.status, 200, JSON.stringify(res.data));
    assert.equal(res.data.provider, "anthropic");
    assert.equal(res.data.model, "claude-haiku-5-5");
    assert.equal(openaiCalls(ctx).length, 0);
    const [call] = anthropicCalls(ctx);
    assert.equal(call.url, "https://api.anthropic.com/v1/messages");
    // SDK 1.x sends the key as a bearer token.
    assert.equal(call.headers.authorization, `Bearer ${ANTHROPIC_KEY}`);
    assert.ok(call.headers["anthropic-version"]);
    assert.equal(call.headers["anthropic-workspace-id"], "wrkspc_123");
    const sent = JSON.parse(call.body);
    assert.equal(sent.model, "claude-haiku-5-5");
    assert.deepEqual(sent.output_config.format.type, "json_schema");
    assert.deepEqual(sent.output_config.format.schema.required, ["tasks"]);
    assert.equal(sent.temperature, undefined);
    assert.equal(ctx.upstream.linearState.created.length, 3);
  } finally {
    await ctx.close();
  }
  // A normal workspace key sends no workspace header.
  const plain = await setup({ env: { ...BASE_ENV, OPENAI_API_KEY: "", ANTHROPIC_API_KEY: ANTHROPIC_KEY } });
  try {
    await connectLinear(plain, "alice");
    assert.equal((await plain.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: GOAL, teamId: "team-eng" } })).status, 200);
    assert.equal(anthropicCalls(plain)[0].headers["anthropic-workspace-id"], undefined);
  } finally {
    await plain.close();
  }
});

test("generate: an OpenAI failure falls back to Claude; Claude's bad output is still rejected; keys stay out of logs", async () => {
  const ctx = await setup({ env: { ...BASE_ENV, ANTHROPIC_API_KEY: ANTHROPIC_KEY } });
  try {
    await connectLinear(ctx, "alice");
    ctx.upstream.handlers.openai = () => json({ error: { message: `Incorrect API key provided: ${OPENAI_KEY}` } }, 401);
    const res = await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: GOAL, teamId: "team-eng" } });
    assert.equal(res.status, 200, JSON.stringify(res.data));
    assert.equal(res.data.provider, "anthropic");
    assert.ok(ctx.logs.some((line) => /OpenAI failed .*using Claude/.test(line)));

    ctx.upstream.handlers.openai = () => openaiResponse("{");
    ctx.upstream.handlers.anthropic = () => anthropicResponse({ tasks: [] });
    const bad = await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: GOAL, teamId: "team-eng" } });
    assert.equal(bad.status, 502);

    ctx.upstream.handlers.openai = () => json({ error: { message: "server error" } }, 400);
    ctx.upstream.handlers.anthropic = () => json({ type: "error", error: { type: "invalid_request_error", message: "Your credit balance is too low to access the Anthropic API." } }, 400);
    const broke = await ctx.call("POST", "/api/app/linear/generate", { account: "alice", body: { goal: GOAL, teamId: "team-eng" } });
    assert.equal(broke.status, 502);
    assert.match(broke.data.error, /credit balance is too low/);
    assertNoSecrets([...ctx.logs, JSON.stringify(res.data), JSON.stringify(bad.data), JSON.stringify(broke.data)]);
  } finally {
    await ctx.close();
  }
});

test("setup: suggests Libra, Offstage and Rudder by name; admins confirm links; members cannot", async () => {
  const ctx = await setup();
  try {
    await connectLinear(ctx, "alice", "org1");
    const res = await ctx.call("GET", "/api/app/linear/setup?org=org1", { account: "alice" });
    assert.equal(res.status, 200, JSON.stringify(res.data));
    const byName = Object.fromEntries(res.data.linearProjects.map((p) => [p.name, p]));
    assert.deepEqual(byName.Libra.suggestion, { workspaceId: "ws-libra", name: "libra" });
    assert.deepEqual(byName.Offstage.suggestion, { workspaceId: "ws-offstage", name: "offstage" });
    assert.deepEqual(byName.Rudder.suggestion, { workspaceId: "ws-rudder", name: "exla-ai-rudder" });
    assert.equal(byName.Rudder.link, null);
    assert.deepEqual(res.data.teams.map((t) => t.key), ["ENG", "OPS"]);
    assert.equal(res.data.canManage, true);
    assert.equal(res.data.generate, true);
    assert.equal(res.data.autoStart, true);
    assert.deepEqual(res.data.projects.map((p) => p.workspaceId), ["ws-libra", "ws-offstage", "ws-rudder"]);

    const teams = await ctx.call("GET", "/api/app/linear/teams?org=org1", { account: "bob" });
    assert.deepEqual(teams.data.teams.map((t) => t.id), ["team-eng", "team-ops"]);
    assert.equal(teams.data.canManage, false);

    const denied = await ctx.call("POST", "/api/app/linear/mapping", { account: "bob", body: { org: "org1", kind: "project", linearId: "lp-libra", workspace: "ws-libra" } });
    assert.equal(denied.status, 403);
    const denyGenerate = await ctx.call("POST", "/api/app/linear/generate", { account: "bob", body: { org: "org1", goal: GOAL, linearProjectId: "lp-libra", projectId: "ws-libra" } });
    assert.equal(denyGenerate.status, 403);

    for (const [linearId, workspace] of [["lp-libra", "ws-libra"], ["lp-offstage", "ws-offstage"], ["lp-rudder", "ws-rudder"]]) {
      const linked = await ctx.call("POST", "/api/app/linear/mapping", { account: "alice", body: { org: "org1", kind: "project", linearId, workspace } });
      assert.equal(linked.status, 200, JSON.stringify(linked.data));
    }
    const rudderTeam = await ctx.call("POST", "/api/app/linear/mapping", { account: "alice", body: { org: "org1", kind: "project", linearId: "lp-rudder", workspace: "ws-rudder", teamId: "team-eng" } });
    assert.equal(rudderTeam.data.link.teamId, "team-eng", "a project with two teams can pick which one gets issues");
    assert.equal((await ctx.call("POST", "/api/app/linear/mapping", { account: "alice", body: { org: "org1", kind: "project", linearId: "lp-nope", workspace: "ws-libra" } })).status, 404);
    assert.equal((await ctx.call("POST", "/api/app/linear/mapping", { account: "alice", body: { org: "org1", kind: "project", linearId: "lp-libra", workspace: "ws-nope" } })).status, 404);

    const after = await ctx.call("GET", "/api/app/linear/setup?org=org1", { account: "bob" });
    const libra = after.data.linearProjects.find((p) => p.id === "lp-libra");
    assert.deepEqual(libra.link, { workspaceId: "ws-libra", projectName: "libra", teamId: "team-eng", mine: false });
    assert.equal(libra.suggestion, null);

    // A member generates into a linked Linear project (no relinking needed).
    const memberGen = await ctx.call("POST", "/api/app/linear/generate", { account: "bob", body: { org: "org1", goal: GOAL, linearProjectId: "lp-libra" } });
    assert.equal(memberGen.status, 200, JSON.stringify(memberGen.data));
    assert.match(JSON.parse(openaiCalls(ctx).at(-1).body).input, /Project: libra/);
    assert.equal(ctx.upstream.linearState.created.at(-1).projectId, "lp-libra");

    const cleared = await ctx.call("POST", "/api/app/linear/mapping", { account: "alice", body: { org: "org1", kind: "project", linearId: "lp-libra", workspace: null } });
    assert.deepEqual(cleared.data, { link: null });
  } finally {
    await ctx.close();
  }
});

test("suggestRudderProject: exact names first, then a last-word match; ties suggest nothing", () => {
  const projects = [
    { workspaceId: "a", name: "exla-ai-rudder", repo: "exla-ai/rudder" },
    { workspaceId: "b", name: "rudder-site", repo: "exla-ai/rudder-site" },
    { workspaceId: "c", name: "libra", repo: "exla-ai/libra" },
    { workspaceId: "d", name: "offstage", repo: null },
  ];
  assert.equal(linear.suggestRudderProject("Rudder", projects).workspaceId, "a");
  assert.equal(linear.suggestRudderProject("Libra", projects).workspaceId, "c");
  assert.equal(linear.suggestRudderProject("Offstage", projects).workspaceId, "d");
  assert.equal(linear.suggestRudderProject("Rudder Site", projects).workspaceId, "b");
  assert.equal(linear.suggestRudderProject("Website", projects), null);
  assert.equal(linear.suggestRudderProject("Rudder", [{ workspaceId: "x", name: "rudder" }, { workspaceId: "y", name: "Rudder" }]), null);
});

test("label find-or-create prefers the team label, then a workspace label", async () => {
  const ctx = await setup();
  try {
    ctx.upstream.linearState.labels.push({ id: "lab-ws", name: "Rudder", team: null }, { id: "lab-other", name: "rudder", team: { id: "team-ops" } });
    const ws = await linear.findOrCreateRudderLabel(ctx.upstream.fetchImpl, "tok", "team-eng");
    assert.deepEqual({ labelId: ws.labelId, created: ws.created, team: ws.team.key }, { labelId: "lab-ws", created: false, team: "ENG" });
    const own = await linear.findOrCreateRudderLabel(ctx.upstream.fetchImpl, "tok", "team-ops");
    assert.equal(own.labelId, "lab-other");
    await assert.rejects(linear.findOrCreateRudderLabel(ctx.upstream.fetchImpl, "tok", "team-missing"), /team not found/);
  } finally {
    await ctx.close();
  }
});
