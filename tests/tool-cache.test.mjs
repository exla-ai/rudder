import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { pruneToolCache, toolCacheEnv, toolCacheRoot } from "../dist/tool-cache.js";
import { codexShellEnvConfigArgs } from "../dist/codex-binary.js";
import { nativeAgentCommand } from "../dist/native-agents.js";

function tempDir(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-tool-cache-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

test("agents and the builder share one read-write sccache and the same stores", (t) => {
  const root = tempDir(t);
  const env = { HOME: path.join(root, "home") };
  const agent = toolCacheEnv("agent", { root, env, hasTool: () => true });
  const builder = toolCacheEnv("builder", { root, env, hasTool: () => true });
  assert.equal(agent.SCCACHE_LOCAL_RW_MODE, "READ_WRITE");
  assert.equal(builder.SCCACHE_LOCAL_RW_MODE, "READ_WRITE");
  assert.equal(agent.RUSTC_WRAPPER, "sccache");
  assert.equal(agent.SCCACHE_DIR, path.join(root, "sccache"));
  for (const key of ["npm_config_store_dir", "npm_config_cache", "UV_CACHE_DIR", "PIP_CACHE_DIR", "CARGO_HOME", "TURBO_CACHE_DIR", "NX_CACHE_DIRECTORY"]) {
    assert.equal(agent[key], builder[key]);
    assert.ok(fs.statSync(agent[key]).isDirectory(), key);
  }
});

test("the tool cache stays in one place, so CARGO_HOME never moves under a build", () => {
  const home = path.join("/home/u", ".cache", "rudder", "tool-cache");
  assert.equal(toolCacheRoot({ HOME: "/home/u" }), home);
  assert.equal(toolCacheRoot({ HOME: "/home/u", RUDDER_CACHE_DIR: "/workspace/cache/build" }), home, "a supervisor's cache dir does not move it");
  assert.equal(toolCacheRoot({ HOME: "/home/u", RUDDER_TOOL_CACHE_DIR: "/x" }), "/x");
});

test("sccache strips the build directory from its keys so workspaces share crates", (t) => {
  const root = tempDir(t);
  const env = { HOME: path.join(root, "home") };
  assert.equal(toolCacheEnv("agent", { root, env, hasTool: () => true, workspace: "/w/agent-1" }).SCCACHE_BASEDIRS, "/w/agent-1");
  assert.equal(toolCacheEnv("builder", { root, env, hasTool: () => true }).SCCACHE_BASEDIRS, undefined);
  assert.equal(toolCacheEnv("agent", { root, env, hasTool: (name) => name !== "sccache", workspace: "/w/a" }).SCCACHE_BASEDIRS, undefined);
});

test("missing tools are skipped", (t) => {
  const root = tempDir(t);
  const out = toolCacheEnv("agent", { root, env: { HOME: root }, hasTool: (name) => name === "npm" });
  assert.equal(out.RUSTC_WRAPPER, undefined);
  assert.equal(out.SCCACHE_DIR, undefined);
  assert.equal(out.UV_CACHE_DIR, undefined);
  assert.equal(out.CARGO_HOME, undefined);
  assert.equal(out.npm_config_cache, path.join(root, "npm"));
});

test("prune removes least recently used files until under the limit", (t) => {
  const root = tempDir(t);
  const dir = path.join(root, "npm");
  fs.mkdirSync(dir, { recursive: true });
  for (const [i, name] of ["old", "mid", "new"].entries()) {
    const file = path.join(dir, name);
    fs.writeFileSync(file, Buffer.alloc(100));
    const time = new Date(Date.now() - (3 - i) * 60_000);
    fs.utimesSync(file, time, time);
  }
  const freed = pruneToolCache(root, { npm: 250 });
  assert.equal(freed, 100);
  assert.deepEqual(fs.readdirSync(dir).sort(), ["mid", "new"]);
  assert.equal(pruneToolCache(root, { npm: 250 }), 0);
});

// Codex filters its shell tool's environment (`shell_environment_policy`,
// e.g. `inherit = "core"`), so the cache vars go into the policy itself.
test("Codex gets every tool cache variable through its shell environment policy", (t) => {
  const root = tempDir(t);
  const env = toolCacheEnv("agent", { root, env: { HOME: root }, hasTool: () => true });
  const args = codexShellEnvConfigArgs(env);
  assert.equal(args.length, Object.keys(env).length * 2);
  for (const [key, value] of Object.entries(env)) {
    assert.equal(args[args.indexOf(`shell_environment_policy.set.${key}=${JSON.stringify(value)}`) - 1], "-c", key);
  }
  // Values are TOML basic strings; names a config key cannot hold are skipped.
  assert.deepEqual(
    codexShellEnvConfigArgs({ A_DIR: 'C:\\x "y"', "bad.name": "z", "1X": "z" }),
    ["-c", 'shell_environment_policy.set.A_DIR="C:\\\\x \\"y\\""'],
  );
});

test("a Codex agent launch carries the tool cache into its shell policy", (t) => {
  const root = tempDir(t);
  const saved = process.env.RUDDER_TOOL_CACHE_DIR;
  process.env.RUDDER_TOOL_CACHE_DIR = root;
  try {
    const command = nativeAgentCommand({
      run: { id: "r", task: "t", backend: "codex", model: "gpt-5.5", workspace: { path: root } },
      prompt: "p",
      contract: "c",
    });
    assert.ok(command.includes(`shell_environment_policy.set.TURBO_CACHE_DIR="${path.join(root, "turbo")}"`), command);
  } finally {
    if (saved === undefined) delete process.env.RUDDER_TOOL_CACHE_DIR;
    else process.env.RUDDER_TOOL_CACHE_DIR = saved;
  }
});
