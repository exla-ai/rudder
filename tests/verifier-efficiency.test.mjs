import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { verifyRun } from "../dist/brain.js";

// Audit the actual VCS boundary, including status parsing and stale recovery.
// A diff request simulates a large patch, so the regression checks requests,
// rather than relying on timings or the patch fitting in an output buffer.
const fakeVcs = String.raw`#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
fs.appendFileSync("commands.jsonl", JSON.stringify(args) + "\n");
const fixture = JSON.parse(fs.readFileSync("status.json", "utf8"));
if (args[0] === "diff") {
  process.stdout.write("+large patch line\n".repeat(100000));
} else if (args[0] === "status") {
  if (fixture.stale && !fs.existsSync("recovered")) {
    process.stderr.write("working copy is stale");
    process.exitCode = 1;
  } else {
    process.stdout.write(fixture.stdout);
    process.exitCode = fixture.code ?? 0;
  }
} else if (args.join(" ") === "workspace update-stale") {
  fs.writeFileSync("recovered", "yes");
} else {
  process.exitCode = 2;
}
`;

test("verifier uses one status decision without requesting a patch", async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-verifier-efficiency-"));
  const bin = path.join(root, "bin");
  fs.mkdirSync(bin);
  for (const name of ["git", "jj"]) {
    fs.writeFileSync(path.join(bin, name), fakeVcs, { mode: 0o755 });
  }
  const previousPath = process.env.PATH;
  process.env.PATH = `${bin}${path.delimiter}${previousPath ?? ""}`;
  t.after(() => {
    if (previousPath === undefined) delete process.env.PATH;
    else process.env.PATH = previousPath;
    fs.rmSync(root, { recursive: true, force: true });
  });

  for (const vcs of ["git", "jj"]) {
    const statuses = vcs === "git"
      ? { changed: " M tracked.txt\n?? added.txt\n", metadata: "?? .rudder/runs/test/output.txt\n" }
      : { changed: "Working copy changes:\nM tracked.txt\nA added.txt\n", metadata: "Working copy changes:\nM .rudder/runs/test/output.txt\n" };
    const cases = [
      { name: "changed", stdout: statuses.changed, changed: true },
      { name: "clean", stdout: "", changed: false },
      { name: "metadata-only", stdout: statuses.metadata, changed: false },
      { name: "failed-status", stdout: "", code: 1, changed: false },
      { name: "non-edit", stdout: "", task: "Explain the architecture", changed: false },
      { name: "skipped-checks", stdout: statuses.changed, transcript: "I did not run any checks.", changed: true, skipped: true },
      { name: "missing-checks", stdout: statuses.changed, transcript: "Finished the task.", changed: true, missingChecks: true },
      ...(vcs === "jj" ? [{ name: "stale-status", stdout: statuses.changed, stale: true, changed: true }] : []),
    ];
    for (const fixture of cases) {
      await t.test(`${vcs}: ${fixture.name}`, async () => {
        const repo = path.join(root, `${vcs}-${fixture.name}`);
        const id = "run";
        const runDir = path.join(repo, ".rudder", "runs", id);
        fs.mkdirSync(runDir, { recursive: true });
        fs.writeFileSync(path.join(repo, "status.json"), JSON.stringify(fixture));
        fs.writeFileSync(path.join(runDir, "output.txt"), fixture.transcript ?? "npm run check\nall checks passed\n");
        fs.writeFileSync(path.join(runDir, "spec.json"), JSON.stringify({ success: "Run `npm run check`", suggestedTests: ["npm run check"] }));
        const run = {
          id, repoRoot: repo, vcs, task: fixture.task ?? "Fix the implementation",
          workspace: { enabled: vcs === "jj", path: repo },
        };

        const result = await verifyRun(run);
        const commands = fs.readFileSync(path.join(repo, "commands.jsonl"), "utf8").trim().split("\n").map(JSON.parse);
        assert.deepEqual(commands, fixture.stale
          ? [["status"], ["workspace", "update-stale"], ["status"]]
          : [vcs === "jj" ? ["status"] : ["status", "--short"]]);
        assert.equal(result.shouldContinue, !fixture.changed && !fixture.task);
        assert.deepEqual(result.missing, fixture.skipped
          ? ["The transcript says verification was skipped or could not run."]
          : fixture.missingChecks
            ? ["No evidence the expected checks ran (looked for: npm run check)."]
            : !fixture.changed && !fixture.task ? ["No file changes were detected."] : []);
        const expectedSatisfied = [fixture.changed ? "The run produced file changes." : fixture.task
          ? "No file changes were needed for this prompt." : null];
        if (!fixture.task && !fixture.skipped && !fixture.missingChecks) {
          expectedSatisfied.push("The transcript shows an expected check ran (npm run check).");
        }
        assert.deepEqual(result.satisfied, expectedSatisfied.filter(Boolean));
        assert.deepEqual(JSON.parse(fs.readFileSync(path.join(runDir, "verifier.json"), "utf8")), result);
      });
    }
  }
});
