import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { discoverModelOptions, refreshModelCatalog } from "../dist/models.js";

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

test("model discovery merges account-specific OpenAI and Anthropic model APIs ahead of fallbacks", async (t) => {
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

  const codex = await discoverModelOptions("codex");
  const claude = await discoverModelOptions("claude");

  assert.equal(codex[1]?.value, "gpt-6-codex-preview", "new account Codex model ranks before catalog fallback");
  assert.ok(!codex.some((option) => option.value === "gpt-image-2"), "non-text OpenAI models stay out of Codex picker");
  const oracleIndex = claude.findIndex((option) => option.value === "claude-oracle-6-20260703");
  const aliasIndex = claude.findIndex((option) => option.value === "fable");
  assert.ok(oracleIndex > 0, "new account Claude model is present");
  assert.ok(aliasIndex === -1 || oracleIndex < aliasIndex, "new account Claude model ranks before static aliases");

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
  const options = await discoverModelOptions("codex");
  assert.ok(options.some(row => row.value === "gpt-account-specific"));
  assert.ok(!options.some(row => row.value === "gpt-hidden"));
  assert.ok(!calls.includes("https://api.openai.com/v1/models"));
});

test("the picker groups models by generation, names them and offers only efforts both the CLI and the dashboard take", async (t) => {
  const root = await withTempRudderHome(t);
  // Claude Code's own list: its headless initialize answer.
  const all = ["low", "medium", "high", "xhigh", "max"];
  const claudeModels = [
    { value: "default", resolvedModel: "claude-opus-5-5", displayName: "Default (recommended)", supportsEffort: true, supportedEffortLevels: all },
    { value: "opus", resolvedModel: "claude-opus-5-5", displayName: "Opus 5.5", description: "For complex work", supportsEffort: true, supportedEffortLevels: all },
    { value: "opus[1m]", resolvedModel: "claude-opus-5-5[1m]", displayName: "Opus 5.5 (1M context)", supportsEffort: true, supportedEffortLevels: all },
    { value: "haiku", resolvedModel: "claude-haiku-5-5", displayName: "Haiku 5.5", supportsEffort: true, supportedEffortLevels: all },
    { value: "claude-opus-4-6", resolvedModel: "claude-opus-4-6", displayName: "Opus 4.6", supportsEffort: true, supportedEffortLevels: ["low", "medium", "high", "max"] },
    { value: "claude-haiku-4-5-20251001", resolvedModel: "claude-haiku-4-5-20251001", displayName: "Haiku 4.5" },
  ];
  const stub = await claudeCodeStub(root, claudeModels);
  process.env.CODEX_HOME = path.join(root, "codex");
  await fsp.mkdir(process.env.CODEX_HOME);
  const levels = (...efforts) => efforts.map((effort) => ({ effort }));
  await fsp.writeFile(path.join(process.env.CODEX_HOME, "models_cache.json"), JSON.stringify({ models: [
    { slug: "gpt-6.2-sol", display_name: "GPT-6.2-Sol", description: "Fast", supported_reasoning_levels: levels("low", "medium", "high", "xhigh", "max", "ultra") },
    { slug: "gpt-6.2-astra", display_name: "GPT-6.2-Astra", supported_reasoning_levels: levels("low", "medium") },
    { slug: "gpt-6.1-sol", display_name: "GPT-6.1 Sol", supported_reasoning_levels: levels("low", "high") },
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
      openai: { id: "openai", models: { "gpt-6.2-sol": priced(10), "gpt-6.2-astra": priced(40), "gpt-6.1-sol": priced(10), "gpt-5.5": priced(30, { limit: { context: 1050000 } }) } },
      anthropic: { id: "anthropic", models: {
        "claude-fable-5-2": priced(50), "claude-opus-5-5": priced(25), "claude-sonnet-5-5": priced(15), "claude-haiku-5-5": priced(5, { reasoning: false }),
        "claude-haiku-4-5": priced(5), "claude-opus-4-5": priced(25), "claude-opus-4-5-20251101": priced(25),
      } },
    });
  };

  const claude = (await discoverModelOptions("claude")).slice(1);
  const groups = (rows) => [...new Set(rows.map((row) => row.group))];
  assert.deepEqual(groups(claude), ["Claude 5.5", "Claude 5.2", "Claude 4.6", "Claude 4.5", "Always the newest"]);
  assert.deepEqual(claude.map((row) => [row.value, row.label]), [
    ["claude-opus-5-5", "Opus 5.5"],
    ["claude-opus-5-5[1m]", "Opus 5.5 · 1M"],
    ["claude-sonnet-5-5", "Sonnet 5.5"],
    ["claude-haiku-5-5", "Haiku 5.5"],
    ["claude-fable-5-2", "Fable 5.2"],
    ["claude-opus-4-6", "Opus 4.6"],
    ["claude-opus-4-5", "Opus 4.5"],
    ["claude-haiku-4-5", "Haiku 4.5"],
    ["opus", "Opus"],
    ["opus[1m]", "Opus · 1M"],
    ["haiku", "Haiku"],
  ]);
  const efforts = (rows, value) => rows.find((row) => row.value === value).efforts;
  assert.deepEqual(efforts(claude, "claude-opus-5-5"), all);
  assert.deepEqual(efforts(claude, "claude-opus-4-6"), ["low", "medium", "high", "max"], "Claude Code's own levels");
  assert.deepEqual(efforts(claude, "claude-haiku-4-5"), [], "Claude Code lists no effort for it");
  assert.deepEqual(efforts(claude, "claude-haiku-5-5"), all, "Claude Code's levels, though the catalog calls it non-reasoning");
  assert.deepEqual(efforts(claude, "haiku"), all);
  assert.match(claude.find((row) => row.value === "opus").detail, /^Now Opus 5\.5 · For complex work$/);

  const codex = (await discoverModelOptions("codex")).slice(1);
  assert.deepEqual(codex.map((row) => [row.group, row.label]), [
    ["GPT-6.2", "GPT-6.2 Astra"],
    ["GPT-6.2", "GPT-6.2 Sol"],
    ["GPT-6.1", "GPT-6.1 Sol"],
    ["GPT-5.5", "GPT-5.5"],
  ]);
  assert.deepEqual(efforts(codex, "gpt-6.2-sol"), ["low", "medium", "high", "xhigh"], "no max or ultra: the dashboard passes neither to Codex");
  assert.deepEqual(efforts(codex, "gpt-6.2-astra"), ["low", "medium"]);
  assert.deepEqual(efforts(codex, "gpt-5.5"), ["low", "medium", "high", "xhigh"], "a catalog model takes the dashboard's levels");
  assert.equal(codex.find((row) => row.value === "gpt-5.5").detail, "1.1M context");
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
  assert.ok((await discoverModelOptions("claude")).length > 1);

  process.env.ANTHROPIC_API_KEY = "refused-key";
  await assert.rejects(discoverModelOptions("claude"), /api\.anthropic\.com\/v1\/models.* returned 401: .*invalid x-api-key/);
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
