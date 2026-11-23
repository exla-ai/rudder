// The "/" lists of an agent's CLI (cloud/worker/slash-commands.mjs) and how
// the app orders them for what is typed (cloud/app/slash-rank.js). The CLI
// test builds both lists with the installed claude, codex and tmux from
// synthetic config folders. Neither holds a login and nothing is sent, so no
// model is called.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  CODEX_VERSION, CODEX_VERSIONS, buildClaudeCommands, buildCodexCommands, buildQueue, claudeInitialize, claudeListKey, claudeStamp, codexHasVoice,
  codexPackageDir, parseClaudeMenu, readClaudeBinaryCommands, readClaudeMenu, readCodexState,
} from "../cloud/worker/slash-commands.mjs";
import { rank } from "../cloud/app/slash-rank.js";

function onPath(name) {
  for (const dir of String(process.env.PATH).split(path.delimiter)) {
    const file = path.join(dir, name);
    if (fs.existsSync(file)) return file;
  }
  throw new Error(`${name} must be installed for this test`);
}

// Not a real key: it only lets Claude Code start without a login screen.
const FAKE_KEY = "sk-ant-api03-rudder-slash-test-000000000000000000000000";

// A stdio MCP server with two prompts.
const MCP_SERVER = `import readline from "node:readline";
const send = (m) => process.stdout.write(JSON.stringify(m) + "\\n");
readline.createInterface({ input: process.stdin }).on("line", (line) => {
  const m = JSON.parse(line);
  if (m.id === undefined) return;
  if (m.method === "initialize") return send({ jsonrpc: "2.0", id: m.id, result: { protocolVersion: m.params.protocolVersion, capabilities: { prompts: {} }, serverInfo: { name: "testmcp", version: "1.0.0" } } });
  if (m.method === "prompts/list") return send({ jsonrpc: "2.0", id: m.id, result: { prompts: [
    { name: "summarize", description: "Summarize a file", arguments: [{ name: "path", required: true }] },
    { name: "standup", description: "Write my standup" },
  ] } });
  send({ jsonrpc: "2.0", id: m.id, error: { code: -32601, message: "unknown method" } });
});
`;

function writeFixture(root) {
  const write = (rel, body) => {
    const file = path.join(root, rel);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, typeof body === "string" ? body : JSON.stringify(body, null, 2));
  };
  const claude = "home/.claude";
  const project = path.join(root, "project");
  write(`${claude}/.claude.json`, {
    hasCompletedOnboarding: true,
    customApiKeyResponses: { approved: [FAKE_KEY.slice(-20)], rejected: [] },
    projects: { [project]: { hasTrustDialogAccepted: true } },
  });
  write(`${claude}/commands/hello.md`, "---\ndescription: Say hello to someone\nargument-hint: <name>\n---\nSay hello to $ARGUMENTS\n");
  write(`${claude}/commands/ops/deploy.md`, "---\ndescription: Deploy the service\n---\nDeploy it\n");
  write(`${claude}/skills/tidy/SKILL.md`, "---\nname: tidy\ndescription: Tidy the working tree\n---\nTidy up.\n");
  write(`${claude}/skills/model-only/SKILL.md`, "---\nname: model-only\ndescription: Only the model uses this\nuser-invocable: false\n---\nx\n");
  // An installed, enabled plugin from a local marketplace.
  const market = path.join(root, "market");
  const installed = path.join(root, claude, "plugins/cache/testmkt/greeter/1.0.0");
  write("market/.claude-plugin/marketplace.json", { name: "testmkt", owner: { name: "Test" }, plugins: [{ name: "greeter", source: "./greeter" }] });
  for (const dir of [path.join(market, "greeter"), installed]) {
    write(path.relative(root, path.join(dir, ".claude-plugin/plugin.json")), { name: "greeter", version: "1.0.0", description: "Greets people" });
    write(path.relative(root, path.join(dir, "commands/greet.md")), "---\ndescription: Greet the team\nargument-hint: '[team]'\n---\nGreet\n");
  }
  const now = new Date().toISOString();
  write(`${claude}/plugins/known_marketplaces.json`, { testmkt: { source: { source: "directory", path: market }, installLocation: market, lastUpdated: now } });
  write(`${claude}/plugins/installed_plugins.json`, { version: 2, plugins: { "greeter@testmkt": [{ scope: "user", installPath: installed, version: "1.0.0", installedAt: now, lastUpdated: now }] } });
  // Accepted once per account, as on a home whose agents run in this mode.
  write(`${claude}/settings.json`, { enabledPlugins: { "greeter@testmkt": true }, skipDangerousModePermissionPrompt: true });
  write("project/.claude/commands/ship.md", "---\ndescription: Ship this project\n---\nShip\n");
  write("project/.claude/commands/greet-args.md", "---\ndescription: Greet with named arguments\narguments: [who, when]\n---\nGreet $who at $when\n");
  write("mcp-server.mjs", MCP_SERVER);
  // Never approved for this folder, like a branch workspace below a checkout
  // with .mcp.json: the agent's bypassPermissions mode starts it unasked.
  write("project/.mcp.json", { mcpServers: { testmcp: { command: process.execPath, args: [path.join(root, "mcp-server.mjs")] } } });
  // Codex: a catalog whose model has a Fast tier, and a custom prompt that
  // Codex's "/" popup does not list.
  write("home/.codex/config.toml", 'model = "test-model"\n');
  write("home/.codex/models_cache.json", { models: [{ slug: "test-model", service_tiers: [{ id: "priority", name: "Fast", description: "Quicker answers" }], available_access_programs: { cyber: ["standard"] } }] });
  write("home/.codex/prompts/standup.md", "---\ndescription: Write my standup\n---\nWrite it\n");
}

test("the worker image pins the Codex whose '/' commands Rudder lists", () => {
  const dockerfile = fs.readFileSync(new URL("../cloud/worker/Dockerfile", import.meta.url), "utf8");
  assert.equal(/^ARG CODEX_VERSION=(\S+)$/m.exec(dockerfile)?.[1], CODEX_VERSION);
});

test("a list the app waits on is built before warm-up lists", async () => {
  // A home that just started queues a Claude Code start per recent agent;
  // the app's request must not wait behind all of them.
  const queue = buildQueue();
  const ran = [];
  const job = (name) => () => { ran.push(name); };
  const warm = ["w1", "w2", "w3", "w4"].map((name) => queue.add(job(name), false));
  const asked = queue.add(job("asked"), true);
  warm[3].hurry();
  await Promise.all([asked.promise, ...warm.map((w) => w.promise)]);
  assert.deepEqual(ran, ["w1", "asked", "w4", "w2", "w3"]);
});

test("a branch's workspace shares its checkout's Claude list unless its own files differ", () => {
  // Claude reads project commands, skills, settings and .mcp.json from the
  // working folder up to the checkout and counts a name with the same content
  // once (measured on 2.1.330). A jj workspace below the checkout has no .git.
  // Built per folder, every new branch waited for a cold Claude Code start.
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "rudder-slash-key-")));
  try {
    const write = (rel, body) => {
      fs.mkdirSync(path.dirname(path.join(root, rel)), { recursive: true });
      fs.writeFileSync(path.join(root, rel), body);
    };
    write("config/.claude.json", JSON.stringify({ projects: {} }));
    write("repo/.git/HEAD", "ref: refs/heads/main\n");
    write("repo/.mcp.json", JSON.stringify({ mcpServers: { docs: { command: "docs-mcp" } } }));
    write("repo/.claude/commands/ship.md", "Ship it\n");
    write("repo/.claude/skills/tidy/SKILL.md", "---\nname: tidy\n---\n");
    const branch = (name) => path.join(root, "repo/.rudder-workspaces/repo-1", name);
    fs.mkdirSync(branch("empty"), { recursive: true });
    write("repo/.rudder-workspaces/repo-1/copy/.claude/commands/ship.md", "Ship it\n");
    write("repo/.rudder-workspaces/repo-1/copy/.mcp.json", fs.readFileSync(path.join(root, "repo/.mcp.json")));
    write("repo/.rudder-workspaces/repo-1/edited/.claude/commands/ship.md", "Ship it carefully\n");
    write("repo/.rudder-workspaces/repo-1/more/.claude/commands/lint.md", "Lint\n");
    const env = { HOME: path.join(root, "home"), CLAUDE_CONFIG_DIR: path.join(root, "config") };
    const key = (cwd, extra = {}, keyEnv = env) => claudeListKey({ cwd, model: "opus", permissionMode: "bypassPermissions", ...extra }, keyEnv);
    const checkout = key(path.join(root, "repo"));
    assert.equal(key(branch("empty")), checkout, "a workspace with nothing of its own");
    assert.equal(key(branch("copy")), checkout, "a workspace holding the checkout's own files");
    assert.notEqual(key(branch("edited")), checkout, "a command the branch changed");
    assert.notEqual(key(branch("more")), checkout, "a command only the branch has");
    assert.notEqual(key(branch("empty"), { model: "haiku" }), checkout, "the menu names the model");
    assert.notEqual(key(branch("empty"), { permissionMode: "plan" }), checkout, "plan mode asks before starting .mcp.json servers");
    assert.notEqual(key(branch("empty"), {}, { ...env, CLAUDE_CONFIG_DIR: path.join(root, "other") }), checkout, "another account");
    // The account's MCP entry for the checkout counts for every folder below it.
    write("config/.claude.json", JSON.stringify({ projects: { [path.join(root, "repo")]: { disabledMcpjsonServers: ["docs"] } } }));
    const disabled = key(path.join(root, "repo"));
    assert.notEqual(disabled, checkout);
    assert.equal(key(branch("empty")), disabled);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a Claude list is rebuilt only when what its menu shows changes", () => {
  // Rebuilt every 30 minutes and on every rewrite or skill use, 36 lists on a
  // home started a 300 MB Claude Code about once a minute (perf audit §9).
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "rudder-slash-stamp-")));
  try {
    const write = (rel, body) => {
      fs.mkdirSync(path.dirname(path.join(root, rel)), { recursive: true });
      fs.writeFileSync(path.join(root, rel), body);
    };
    const later = (rel) => {
      const at = new Date(Date.now() + 60_000);
      fs.utimesSync(path.join(root, rel), at, at);
    };
    write("bin/claude", "#!/bin/sh\n");
    write("config/.claude.json", JSON.stringify({ oauthAccount: { accountUuid: "a" }, skillUsage: {}, numStartups: 1 }));
    write("config/settings.json", "{}\n");
    write("config/commands/hello.md", "Say hello\n");
    write("config/skills/tidy/SKILL.md", "---\nname: tidy\n---\n");
    write("config/skills/synced/acct/manifest.json", "{\"skills\":[]}\n");
    const env = { HOME: path.join(root, "home"), CLAUDE_CONFIG_DIR: path.join(root, "config") };
    const stamp = () => claudeStamp({ bin: path.join(root, "bin/claude"), env });
    const first = stamp();

    // Rewritten unchanged, used, counted, or a marketplace refreshed (every
    // Claude Code start, a list build's own too): the same rows.
    for (const rel of ["config/settings.json", "config/commands/hello.md", "config/skills/tidy/SKILL.md", "config/skills/synced/acct/manifest.json"]) later(rel);
    write("config/.claude.json", JSON.stringify({ oauthAccount: { accountUuid: "a" }, skillUsage: { tidy: { usageCount: 3, lastUsedAt: Date.now() } }, numStartups: 9 }));
    write("config/plugins/known_marketplaces.json", JSON.stringify({ m: { source: { source: "github" }, lastUpdated: new Date().toISOString() } }));
    assert.equal(stamp(), first);

    const changes = [
      () => write("config/settings.json", "{\"enabledPlugins\":{\"x@y\":true}}\n"),
      () => write("config/commands/hello.md", "Say hello warmly\n"),
      () => write("config/skills/lint/SKILL.md", "---\nname: lint\n---\n"),
      () => write("config/skills/synced/acct/manifest.json", "{\"skills\":[\"standup\"]}\n"),
      () => write("config/plugins/installed_plugins.json", "{}\n"),
      () => write("config/.claude.json", JSON.stringify({ oauthAccount: { accountUuid: "b" } })),
    ];
    let previous = first;
    for (const change of changes) {
      change();
      const next = stamp();
      assert.notEqual(next, previous, change.toString());
      previous = next;
    }
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Claude Code's popup is read off its screen", () => {
  // The prompt arrow is followed by a no-break space; rows are indented two
  // columns, the selected one marked; a long description wraps under itself.
  const below = [
    "╭────╮",
    "❯ /",
    "────",
    "  ❯ /code-review                       Review the current diff",
    "    /add-dir                           Add a working directory",
    "    /docs                              Read this before any search, file read or plan,",
    "                                       even with files attached. (claude.ai sync)",
    "    /claude.ai Notion:make-page (MCP)  Turn the current work into a page",
    "    /tidy",
    "",
  ].join("\n");
  assert.deepEqual(parseClaudeMenu(below), [
    { display: "code-review", description: "Review the current diff" },
    { display: "add-dir", description: "Add a working directory" },
    { display: "docs", description: "Read this before any search, file read or plan, even with files attached. (claude.ai sync)" },
    { display: "claude.ai Notion:make-page (MCP)", description: "Turn the current work into a page" },
    { display: "tidy", description: "" },
  ]);
  // With no room below, the popup is drawn above the prompt.
  const above = ["    /plan     Plan first", "", "  ❯ /add-dir   Add a working directory", "    /clear    Clear the conversation", "────", "❯ /", "  ? for shortcuts"].join("\n");
  assert.deepEqual(parseClaudeMenu(above).map((r) => r.display), ["add-dir", "clear"], "the popup is the longest block of rows");
  assert.equal(parseClaudeMenu("❯ hello\n"), null, "no menu until / is typed");
  assert.throws(() => parseClaudeMenu(`❯ /\n  ❯ /add-dir   Add\n    ↓ 117 more\n`), /did not fit/);
});

test("Claude rows get the inserted name, argument rule and what they open", () => {
  const binary = new Map([
    ["resume", [{ type: "local-jsx", argumentHint: "[session ID]", aliases: ["continue"] }]],
    ["model", [{ type: "local-jsx", argumentHint: "[model]", aliases: [] }]],
    ["compact", [{ type: "local", headless: true, argumentHint: "<instructions>", aliases: [] }]],
    ["context", [{ type: "local-jsx", aliases: [] }, { type: "local", headless: true, aliases: [] }]],
  ]);
  const headless = [
    { name: "compact", builtin: true, aliases: [] },
    { name: "context", builtin: false, argumentHint: "", aliases: [] },
    { name: "greet", builtin: false, argumentHint: "<who>", aliases: [] },
  ];
  const menu = [
    { display: "resume", description: "Resume a previous session" },
    { display: "model", description: "Switch model (currently Opus)" },
    { display: "compact", description: "Free up context" },
    { display: "context", description: "My own context command (project)" },
    { display: "greet", description: "Greet (project) (arguments: who, when)" },
    { display: "my server:sum up (MCP)", description: "Summarize" },
  ];
  const byName = new Map(buildClaudeCommands({ menu, headless, binary }).map((c) => [c.name, c]));
  assert.deepEqual(byName.get("resume"), { name: "resume", description: "Resume a previous session", hint: "[session ID]", args: null, opens: "terminal", when: "always", aliases: ["continue"] });
  assert.equal(byName.get("model").opens, "model");
  assert.equal(byName.get("compact").opens, null);
  assert.equal(byName.get("context").opens, null, "a project command that shares a built-in's name is the project's");
  assert.deepEqual([byName.get("greet").args, byName.get("greet").hint], ["required", "<who>"]);
  assert.deepEqual(byName.get("mcp__my_server__sum_up"), { name: "mcp__my_server__sum_up", label: "my server:sum up (MCP)", description: "Summarize", hint: null, args: null, opens: null, aliases: [] });
});

test("Codex gates /daybreak on the sign-in and /voice on the install", () => {
  const base = { version: CODEX_VERSION, models: [], model: null, platform: "linux", chatgpt: false, apiKey: true, voice: false };
  const names = (state) => buildCodexCommands({ ...base, ...state }).map((c) => c.name);
  const daybreak = { cli_daybreak: true };
  assert.equal(names({ features: daybreak }).includes("daybreak"), false, "an API key needs the access-program feature too");
  assert.equal(names({ features: { ...daybreak, api_key_cyber_access_programs: true } }).includes("daybreak"), true);
  assert.equal(names({ features: daybreak, apiKey: false, chatgpt: true }).includes("daybreak"), true);
  assert.equal(names({ features: {} }).includes("voice"), false);
  assert.equal(names({ features: {}, voice: true }).includes("voice"), true);

  const pkg = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-codex-pkg-"));
  try {
    fs.mkdirSync(path.join(pkg, "codex-resources/voice/bin"), { recursive: true });
    fs.writeFileSync(path.join(pkg, "codex-resources/voice/bin/codex-voice-host"), "");
    assert.equal(codexHasVoice(pkg, "linux"), false, "the helper alone is not the runtime");
    fs.mkdirSync(path.join(pkg, "codex-resources/voice/lib"));
    fs.writeFileSync(path.join(pkg, "codex-resources/voice/lib/libgstreamer-1.0.so.0"), "");
    assert.equal(codexHasVoice(pkg, "linux"), true);
  } finally {
    fs.rmSync(pkg, { recursive: true, force: true });
  }
});

test("Codex /fast runs immediately without opening the terminal", () => {
  const model = "test-model";
  const models = [{ slug: model, service_tiers: [{ id: "priority", name: "Fast", description: "Quicker answers" }] }];
  const commands = buildCodexCommands({ version: CODEX_VERSION, features: { fast_mode: true }, model, models });
  const fast = commands.find((command) => command.name === "fast");
  assert.equal(fast.opens, null);
  assert.equal(fast.immediate, true);
});

test("typed text orders Claude rows as Claude does and Codex rows as Codex does", () => {
  const c = (name, description, extra = {}) => ({ name, description, aliases: [], ...extra });
  const claude = [
    c("code-review", "Review the diff", { aliases: ["review"] }),
    c("compact", "Free up context"),
    c("context", "Show what fills the context"),
    c("copy", "Copy the last reply"),
    c("resume", "Resume a session", { aliases: ["continue"] }),
    c("security-review", "Review for security"),
    c("usage", "Show plan usage", { aliases: ["cost", "stats"] }),
    c("status", "Show status"),
    c("mcp__srv__standup", "Write my standup", { label: "srv:standup (MCP)" }),
  ];
  const show = (rows) => rows.map(({ command, alias }) => (alias ? `${command.name} (${alias})` : command.name));
  assert.deepEqual(show(rank(claude, "co", "claude")).slice(0, 4), ["copy", "compact", "context", "code-review"], "prefix matches, shortest first");
  assert.deepEqual(show(rank(claude, "rev", "claude")).slice(0, 2), ["code-review (review)", "security-review"], "an alias match shows the alias");
  assert.deepEqual(show(rank(claude, "stat", "claude")).slice(0, 2), ["status", "usage (stats)"], "a name prefix before an alias prefix");
  assert.deepEqual(show(rank(claude, "cont", "claude")).slice(0, 2), ["context", "resume (continue)"]);
  assert.equal(show(rank(claude, "stand", "claude"))[0], "mcp__srv__standup", "an MCP prompt is found by what its row shows");
  assert.equal(rank(claude, "", "claude").length, claude.length);

  const codex = [c("model", "choose model"), c("review", "review changes"), c("btw", "ask aside", { alias: true }), c("resume", "resume a chat")];
  assert.deepEqual(show(rank(codex, "", "codex")), ["model", "review", "resume"], "alias rows wait for typed text");
  assert.deepEqual(show(rank(codex, "re", "codex")), ["review", "resume"], "Codex keeps its own order and only prefixes");
  assert.deepEqual(show(rank(codex, "b", "codex")), ["btw"]);
});

test("Claude and Codex '/' lists from the installed CLIs", async () => {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "rudder-slash-")));
  try {
    writeFixture(root);
    const env = {
      PATH: process.env.PATH,
      HOME: path.join(root, "home"),
      CLAUDE_CONFIG_DIR: path.join(root, "home/.claude"),
      CODEX_HOME: path.join(root, "home/.codex"),
      ANTHROPIC_API_KEY: FAKE_KEY,
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
    };
    const cwd = path.join(root, "project");

    const claudeBin = onPath("claude");
    onPath("tmux");
    const started = Date.now();
    let first = null;
    const [menu, answer, binary] = await Promise.all([
      readClaudeMenu({ bin: claudeBin, cwd, env, permissionMode: "bypassPermissions", onFirst: async (rows) => { first = { rows, at: Date.now() - started }; } }),
      claudeInitialize({ bin: claudeBin, cwd, env }),
      readClaudeBinaryCommands(fs.realpathSync(claudeBin)),
    ]);
    // The app is served the popup's first reading while MCP servers' prompts
    // may still be joining it; the list itself waits for them to settle.
    assert.ok(first, "the popup's first reading is handed over");
    assert.ok(first.at + 1500 < Date.now() - started, `the first reading (${first.at} ms) comes well before the settled list`);
    const settledRows = new Set(menu.map((r) => r.display));
    assert.ok(first.rows.some((r) => r.display === "compact"), "the first reading has the built-ins");
    assert.deepEqual(first.rows.filter((r) => !settledRows.has(r.display)), [], "no row of the first reading leaves the menu");
    const commands = buildClaudeCommands({ menu, headless: answer.commands, binary });
    const names = commands.map((c) => c.name);
    const byName = new Map(commands.map((c) => [c.name, c]));
    assert.deepEqual(byName.get("hello"), { name: "hello", description: "Say hello to someone (user)", hint: "<name>", args: null, opens: null, aliases: [] });
    assert.equal(byName.get("ops:deploy")?.description, "Deploy the service (user)", "a subfolder namespaces the command");
    assert.equal(byName.get("tidy")?.description, "Tidy the working tree (user)");
    assert.equal(byName.has("model-only"), false, "a skill that is not user-invocable stays out");
    assert.equal(byName.get("greeter:greet")?.hint, "[team]");
    assert.deepEqual(byName.get("greet-args"), {
      name: "greet-args", description: "Greet with named arguments (project) (arguments: who, when)", hint: "[who] [when]", args: "required", opens: null, aliases: [],
    });
    assert.deepEqual(byName.get("mcp__testmcp__summarize"), {
      name: "mcp__testmcp__summarize", label: "testmcp:summarize (MCP)", description: "Summarize a file (arguments: path)", hint: "[path]", args: "required", opens: null, aliases: [],
    });
    // Claude's order: built-ins, then user, then project, then plugins, MCP
    // prompts and bundled skills, each A to Z.
    const at = (name) => {
      assert.ok(names.includes(name), `/${name} is listed`);
      return names.indexOf(name);
    };
    const order = ["resume", "hello", "ops:deploy", "tidy", "greet-args", "ship", "greeter:greet", "mcp__testmcp__standup", "mcp__testmcp__summarize"];
    assert.deepEqual([...order].sort((a, b) => at(a) - at(b)), order);
    assert.equal(byName.get("resume").opens, "terminal");
    assert.ok(byName.get("resume").aliases.includes("continue"));
    assert.equal(byName.get("model").opens, "model");
    assert.equal(byName.get("compact").opens, null);

    const state = await readCodexState({ bin: onPath("codex"), cwd, env, plugins: true });
    assert.ok(CODEX_VERSIONS.includes(state.version), `CODEX_BUILTINS is checked against Codex ${CODEX_VERSIONS.join(", ")}, not ${state.version}`);
    assert.equal(state.chatgpt, false);
    assert.equal(state.voice, codexHasVoice(codexPackageDir(onPath("codex"))));
    const models = JSON.parse(fs.readFileSync(path.join(env.CODEX_HOME, "models_cache.json"), "utf8")).models;
    const codex = buildCodexCommands({ ...state, models, model: "test-model", platform: "linux" });
    const codexNames = codex.map((c) => c.name);
    assert.deepEqual(codexNames.slice(0, 4), ["model", "fast", "presets", "ide"], "popup order, the model's tiers right after /model");
    assert.equal(codex[0].opens, "model");
    assert.equal(codex[1].description, "Quicker answers");
    assert.ok(codexNames.includes("plugins") && codexNames.includes("goal"), "Rudder starts Codex with plugins and goals on");
    assert.equal(codexNames.includes("voice"), state.voice);
    for (const absent of ["usage", "apps", "debug-config", "app", "prompts:standup", "standup"]) {
      assert.equal(codexNames.includes(absent), false, `/${absent} is not in this popup`);
    }
    assert.equal(codex.find((c) => c.name === "btw")?.alias, true);
    assert.throws(() => buildCodexCommands({ ...state, version: "0.0.1", models, model: "test-model" }), /this machine has Codex 0\.0\.1\. Install /);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
