import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import test from "node:test";

const execute = promisify(execFile);
const cli = fileURLToPath(new URL("../dist/index.js", import.meta.url));
const pkg = JSON.parse(await fs.readFile(new URL("../package.json", import.meta.url), "utf8"));

async function fixture(t) {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), "rudder-cli-startup-"));
  t.after(() => fs.rm(directory, { recursive: true, force: true }));
  const home = path.join(directory, "home");
  const env = {
    ...process.env,
    HOME: home,
    RUDDER_HOME: path.join(home, ".rudder"),
    CODEX_HOME: path.join(home, ".codex"),
    CLAUDE_CONFIG_DIR: path.join(home, ".claude"),
    XDG_CACHE_HOME: path.join(home, ".cache"),
    RUDDER_DISABLE_UPDATE_CHECK: "1",
    RUDDER_DISABLE_AUTO_UPDATE: "1",
    RUDDER_POSTHOG_HOST: "http://127.0.0.1:1",
    RUDDER_POSTHOG_KEY: "local-startup-fixture",
  };
  // Exercise the real first-use disclosure without sending product events.
  for (const key of ["CI", "RUDDER_TELEMETRY", "RUDDER_FAKE_BACKEND", "RUDDER_DISABLE_AGENT_SKILLS"]) delete env[key];
  const run = (...args) => execute(process.execPath, [cli, ...args], {
    cwd: directory, env, timeout: 15_000,
  });
  return { directory, home, env, run };
}

test("fresh version inspection has one stdout line and leaves first-use setup pending", async (t) => {
  const { home, run } = await fixture(t);
  for (const args of [["--version"], ["version"]]) {
    const result = await run(...args);
    assert.equal(result.stdout, `${pkg.version}\n`);
    assert.equal(result.stderr, "");
    await assert.rejects(fs.stat(home), { code: "ENOENT" });
  }
});

test("help and unknown private callbacks leave agent setup pending", async (t) => {
  const { home, run } = await fixture(t);
  const help = await run("--help");
  assert.match(help.stdout, /Usage:/);
  assert.equal(help.stderr, "");
  const callback = await run("__future_callback");
  assert.equal(callback.stdout, "");
  assert.equal(callback.stderr, "");
  await assert.rejects(fs.stat(home), { code: "ENOENT" });
});

test("first-use disclosure preserves JSON stdout and is shown once on stderr", {
  skip: process.platform === "win32" ? "jj discovery fixture uses a POSIX executable" : false,
}, async (t) => {
  const { directory, home, env, run } = await fixture(t);
  const bin = path.join(directory, "bin");
  await fs.mkdir(bin);
  await fs.writeFile(path.join(bin, "jj"), "#!/bin/sh\nprintf 'jj 0.40.0\\n'\n", { mode: 0o755 });
  env.PATH = `${bin}${path.delimiter}${env.PATH ?? ""}`;
  await fs.writeFile(path.join(directory, "AGENTS.md"), "Run the focused checks.\n");
  const first = await run("context", "audit", "--json");
  assert.equal(JSON.parse(first.stdout).files.length, 1);
  assert.match(first.stderr, /rudder sends anonymous usage events/);
  const config = JSON.parse(await fs.readFile(path.join(home, ".rudder/config.json"), "utf8"));
  assert.ok(config.telemetryNoticeShownAt);
  const next = await run("context", "audit", "--json");
  assert.equal(JSON.parse(next.stdout).files.length, 1);
  assert.equal(next.stderr, "");
});
