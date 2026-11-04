import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { pruneToolCache, toolCacheEnv } from "../dist/tool-cache.js";

function tempDir(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-tool-cache-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

test("agents get read-only sccache and the builder gets read-write", (t) => {
  const root = tempDir(t);
  const env = { HOME: path.join(root, "home") };
  const agent = toolCacheEnv("agent", { root, env, hasTool: () => true });
  const builder = toolCacheEnv("builder", { root, env, hasTool: () => true });
  assert.equal(agent.SCCACHE_LOCAL_RW_MODE, "READ_ONLY");
  assert.equal(builder.SCCACHE_LOCAL_RW_MODE, "READ_WRITE");
  assert.equal(agent.RUSTC_WRAPPER, "sccache");
  assert.equal(agent.SCCACHE_DIR, path.join(root, "sccache"));
  for (const key of ["npm_config_store_dir", "npm_config_cache", "UV_CACHE_DIR", "PIP_CACHE_DIR", "CARGO_HOME", "TURBO_CACHE_DIR", "NX_CACHE_DIRECTORY"]) {
    assert.equal(agent[key], builder[key]);
    assert.ok(fs.statSync(agent[key]).isDirectory(), key);
  }
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
