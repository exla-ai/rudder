import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const compiledModule = fileURLToPath(new URL("../dist/version-check.js", import.meta.url));

function updaterFixture(t, { name = "@exla/rudder", current = "4.0.0-alpha.1", latest, failNew = false }) {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-prerelease-update-"));
  t.after(() => fs.rmSync(scratch, { recursive: true, force: true }));
  const pkg = path.join(scratch, "pkg");
  const bin = path.join(scratch, "bin");
  const log = path.join(scratch, "commands.log");
  fs.mkdirSync(path.join(pkg, "dist"), { recursive: true });
  fs.mkdirSync(bin);
  fs.copyFileSync(compiledModule, path.join(pkg, "dist", "version-check.js"));
  fs.writeFileSync(path.join(pkg, "package.json"), JSON.stringify({ name, version: current, type: "module" }));
  fs.writeFileSync(path.join(bin, "npm"), `#!/bin/sh
printf 'npm %s\\n' "$*" >> "$RUDDER_TEST_UPDATE_LOG"
case "$*" in
  "install -g @exla/rudder@"*) if [ "$RUDDER_TEST_FAIL_NEW" = "1" ]; then exit 1; fi ;;
esac
exit 0
`, { mode: 0o755 });
  fs.writeFileSync(path.join(bin, "rudder"), `#!/bin/sh
printf 'rudder %s\\n' "$*" >> "$RUDDER_TEST_UPDATE_LOG"
exit 0
`, { mode: 0o755 });
  const driver = path.join(pkg, "driver.mjs");
  fs.writeFileSync(driver, `
import { autoUpdateAndRerunIfNeeded } from './dist/version-check.js';
globalThis.fetch = async () => ({ ok: true, json: async () => ({ version: ${JSON.stringify(latest)} }) });
if (process.env.RUDDER_SKIP_AUTO_UPDATE) console.log('restarted');
else console.log(JSON.stringify({ updated: await autoUpdateAndRerunIfNeeded(['status']) }));
`);
  const env = { ...process.env, PATH: `${bin}:${process.env.PATH || ""}`, XDG_CACHE_HOME: path.join(scratch, "cache"),
    RUDDER_TEST_UPDATE_LOG: log, RUDDER_TEST_FAIL_NEW: failNew ? "1" : "0" };
  for (const key of ["CI", "RUDDER_DISABLE_UPDATE_CHECK", "RUDDER_DISABLE_AUTO_UPDATE", "RUDDER_SKIP_AUTO_UPDATE"]) delete env[key];
  const result = spawnSync(process.execPath, [driver], { env, encoding: "utf8", timeout: 10_000 });
  assert.equal(result.status, 0, result.stderr || result.error?.message);
  const commands = fs.existsSync(log) ? fs.readFileSync(log, "utf8").trim().split("\n") : [];
  return { commands, output: result.stdout, stderr: result.stderr };
}

const options = { skip: process.platform === "win32" ? "global package command fixture uses POSIX shell" : false };

test("an opted-in v4 alpha global install does not downgrade to the v3 latest tag", options, (t) => {
  const result = updaterFixture(t, { latest: "3.9.99" });
  assert.deepEqual(result.commands, []);
  assert.match(result.output, /"updated":false/);
});

test("a global alpha installs stable v4 and restarts with the update recursion guard", options, (t) => {
  const result = updaterFixture(t, { latest: "4.0.0" });
  assert.deepEqual(result.commands, ["npm install -g @exla/rudder@4.0.0"]);
  assert.match(result.output, /restarted/);
  assert.match(result.output, /"updated":true/);
});

test("legacy-name migration preserves an opted-in alpha ahead of the latest tag", options, (t) => {
  const result = updaterFixture(t, { name: "@viraatdas/rudder", latest: "3.9.99" });
  assert.deepEqual(result.commands, ["npm uninstall -g @viraatdas/rudder", "npm install -g @exla/rudder@4.0.0-alpha.1", "rudder status"]);
  assert.match(result.output, /"updated":true/);
});

test("a missing renamed prerelease restores the original exact package and version", options, (t) => {
  const result = updaterFixture(t, { name: "@viraatdas/rudder", latest: "3.9.99", failNew: true });
  assert.deepEqual(result.commands, ["npm uninstall -g @viraatdas/rudder", "npm install -g @exla/rudder@4.0.0-alpha.1", "npm install -g @viraatdas/rudder@4.0.0-alpha.1"]);
  assert.match(result.output, /"updated":false/);
});
