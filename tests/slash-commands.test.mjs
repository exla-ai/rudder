// The "/" lists of an agent's CLI (cloud/worker/slash-commands.mjs), built
// from synthetic config folders with the installed claude and codex. Neither
// folder holds a login, so no model can be called.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  CLAUDE_CLASSIFIED_FOR, CODEX_VERSION, buildClaudeCommands, buildCodexCommands, claudeInitialize,
  readClaudeBinaryCommands, readCodexState,
} from "../cloud/worker/slash-commands.mjs";

function onPath(name) {
  for (const dir of String(process.env.PATH).split(path.delimiter)) {
    const file = path.join(dir, name);
    if (fs.existsSync(file)) return file;
  }
  throw new Error(`${name} must be installed for this test`);
}

function writeFixture(root) {
  const write = (rel, body) => {
    const file = path.join(root, rel);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, typeof body === "string" ? body : JSON.stringify(body, null, 2));
  };
  const claude = "home/.claude";
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
  write(`${claude}/settings.json`, { enabledPlugins: { "greeter@testmkt": true } });
  write("project/.claude/commands/ship.md", "---\ndescription: Ship this project\n---\nShip\n");
  // Codex: a catalog whose model has a Fast tier, and a custom prompt that
  // Codex 0.169's "/" popup does not list.
  write("home/.codex/config.toml", 'model = "test-model"\n');
  write("home/.codex/models_cache.json", { models: [{ slug: "test-model", service_tiers: [{ id: "priority", name: "Fast", description: "Quicker answers" }], available_access_programs: { cyber: ["standard"] } }] });
  write("home/.codex/prompts/standup.md", "---\ndescription: Write my standup\n---\nWrite it\n");
}

test("Claude and Codex '/' lists from synthetic config folders", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-slash-"));
  try {
    writeFixture(root);
    const env = {
      PATH: process.env.PATH,
      HOME: path.join(root, "home"),
      CLAUDE_CONFIG_DIR: path.join(root, "home/.claude"),
      CODEX_HOME: path.join(root, "home/.codex"),
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
    };
    const cwd = path.join(root, "project");

    const claudeBin = onPath("claude");
    const [answer, binary] = await Promise.all([
      claudeInitialize({ bin: claudeBin, cwd, env }),
      readClaudeBinaryCommands(fs.realpathSync(claudeBin)),
    ]);
    const { commands, unclassified } = buildClaudeCommands({ headless: answer.commands, binary });
    const byName = new Map(commands.map((c) => [c.name, c]));
    assert.deepEqual(byName.get("hello"), { name: "hello", description: "Say hello to someone (user)", hint: "<name>", args: "required", opens: null, aliases: [] });
    assert.equal(byName.get("ops:deploy")?.description, "Deploy the service (user)", "a subfolder namespaces the command");
    assert.equal(byName.get("tidy")?.description, "Tidy the working tree (user)");
    assert.equal(byName.has("model-only"), false, "a skill that is not user-invocable stays out");
    assert.deepEqual(byName.get("greeter:greet"), { name: "greeter:greet", description: "(greeter) Greet the team", hint: "[team]", args: "optional", opens: null, aliases: [] });
    assert.equal(byName.get("ship")?.description, "Ship this project (project)");
    // Built-ins: the headless ones, the TUI's own (open in the terminal), and
    // model and effort, which open the chat's model sheet.
    assert.equal(byName.get("compact")?.opens, null);
    assert.equal(byName.get("resume")?.opens, "terminal");
    assert.equal(byName.get("permissions")?.opens, "terminal");
    assert.equal(byName.get("context")?.description, "Show what fills the context", "the TUI's half of a twin");
    assert.equal(byName.get("model")?.opens, "model");
    assert.equal(byName.get("effort")?.opens, "model");
    assert.equal(byName.has("__remote-workflow"), false, "hidden built-ins stay out");
    assert.deepEqual(commands.map((c) => c.name), [...commands.map((c) => c.name)].sort((a, b) => a.localeCompare(b)));
    if (answer.version === CLAUDE_CLASSIFIED_FOR) {
      assert.deepEqual(unclassified, [], `Claude Code ${answer.version} built-ins Rudder has not classified`);
    }

    const state = await readCodexState({ bin: onPath("codex"), cwd, env, plugins: true });
    assert.equal(state.version, CODEX_VERSION, "CODEX_BUILTINS is checked against this Codex");
    assert.equal(state.chatgpt, false);
    const models = JSON.parse(fs.readFileSync(path.join(env.CODEX_HOME, "models_cache.json"), "utf8")).models;
    const codex = buildCodexCommands({ ...state, models, model: "test-model", platform: "linux" });
    const names = codex.map((c) => c.name);
    assert.deepEqual(names.slice(0, 4), ["model", "fast", "presets", "ide"], "popup order, the model's tiers right after /model");
    assert.equal(codex[0].opens, "model");
    assert.equal(codex[1].description, "Quicker answers");
    assert.ok(names.includes("plugins") && names.includes("goal"), "Rudder starts Codex with plugins and goals on");
    for (const absent of ["usage", "apps", "debug-config", "app", "voice", "prompts:standup", "standup"]) {
      assert.equal(names.includes(absent), false, `/${absent} is not in this popup`);
    }
    assert.equal(codex.find((c) => c.name === "btw")?.alias, true);
    assert.throws(() => buildCodexCommands({ ...state, version: "0.170.0", models, model: "test-model" }), /Codex 0\.169\.0; this machine has Codex 0\.170\.0/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
