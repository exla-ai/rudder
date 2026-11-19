import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { discoverHarnesses, modelName, refreshModelCatalog } from "../dist/models.js";
import { harnessLogo } from "../cloud/app/harness-logos.js";

/** One harness's rows, as the pickers list them. */
async function modelsOf(id) {
  const harness = (await discoverHarnesses()).find((entry) => entry.id === id);
  assert.ok(harness, `no ${id} harness`);
  return harness.models;
}

async function withTempRudderHome(t) {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-models-"));
  const previousHome = process.env.RUDDER_HOME;
  const previousUserHome = process.env.HOME;
  const previousCodexHome = process.env.CODEX_HOME;
  process.env.RUDDER_HOME = root;
  // Model discovery also reads ~/.codex and ~/.claude/projects. Keep those
  // lookups in the same disposable home instead of scanning the real account.
  process.env.HOME = root;
  delete process.env.CODEX_HOME;
  t.after(async () => {
    if (previousHome === undefined) delete process.env.RUDDER_HOME;
    else process.env.RUDDER_HOME = previousHome;
    if (previousUserHome === undefined) delete process.env.HOME;
    else process.env.HOME = previousUserHome;
    if (previousCodexHome === undefined) delete process.env.CODEX_HOME;
    else process.env.CODEX_HOME = previousCodexHome;
    await fsp.rm(root, { recursive: true, force: true }).catch(() => {});
  });
  return root;
}

/** A `claude` that answers the headless initialize request with `models`. */
async function claudeCodeStub(root, models) {
  const stub = path.join(root, "claude");
  await fsp.writeFile(stub, `#!/usr/bin/env node
process.stdin.once("data", (line) => {
  const { request_id } = JSON.parse(String(line));
  process.stdout.write(JSON.stringify({ type: "control_response", response: { subtype: "success", request_id, response: { models: ${JSON.stringify(models)} } } }) + "\\n");
});
`, { mode: 0o755 });
  return stub;
}

function jsonResponse(body) {
  return {
    ok: true,
    json: async () => body,
  };
}

test("the provider APIs feed the catalog, and the pickers list only what each CLI lists", async (t) => {
  const rudderHome = await withTempRudderHome(t);
  assert.equal(os.homedir(), rudderHome);
  const oldFetch = globalThis.fetch;
  const oldOpenAI = process.env.OPENAI_API_KEY;
  const oldAnthropic = process.env.ANTHROPIC_API_KEY;
  process.env.OPENAI_API_KEY = "test-openai-key";
  process.env.ANTHROPIC_API_KEY = "test-anthropic-key";
  t.after(() => {
    globalThis.fetch = oldFetch;
    if (oldOpenAI === undefined) delete process.env.OPENAI_API_KEY;
    else process.env.OPENAI_API_KEY = oldOpenAI;
    if (oldAnthropic === undefined) delete process.env.ANTHROPIC_API_KEY;
    else process.env.ANTHROPIC_API_KEY = oldAnthropic;
  });

  globalThis.fetch = async (url) => {
    const href = String(url);
    if (href === "https://models.dev/api.json") {
      return jsonResponse({
        openai: { id: "openai", models: { "gpt-5.5": { tool_call: true, reasoning: true } } },
        anthropic: { id: "anthropic", models: { "claude-sonnet-4-6": { tool_call: true, reasoning: true } } },
      });
    }
    if (href === "https://api.openai.com/v1/models") {
      return jsonResponse({
        object: "list",
        data: [
          { id: "gpt-6-codex-preview", object: "model", created: 1780000000, owned_by: "openai" },
          { id: "gpt-image-2", object: "model", created: 1780000000, owned_by: "openai" },
        ],
      });
    }
    if (href.startsWith("https://api.anthropic.com/v1/models")) {
      return jsonResponse({
        data: [
          { id: "claude-oracle-6-20260703", display_name: "Claude Oracle 6", created_at: "2026-07-03T00:00:00Z" },
        ],
      });
    }
    throw new Error(`unexpected fetch ${href}`);
  };

  const codex = await modelsOf("codex");
  const claude = await modelsOf("claude");

  // The pickers list only what each CLI lists; the APIs feed the catalog.
  assert.ok(!codex.some((option) => option.value === "gpt-6-codex-preview"), "a catalog model Codex does not list makes no row");
  assert.ok(!claude.some((option) => option.value === "claude-oracle-6-20260703"), "a catalog model Claude Code does not list makes no row");

  const cache = JSON.parse(await fsp.readFile(path.join(rudderHome, "models-dev.json"), "utf8"));
  assert.ok(cache.openai.models["gpt-6-codex-preview"], "OpenAI API model is written for native task pane");
  assert.ok(cache.anthropic.models["claude-oracle-6-20260703"], "Anthropic API model is written for native task pane");
});

test("Codex subscription OAuth stays off public API discovery and uses the selected local home", async t => {
  const root = await withTempRudderHome(t);
  const local = path.join(root, "selected-codex-home");
  process.env.CODEX_HOME = local;
  await fsp.mkdir(local);
  await fsp.writeFile(path.join(local, "models_cache.json"), JSON.stringify({ models: [
    { slug: "gpt-account-specific", display_name: "Account model" },
    { slug: "gpt-hidden", visibility: "hide" },
  ] }));
  await fsp.writeFile(path.join(root, "auth-profiles.json"), JSON.stringify({ version: 1, profiles: {
    "openai-codex:default": { type: "oauth", provider: "openai-codex", access: "subscription-fixture-bearer", refresh: "fixture-refresh", expires: Date.now() + 60000 },
  } }));
  const oldFetch = globalThis.fetch, oldKey = process.env.OPENAI_API_KEY;
  delete process.env.OPENAI_API_KEY;
  t.after(() => { globalThis.fetch = oldFetch; if (oldKey === undefined) delete process.env.OPENAI_API_KEY; else process.env.OPENAI_API_KEY = oldKey; });
  const calls = [];
  globalThis.fetch = async (url, init) => {
    calls.push(String(url));
    assert.ok(!JSON.stringify(init).includes("subscription-fixture-bearer"));
    return jsonResponse({});
  };
  const options = await modelsOf("codex");
  assert.ok(options.some(row => row.value === "gpt-account-specific"));
  assert.ok(!options.some(row => row.value === "gpt-hidden"));
  assert.ok(!calls.includes("https://api.openai.com/v1/models"));
});

test("the picker sorts models by version, then tier, names them and offers only efforts both the CLI and the dashboard take", async (t) => {
  const root = await withTempRudderHome(t);
  // Claude Code's own list (its headless initialize answer), in its own order.
  const all = ["low", "medium", "high", "xhigh", "max"];
  const claudeModels = [
    { value: "default", resolvedModel: "claude-opus-5-5", displayName: "Default (recommended)", supportsEffort: true, supportedEffortLevels: all },
    { value: "opus", resolvedModel: "claude-opus-5-5", displayName: "Opus 5.5", description: "For complex work", supportsEffort: true, supportedEffortLevels: all },
    { value: "opus[1m]", resolvedModel: "claude-opus-5-5[1m]", displayName: "Opus 5.5 (1M context)", supportsEffort: true, supportedEffortLevels: all },
    { value: "fable", resolvedModel: "claude-fable-5-2", displayName: "Fable 5.2", description: "For your toughest challenges", supportsEffort: true, supportedEffortLevels: all },
    { value: "sonnet", resolvedModel: "claude-sonnet-5-5", displayName: "Sonnet 5.5", supportsEffort: true, supportedEffortLevels: all },
    { value: "haiku", resolvedModel: "claude-haiku-5-5", displayName: "Haiku 5.5", supportsEffort: true, supportedEffortLevels: all },
    { value: "claude-sonnet-5", resolvedModel: "claude-sonnet-5", displayName: "Sonnet 5", supportsEffort: true, supportedEffortLevels: all },
    { value: "claude-opus-5", resolvedModel: "claude-opus-5", displayName: "Opus 5", supportsEffort: true, supportedEffortLevels: all },
    { value: "claude-fable-5", resolvedModel: "claude-fable-5", displayName: "Fable 5", supportsEffort: true, supportedEffortLevels: all },
    { value: "claude-opus-4-6", resolvedModel: "claude-opus-4-6", displayName: "Opus 4.6", supportsEffort: true, supportedEffortLevels: ["low", "medium", "high", "max"] },
    { value: "claude-haiku-4-5-20251001", resolvedModel: "claude-haiku-4-5-20251001", displayName: "Haiku 4.5" },
  ];
  const stub = await claudeCodeStub(root, claudeModels);
  // A model this machine ran lately, which the catalog names the picker's way.
  await fsp.mkdir(path.join(root, ".claude", "projects", "p"), { recursive: true });
  await fsp.writeFile(path.join(root, ".claude", "projects", "p", "s.jsonl"), `${JSON.stringify({ message: { model: "claude-fable-5-2" } })}\n`);
  process.env.CODEX_HOME = path.join(root, "codex");
  await fsp.mkdir(process.env.CODEX_HOME);
  const levels = (...efforts) => efforts.map((effort) => ({ effort }));
  // Codex's own list order is not a tier order (6.1 Terra before 6.1 Sol).
  await fsp.writeFile(path.join(process.env.CODEX_HOME, "models_cache.json"), JSON.stringify({ models: [
    { slug: "gpt-6.2-sol", display_name: "GPT-6.2-Sol", description: "Fast", supported_reasoning_levels: levels("low", "medium", "high", "xhigh", "max", "ultra") },
    { slug: "gpt-6.2-astra", display_name: "GPT-6.2-Astra", supported_reasoning_levels: levels("low", "medium") },
    { slug: "gpt-6.1-terra", display_name: "GPT-6.1 Terra", supported_reasoning_levels: levels("low", "high") },
    { slug: "gpt-6.1-sol", display_name: "GPT-6.1-Sol", supported_reasoning_levels: levels("low", "high") },
    { slug: "gpt-6-luna", display_name: "GPT-6-Luna", supported_reasoning_levels: levels("low", "high") },
  ] }));
  const oldBin = process.env.RUDDER_CLAUDE_BIN, oldFetch = globalThis.fetch, oldOpenAI = process.env.OPENAI_API_KEY, oldAnthropic = process.env.ANTHROPIC_API_KEY;
  process.env.RUDDER_CLAUDE_BIN = stub;
  delete process.env.OPENAI_API_KEY;
  delete process.env.ANTHROPIC_API_KEY;
  t.after(() => {
    globalThis.fetch = oldFetch;
    for (const [key, value] of [["RUDDER_CLAUDE_BIN", oldBin], ["OPENAI_API_KEY", oldOpenAI], ["ANTHROPIC_API_KEY", oldAnthropic]]) {
      if (value === undefined) delete process.env[key]; else process.env[key] = value;
    }
  });
  const priced = (output, extra = {}) => ({ tool_call: true, cost: { output }, ...extra });
  globalThis.fetch = async (url) => {
    assert.equal(String(url), "https://models.dev/api.json");
    return jsonResponse({
      openai: { id: "openai", models: {
        "gpt-6.2-sol": priced(10), "gpt-6.2-astra": priced(40), "gpt-6.1-terra": priced(5), "gpt-6.1-sol": priced(10), "gpt-6-luna": priced(0.5, { limit: { context: 1050000 } }),
        "gpt-5.5": priced(30, { limit: { context: 1050000 } }),
      } },
      anthropic: { id: "anthropic", models: {
        "claude-fable-5-2": priced(50), "claude-opus-5-5": priced(25), "claude-sonnet-5-5": priced(15), "claude-haiku-5-5": priced(5, { reasoning: false }),
        "claude-fable-5": priced(50), "claude-opus-5": priced(25), "claude-sonnet-5": priced(15),
        "claude-haiku-4-5": priced(5), "claude-opus-4-5": priced(25), "claude-opus-4-5-20251101": priced(25),
      } },
    });
  };

  const harnesses = await discoverHarnesses();
  assert.deepEqual(harnesses.map((h) => [h.id, h.label, h.account]), [["claude", "Claude", "Claude Code"], ["codex", "Codex", "ChatGPT"]]);
  const claude = harnesses[0].models;
  // Exactly Claude Code's models (no catalog-only Opus 4.5), the name and then
  // the version, the highest version first, Claude Code's family order within
  // one (Opus, Fable, Sonnet, Haiku, though Fable costs more); an alias reads
  // and sorts like the model it stands for.
  assert.deepEqual(claude.map((row) => [row.value, row.label, !!row.alias]), [
    ["claude-opus-5-5", "Opus 5.5", false],
    ["claude-opus-5-5[1m]", "Opus 5.5 · 1M", false],
    ["claude-sonnet-5-5", "Sonnet 5.5", false],
    ["claude-haiku-5-5", "Haiku 5.5", false],
    ["claude-fable-5-2", "Fable 5.2", false],
    ["claude-opus-5", "Opus 5", false],
    ["claude-fable-5", "Fable 5", false],
    ["claude-sonnet-5", "Sonnet 5", false],
    ["claude-opus-4-6", "Opus 4.6", false],
    ["claude-haiku-4-5-20251001", "Haiku 4.5", false],
    ["opus", "Opus 5.5", true],
    ["opus[1m]", "Opus 5.5 · 1M", true],
    ["sonnet", "Sonnet 5.5", true],
    ["haiku", "Haiku 5.5", true],
    ["fable", "Fable 5.2", true],
  ]);
  const catalog = JSON.parse(await fsp.readFile(path.join(root, "models-dev.json"), "utf8"));
  assert.equal(catalog.anthropic.models["claude-fable-5-2"].name, "Fable 5.2", "the dashboard's detail line reads the same name");
  const efforts = (rows, value) => rows.find((row) => row.value === value).efforts;
  assert.deepEqual(efforts(claude, "claude-opus-5-5"), all);
  assert.deepEqual(efforts(claude, "claude-opus-4-6"), ["low", "medium", "high", "max"], "Claude Code's own levels");
  assert.deepEqual(efforts(claude, "claude-haiku-4-5-20251001"), [], "Claude Code lists no effort for it");
  assert.deepEqual(efforts(claude, "claude-haiku-5-5"), all, "Claude Code's levels, though the catalog calls it non-reasoning");
  assert.deepEqual(efforts(claude, "haiku"), all);
  assert.equal(claude.find((row) => row.value === "opus").detail, "For complex work");

  // Exactly Codex's models (no catalog-only GPT-5.5), named the name and then
  // the version ("GPT-6.2-Astra" reads "Astra 6.2"), the highest version
  // first, the priciest family first within one.
  const codex = await modelsOf("codex");
  assert.deepEqual(codex.map((row) => [row.value, row.label]), [
    ["gpt-6.2-astra", "Astra 6.2"],
    ["gpt-6.2-sol", "Sol 6.2"],
    ["gpt-6.1-sol", "Sol 6.1"],
    ["gpt-6.1-terra", "Terra 6.1"],
    ["gpt-6-luna", "Luna 6"],
  ]);
  assert.deepEqual(efforts(codex, "gpt-6.2-sol"), ["low", "medium", "high", "xhigh"], "no max or ultra: the dashboard passes neither to Codex");
  assert.deepEqual(efforts(codex, "gpt-6.2-astra"), ["low", "medium"]);
  assert.equal(codex.find((row) => row.value === "gpt-6-luna").detail, "1.1M context", "the catalog's detail when Codex gives no description");

  // The name the app shows for any model an agent runs on (the home sends it
  // with each agent): a listed model as its row reads, an alias like the model
  // it stands for, any other id by the rows' rules, never the raw id.
  assert.equal(modelName(harnesses, "claude", "opus"), "Opus 5.5");
  assert.equal(modelName(harnesses, "claude", "claude-opus-4-1-20250805"), "Opus 4.1", "a model Claude Code no longer lists");
  assert.equal(modelName(harnesses, "codex", "gpt-5.5"), "GPT-5.5", "a model Codex no longer lists");
  assert.equal(modelName([], "claude", "opus[1m]"), "Opus · 1M", "before the first list, an alias reads by its shape");
  assert.equal(modelName(harnesses, "codex", "my_custom-model"), "My Custom Model", "an id of no known shape reads as its words");
  assert.equal(modelName(harnesses, "opencode", "anthropic/claude-sonnet"), "Claude Sonnet", "a harness without a picker section");
  // Model chips show the harness's logo in place of its name.
  for (const harness of harnesses) {
    assert.match(harnessLogo(harness.id), /^<svg /, `${harness.label} needs a logo in cloud/app/harness-logos.js`);
  }
});

test("an expired stored login is not sent, and a refused key fails discovery with the reason", async (t) => {
  const root = await withTempRudderHome(t);
  await fsp.writeFile(path.join(root, "auth-profiles.json"), JSON.stringify({ version: 1, profiles: {
    "anthropic:claude-code": { type: "oauth", provider: "anthropic", access: "expired-fixture-bearer", refresh: "r", expires: Date.now() - 1000 },
  } }));
  const oldFetch = globalThis.fetch, oldAnthropic = process.env.ANTHROPIC_API_KEY, oldOpenAI = process.env.OPENAI_API_KEY;
  delete process.env.ANTHROPIC_API_KEY;
  delete process.env.OPENAI_API_KEY;
  t.after(() => {
    globalThis.fetch = oldFetch;
    if (oldAnthropic === undefined) delete process.env.ANTHROPIC_API_KEY; else process.env.ANTHROPIC_API_KEY = oldAnthropic;
    if (oldOpenAI === undefined) delete process.env.OPENAI_API_KEY; else process.env.OPENAI_API_KEY = oldOpenAI;
  });
  globalThis.fetch = async (url, init) => {
    assert.ok(!JSON.stringify(init ?? {}).includes("expired-fixture-bearer"), "expired login was sent");
    if (String(url).startsWith("https://api.anthropic.com/")) {
      return { ok: false, status: 401, text: async () => '{"error":{"message":"invalid x-api-key"}}' };
    }
    return jsonResponse({});
  };
  await discoverHarnesses();

  process.env.ANTHROPIC_API_KEY = "refused-key";
  await assert.rejects(discoverHarnesses(), /api\.anthropic\.com\/v1\/models.* returned 401: .*invalid x-api-key/);
});

test("a model only Claude Code lists lands in the catalog the dashboard reads, with exactly its efforts", async (t) => {
  const root = await withTempRudderHome(t);
  const all = ["low", "medium", "high", "xhigh", "max"];
  const stub = await claudeCodeStub(root, [
    { value: "haiku", resolvedModel: "claude-haiku-5-5", displayName: "Haiku 5.5", supportsEffort: true, supportedEffortLevels: all },
    { value: "claude-brandnew-6", resolvedModel: "claude-brandnew-6", displayName: "Brandnew 6", supportsEffort: true, supportedEffortLevels: ["low", "high", "ultra"] },
    { value: "claude-haiku-4-5-20251001", resolvedModel: "claude-haiku-4-5-20251001", displayName: "Haiku 4.5" },
  ]);
  const oldBin = process.env.RUDDER_CLAUDE_BIN, oldFetch = globalThis.fetch, oldOpenAI = process.env.OPENAI_API_KEY, oldAnthropic = process.env.ANTHROPIC_API_KEY;
  process.env.RUDDER_CLAUDE_BIN = stub;
  delete process.env.OPENAI_API_KEY;
  delete process.env.ANTHROPIC_API_KEY;
  t.after(() => {
    globalThis.fetch = oldFetch;
    for (const [key, value] of [["RUDDER_CLAUDE_BIN", oldBin], ["OPENAI_API_KEY", oldOpenAI], ["ANTHROPIC_API_KEY", oldAnthropic]]) {
      if (value === undefined) delete process.env[key]; else process.env[key] = value;
    }
  });
  globalThis.fetch = async () => jsonResponse({
    anthropic: { id: "anthropic", models: {
      "claude-haiku-5-5": { tool_call: true, reasoning: false, cost: { output: 5 } },
      "claude-haiku-4-5": { tool_call: true, reasoning: true },
    } },
  });

  await refreshModelCatalog();
  const models = JSON.parse(await fsp.readFile(path.join(root, "models-dev.json"), "utf8")).anthropic.models;
  assert.deepEqual(models["claude-brandnew-6"], {
    id: "claude-brandnew-6", name: "Brandnew 6", tool_call: true, efforts: ["low", "high"], modalities: { output: ["text"] }, source: "claude-code",
  });
  assert.deepEqual(models["claude-haiku-5-5"].efforts, all);
  assert.equal(models["claude-haiku-5-5"].cost.output, 5, "the catalog's own fields stay");
  assert.deepEqual(models.haiku.efforts, all, "an alias carries its levels too");
  assert.deepEqual(models["claude-haiku-4-5-20251001"].efforts, []);
  assert.deepEqual(models["claude-haiku-4-5"].efforts, [], "the undated twin the pickers list takes the dated id's levels");
});
