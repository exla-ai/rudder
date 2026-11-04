// Deleting in Rudder Cloud is not undoable, so the CLI never assumes yes: with
// no terminal to ask on, it refuses unless --yes says so explicitly.
import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const cli = path.join(repoRoot, "dist", "index.js");

function runCli(args) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-delete-cli-"));
  try {
    return spawnSync(process.execPath, [cli, ...args], {
      encoding: "utf8",
      // stdin is not a TTY here, exactly like a script or the dashboard.
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        RUDDER_HOME: home,
        RUDDER_DISABLE_AUTO_UPDATE: "1",
        RUDDER_TELEMETRY: "0",
        // Unroutable: if confirmation were skipped, the request would fail
        // loudly instead of silently deleting anything real.
        RUDDER_CLOUD_URL: "http://127.0.0.1:9",
        RUDDER_CLOUD_TOKEN: "rdr_test",
      },
      timeout: 30_000,
    });
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
}

test("cloud delete without a terminal refuses unless --yes is given", () => {
  const sail = runCli(["cloud", "delete", "sail-123"]);
  assert.notEqual(sail.status, 0);
  assert.match(sail.stderr, /Refusing to delete cloud sail sail-123 without confirmation; pass --yes/);

  const usage = runCli(["cloud", "delete"]);
  assert.notEqual(usage.status, 0);
  assert.match(usage.stderr, /Usage: rudder cloud delete <sail-id> \[--yes\]/);
});

test("cloud delete --yes skips the question and goes to the control plane", () => {
  const result = runCli(["cloud", "delete", "sail-123", "--yes"]);
  assert.notEqual(result.status, 0);
  assert.doesNotMatch(result.stderr, /Refusing to delete/);
});
