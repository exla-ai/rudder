import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { fileURLToPath } from "node:url";
import test from "node:test";

const cacheScript = fileURLToPath(new URL("../cloud/worker/cache-env.sh", import.meta.url));

async function fixture(t) {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), "rudder-cache-"));
  t.after(() => fs.rm(dir, { recursive: true, force: true }));
  return dir;
}

function cacheEnv(dir, overrides = {}) {
  const env = { ...process.env, HOME: dir, TEST_CACHE_SCRIPT: cacheScript };
  for (const key of ["RUDDER_CACHE_DIR", "RUDDER_HOME", "RUDDER_HOME_VOLUME", "RUDDER_BUILD_CACHE", "npm_config_cache", "NPM_CONFIG_CACHE", "npm_config_store_dir", "NPM_CONFIG_STORE_DIR", "UV_CACHE_DIR"]) delete env[key];
  Object.assign(env, overrides);
  const result = spawnSync("sh", ["-eu", "-c", '. "$TEST_CACHE_SCRIPT"; env'], { env, encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
  return Object.fromEntries(result.stdout.trim().split("\n").map((line) => {
    const at = line.indexOf("=");
    return [line.slice(0, at), line.slice(at + 1)];
  }));
}

test("home package caches survive reboot on the account volume and respect explicit tool paths", async (t) => {
  const dir = await fixture(t);
  const volume = path.join(dir, "volume");
  const env = cacheEnv(dir, { RUDDER_HOME: "1", RUDDER_HOME_VOLUME: volume });
  const root = path.join(volume, "cache", "build");
  assert.equal(env.npm_config_cache, path.join(root, "npm"));
  assert.equal(env.npm_config_store_dir, path.join(root, "pnpm"));
  assert.equal(env.UV_CACHE_DIR, path.join(root, "uv"));
  assert.equal(env.CARGO_TARGET_DIR, process.env.CARGO_TARGET_DIR, "do not share mutable compiler outputs");
  await fs.writeFile(path.join(root, "retained"), "package blob");
  const next = cacheEnv(dir, { RUDDER_HOME: "1", RUDDER_HOME_VOLUME: volume, NPM_CONFIG_CACHE: "/custom/npm", NPM_CONFIG_STORE_DIR: "/custom/pnpm", UV_CACHE_DIR: "/custom/uv" });
  assert.equal(await fs.readFile(path.join(root, "retained"), "utf8"), "package blob");
  assert.deepEqual([next.npm_config_cache, next.npm_config_store_dir, next.UV_CACHE_DIR], ["/custom/npm", "/custom/pnpm", "/custom/uv"]);
});

test("disabled or unavailable cache setup preserves normal startup", async (t) => {
  const dir = await fixture(t);
  assert.equal(cacheEnv(dir, { RUDDER_BUILD_CACHE: "0" }).npm_config_cache, undefined);
  const occupied = path.join(dir, "occupied");
  await fs.writeFile(occupied, "a regular file");
  const env = cacheEnv(dir, { RUDDER_CACHE_DIR: path.join(occupied, "cache") });
  assert.equal(env.npm_config_cache, undefined);
  assert.equal(env.UV_CACHE_DIR, undefined);
  assert.equal(await fs.readFile(occupied, "utf8"), "a regular file");
});

test("a blocked agent update does not delay the supervisor", async (t) => {
  const dir = await fixture(t);
  const bin = path.join(dir, "bin");
  await fs.mkdir(bin);
  const release = path.join(dir, "release");
  const completed = path.join(dir, "updated");
  const started = path.join(dir, "started");
  const writeBin = (name, contents) => fs.writeFile(path.join(bin, name), `#!/bin/sh\n${contents}\n`, { mode: 0o755 });
  await writeBin("claude", 'if [ "$1" = update ]; then while [ ! -f "$TEST_UPDATE_RELEASE" ]; do sleep 0.02; done; : > "$TEST_UPDATE_DONE"; fi');
  await writeBin("node", ': > "$TEST_SUPERVISOR_STARTED"');
  // The production image supplies GNU timeout. This fixture exercises startup
  // ordering, so provide its command wrapper on macOS as well as Linux; the
  // test's own deadline/release keeps the blocked updater bounded.
  await writeBin("timeout", 'shift\nexec "$@"');
  await writeBin("npm", "exit 0");
  await writeBin("codex", "exit 0");
  const source = await fs.readFile(new URL("../cloud/worker/entrypoint.sh", import.meta.url), "utf8");
  const entry = path.join(dir, "entrypoint.sh");
  await fs.writeFile(entry, source.replace(". /opt/rudder-worker/cache-env.sh", '. "$TEST_CACHE_SCRIPT"'));
  const child = spawn("sh", [entry], {
    env: { ...process.env, HOME: dir, PATH: `${bin}:/usr/bin:/bin`, TEST_CACHE_SCRIPT: cacheScript,
      RUDDER_HOME: "1", RUDDER_HOME_VOLUME: dir, RUDDER_WORKER_SKIP_AGENT_UPDATE: "0",
      TEST_UPDATE_RELEASE: release, TEST_UPDATE_DONE: completed, TEST_SUPERVISOR_STARTED: started },
    stdio: "ignore",
  });
  t.after(() => child.kill());
  try {
    const [code] = await once(child, "exit", { signal: AbortSignal.timeout(3000) });
    assert.equal(code, 0);
    assert.equal((await fs.stat(started)).isFile(), true);
    await assert.rejects(fs.stat(completed), { code: "ENOENT" });
  } finally {
    await fs.writeFile(release, "");
  }
  for (let attempt = 0; attempt < 100; attempt++) {
    if (await fs.stat(completed).catch(() => null)) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.fail("the background updater should still run after the supervisor starts");
});
