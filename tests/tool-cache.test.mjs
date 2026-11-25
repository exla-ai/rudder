import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { pruneToolCache, toolCacheEnv, toolCacheRoot, toolCacheUsage } from "../dist/tool-cache.js";
import { codexShellEnvConfigArgs } from "../dist/codex-config.js";
import { nativeAgentCommand } from "../dist/native-agents.js";

function tempDir(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-tool-cache-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

test("agents and the builder share one read-write sccache and the same stores", (t) => {
  const root = tempDir(t);
  const env = { HOME: path.join(root, "home"), JJ_CONFIG: "/u/jj.toml" };
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
  const env = { HOME: path.join(root, "home"), JJ_CONFIG: "/u/jj.toml" };
  assert.equal(toolCacheEnv("agent", { root, env, hasTool: () => true, workspace: "/w/agent-1" }).SCCACHE_BASEDIRS, "/w/agent-1");
  assert.equal(toolCacheEnv("builder", { root, env, hasTool: () => true }).SCCACHE_BASEDIRS, undefined);
  assert.equal(toolCacheEnv("agent", { root, env, hasTool: (name) => name !== "sccache", workspace: "/w/a" }).SCCACHE_BASEDIRS, undefined);
});

// Antigravity runs commands in a terminal, where a pager waits for a key.
test("agents never page: git and jj print straight through, the user's jj config still loads", (t) => {
  const root = tempDir(t);
  const user = ["/u/.config/jj/config.toml", "/u/.config/jj/conf.d"];
  const env = { HOME: root, JJ_CONFIG: user.join(path.delimiter) };
  const agent = toolCacheEnv("agent", { root, env, hasTool: () => true });
  const file = path.join(root, "jj-agent.toml");
  assert.equal(agent.PAGER, "cat");
  assert.equal(agent.GIT_PAGER, "cat");
  assert.equal(agent.JJ_CONFIG, [...user, file].join(path.delimiter));
  assert.equal(fs.readFileSync(file, "utf8"), '[ui]\npaginate = "never"\n');
  // A launch from inside an agent lists the file once.
  assert.equal(toolCacheEnv("agent", { root, env: { ...env, JJ_CONFIG: agent.JJ_CONFIG }, hasTool: () => true }).JJ_CONFIG, agent.JJ_CONFIG);
  assert.equal(toolCacheEnv("builder", { root, env, hasTool: () => true }).PAGER, undefined);
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
  const env = toolCacheEnv("agent", { root, env: { HOME: root, JJ_CONFIG: "/u/jj.toml" }, hasTool: () => true });
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

test("remote cache env configures sccache multilevel webdav and Turborepo when proxy url and token are set", (t) => {
  const root = tempDir(t);
  const env = {
    HOME: path.join(root, "home"),
    JJ_CONFIG: "/u/jj.toml",
    RUDDER_REMOTE_CACHE_URL: "  http://127.0.0.1:41234/  ",
    RUDDER_REMOTE_CACHE_TOKEN: "  rdrc_test_token  ",
    RUDDER_BURST_SOCKET: "/run/rudder/burst.sock",
  };
  for (const role of ["agent", "builder"]) {
    const out = toolCacheEnv(role, { root, env, hasTool: () => true });
    assert.equal(out.SCCACHE_MULTILEVEL_CHAIN, "disk,webdav");
    assert.equal(out.SCCACHE_WEBDAV_ENDPOINT, "http://127.0.0.1:41234/sccache");
    assert.equal(out.SCCACHE_WEBDAV_TOKEN, "rdrc_test_token");
    assert.equal(out.SCCACHE_WEBDAV_RW_MODE, "READ_WRITE");
    assert.equal(out.SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY, "ignore");
    assert.equal(out.TURBO_API, "http://127.0.0.1:41234/turbo");
    assert.equal(out.TURBO_TOKEN, "rdrc_test_token");
    assert.equal(out.TURBO_TEAM, "rudder");
  }

  // Without sccache installed, only Turborepo remote vars are set.
  const noSccache = toolCacheEnv("agent", { root, env, hasTool: (name) => name !== "sccache" });
  assert.equal(noSccache.SCCACHE_MULTILEVEL_CHAIN, undefined);
  assert.equal(noSccache.SCCACHE_WEBDAV_ENDPOINT, undefined);
  assert.equal(noSccache.TURBO_API, "http://127.0.0.1:41234/turbo");
  assert.equal(noSccache.TURBO_TOKEN, "rdrc_test_token");
  assert.equal(noSccache.TURBO_TEAM, "rudder");

  // Explicitly disabled or missing url/token leaves remote vars unset.
  for (const disabled of [
    { ...env, RUDDER_REMOTE_CACHE: "0" },
    { ...env, RUDDER_REMOTE_CACHE_URL: "   " },
    { ...env, RUDDER_REMOTE_CACHE_TOKEN: "" },
  ]) {
    const out = toolCacheEnv("agent", { root, env: disabled, hasTool: () => true });
    assert.equal(out.SCCACHE_MULTILEVEL_CHAIN, undefined);
    assert.equal(out.TURBO_API, undefined);
    assert.equal(out.TURBO_TOKEN, undefined);
    assert.equal(out.TURBO_TEAM, undefined);
  }

  // Codex shell policy picks up every remote cache variable automatically.
  const codexArgs = codexShellEnvConfigArgs(toolCacheEnv("agent", { root, env, hasTool: () => true }));
  assert.ok(codexArgs.includes('shell_environment_policy.set.RUDDER_BURST_SOCKET="/run/rudder/burst.sock"'));
  assert.ok(codexArgs.includes('shell_environment_policy.set.SCCACHE_WEBDAV_ENDPOINT="http://127.0.0.1:41234/sccache"'));
  assert.ok(codexArgs.includes('shell_environment_policy.set.TURBO_API="http://127.0.0.1:41234/turbo"'));
});

test("toolCacheUsage reports per-tool bytes, counts, recent files and lastWriteAt without following symlinks", (t) => {
  const root = tempDir(t);
  const outside = path.join(root, "outside.bin");
  fs.writeFileSync(outside, Buffer.alloc(10_000));

  const npmDir = path.join(root, "npm", "nested");
  fs.mkdirSync(npmDir, { recursive: true });
  const recentFile = path.join(npmDir, "recent.tgz");
  const oldFile = path.join(npmDir, "old.tgz");
  fs.writeFileSync(recentFile, Buffer.alloc(300));
  fs.writeFileSync(oldFile, Buffer.alloc(200));
  const recentAt = new Date(Date.now() - 60_000);
  const oldAt = new Date(Date.now() - 48 * 3600_000);
  fs.utimesSync(recentFile, recentAt, recentAt);
  fs.utimesSync(oldFile, oldAt, oldAt);
  fs.symlinkSync(outside, path.join(root, "npm", "linked-outside"));

  const usage = toolCacheUsage(root, { npm: 1024, turbo: 2048 });
  assert.equal(usage.root, root);
  assert.equal(usage.totalBytes, 500);
  assert.equal(usage.entries.length, 2);

  const npm = usage.entries.find((e) => e.name === "npm");
  assert.equal(npm.bytes, 500);
  assert.equal(npm.limitBytes, 1024);
  assert.equal(npm.fileCount, 2);
  assert.equal(npm.recentFiles24h, 1);
  assert.equal(typeof npm.lastWriteAt, "number");
  assert.ok(Math.abs(npm.lastWriteAt - recentAt.getTime()) < 2000);

  const turbo = usage.entries.find((e) => e.name === "turbo");
  assert.deepEqual(turbo, {
    name: "turbo",
    bytes: 0,
    limitBytes: 2048,
    fileCount: 0,
    recentFiles24h: 0,
    lastWriteAt: null,
  });
});
