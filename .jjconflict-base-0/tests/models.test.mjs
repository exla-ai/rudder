import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { discoverModelOptions, fallbackModelOptions } from "../dist/models.js";

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

test("Claude fallback choices mirror Claude Code without synthetic 1m aliases", () => {
  const options = fallbackModelOptions("claude");
  assert.deepEqual(
    options.map((option) => option.value),
    [undefined, "opus", "fable", "sonnet", "haiku"],
  );
  assert.match(options.find((option) => option.value === "opus")?.detail ?? "", /1M context/);
  assert.ok(!options.some((option) => option.value?.includes("[1m]")));
});
