// Moving live agents from a laptop to the home machine (src/move-home.ts),
// against real git + jj: a laptop checkout whose agent works on top of a
// local-only change, and a home clone of the same GitHub (bare) origin.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  adoptMovedAgents,
  adoptedWorkspacePath,
  buildMoveBundle,
  findMoveCandidates,
  markRunsMovedHome,
} from "../dist/move-home.js";
import { encodeClaudeProjectsCwd } from "../dist/migration.js";

function haveJj() {
  try {
    execFileSync("jj", ["--version"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

function sh(cwd, command, args) {
  return execFileSync(command, args, {
    cwd,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      GIT_AUTHOR_NAME: "t",
      GIT_AUTHOR_EMAIL: "t@example.com",
      GIT_COMMITTER_NAME: "t",
      GIT_COMMITTER_EMAIL: "t@example.com",
      JJ_USER: "t",
      JJ_EMAIL: "t@example.com",
    },
  });
}

async function writeRun(repo, runId, fields) {
  const dir = path.join(repo, ".rudder", "runs", runId);
  await fsp.mkdir(dir, { recursive: true });
  await fsp.writeFile(path.join(dir, "run.json"), JSON.stringify({ id: runId, repoRoot: repo, vcs: "jj", turns: [], ...fields }, null, 2));
}

async function readRun(repo, runId) {
  return JSON.parse(await fsp.readFile(path.join(repo, ".rudder", "runs", runId, "run.json"), "utf8"));
}

test("an agent's jj change and conversation move to the home clone without pushing anything", { skip: !haveJj() && "jj not installed" }, async () => {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-move-home-"));
  const previousHome = process.env.HOME;
  try {
    const origin = path.join(root, "origin.git");
    const laptop = path.join(root, "laptop", "repo");
    const laptopHome = path.join(root, "laptop-home");
    const homeMachineHome = path.join(root, "home-home");
    const homeClone = path.join(root, "home", "code", "repo");
    await fsp.mkdir(path.dirname(laptop), { recursive: true });
    await fsp.mkdir(laptopHome, { recursive: true });
    await fsp.mkdir(homeMachineHome, { recursive: true });

    sh(root, "git", ["init", "-q", "--bare", "-b", "main", origin]);
    sh(root, "git", ["clone", "-q", origin, laptop]);
    await fsp.writeFile(path.join(laptop, "README.md"), "hello\n");
    sh(laptop, "git", ["add", "README.md"]);
    sh(laptop, "git", ["commit", "-qm", "init"]);
    sh(laptop, "git", ["push", "-q", "origin", "main"]);
    sh(laptop, "git", ["remote", "set-head", "origin", "main"]);
    sh(laptop, "jj", ["git", "init", "--colocate"]);

    // A local-only change in the main checkout (never pushed): the agent's
    // workspace is based on it, so the move has to carry it too.
    await fsp.writeFile(path.join(laptop, "local-only.txt"), "not on github\n");
    sh(laptop, "jj", ["describe", "-m", "local integration"]);
    sh(laptop, "jj", ["new"]);

    const runId = "1792000000000000001-fix-thing-1234";
    const workspace = path.join(laptop, ".rudder-workspaces", "repo-abc123", "fix-thing-deadbeef");
    await fsp.mkdir(path.dirname(workspace), { recursive: true });
    sh(laptop, "jj", ["workspace", "add", workspace, "--name", "rudder-fixthing", "-r", "@-"]);
    sh(workspace, "jj", ["describe", "-m", "rudder: fix thing [running]"]);
    await fsp.writeFile(path.join(workspace, "agent.txt"), "agent work in progress\n");

    const sessionId = "11111111-2222-3333-4444-555555555555";
    await writeRun(laptop, runId, {
      status: "running",
      backend: "claude",
      task: "fix the thing",
      taskSummary: "fix thing",
      workspace: { enabled: true, path: workspace, workspaceName: "rudder-fixthing" },
      session: { nativeSessionId: sessionId },
      process: { pid: 4242 },
    });
    // Rows that must NOT move: a finished one and a main-checkout one.
    await writeRun(laptop, "done-run", { status: "completed", backend: "claude", task: "x", workspace: { enabled: true, path: workspace } });
    await writeRun(laptop, "main-run", { status: "running", backend: "claude", task: "x", workspace: { enabled: false, path: laptop } });

    process.env.HOME = laptopHome;
    const transcriptDir = path.join(laptopHome, ".claude", "projects", encodeClaudeProjectsCwd(workspace));
    await fsp.mkdir(transcriptDir, { recursive: true });
    await fsp.writeFile(path.join(transcriptDir, `${sessionId}.jsonl`), '{"type":"user","message":"fix the thing"}\n');

    const candidates = await findMoveCandidates(laptop);
    assert.deepEqual(candidates.map((c) => c.runId), [runId]);

    const stage = path.join(root, "stage");
    await fsp.mkdir(stage);
    const manifest = await buildMoveBundle(laptop, candidates, stage);
    assert.equal(manifest.agents.length, 1);
    assert.equal(manifest.agents[0].sessionFile, `sessions/${runId}.jsonl`);
    assert.equal(manifest.agents[0].relativeWorkspacePath, ".rudder-workspaces/repo-abc123/fix-thing-deadbeef");
    assert.equal(
      sh(laptop, "git", ["for-each-ref", "refs/rudder-move/"]).trim(),
      "",
      "the temporary bundle refs are removed from the laptop",
    );
    assert.equal(
      sh(root, "git", ["--git-dir", origin, "for-each-ref", "refs/heads/"]).trim().split("\n").length,
      1,
      "nothing was pushed to the origin",
    );

    // The home machine: a fresh clone of the origin, where the dashboard
    // has not run yet (no jj).
    await fsp.mkdir(path.dirname(homeClone), { recursive: true });
    sh(root, "git", ["clone", "-q", origin, homeClone]);
    const result = await adoptMovedAgents(homeClone, stage, homeMachineHome);
    assert.deepEqual(result.failed, []);
    assert.equal(result.adopted.length, 1);
    const adopted = result.adopted[0];
    assert.equal(adopted.workspacePath, path.join(homeClone, ".rudder-workspaces", "repo-abc123", "fix-thing-deadbeef"));
    assert.equal(adopted.workspacePath, adoptedWorkspacePath(homeClone, manifest.agents[0]));
    assert.equal(adopted.resumable, true);

    // The workspace's @ IS the moved change: its diff is the agent's work,
    // and the local-only base came along underneath it.
    const diff = sh(adopted.workspacePath, "jj", ["diff", "--git"]);
    assert.match(diff, /agent work in progress/);
    assert.doesNotMatch(diff, /not on github/, "the base change is an ancestor, not part of the agent's diff");
    assert.equal(fs.readFileSync(path.join(adopted.workspacePath, "local-only.txt"), "utf8"), "not on github\n");
    assert.match(sh(adopted.workspacePath, "jj", ["log", "--no-graph", "-r", "@", "-T", "description"]), /fix thing/);
    assert.equal(
      sh(homeClone, "jj", ["bookmark", "list", "--all-remotes"]).includes("rudder-move"),
      false,
      "the import bookmark is gone once the workspace holds the commit",
    );

    // The conversation is where `claude --resume` looks for it at the new cwd.
    const placed = path.join(homeMachineHome, ".claude", "projects", encodeClaudeProjectsCwd(adopted.workspacePath), `${sessionId}.jsonl`);
    assert.ok(fs.existsSync(placed), `transcript placed at ${placed}`);

    const record = await readRun(homeClone, runId);
    assert.equal(record.status, "paused");
    assert.equal(record.repoRoot, homeClone);
    assert.equal(record.workspace.path, adopted.workspacePath);
    assert.equal(record.workspace.jjChangeId, adopted.jjChangeId);
    assert.equal(record.process, undefined);
    assert.equal(record.session.nativeSessionId, sessionId);
    assert.equal(record.migration.origin, "laptop");
    assert.equal(record.migration.pendingResume, true);

    // A retried move (the laptop never heard back) must not stage it twice.
    const again = await adoptMovedAgents(homeClone, stage, homeMachineHome);
    assert.deepEqual(again.failed, []);
    assert.equal(again.adopted[0].alreadyAdopted, true);

    await markRunsMovedHome(laptop, [runId], "owner/repo");
    const local = await readRun(laptop, runId);
    assert.equal(local.status, "migrated");
    assert.equal(local.movedTo.kind, "home");
    assert.equal(local.process, undefined);
  } finally {
    if (previousHome === undefined) {
      delete process.env.HOME;
    } else {
      process.env.HOME = previousHome;
    }
    await fsp.rm(root, { recursive: true, force: true });
  }
});

test("a run without a resumable conversation moves with a restart prompt that points at its diff", { skip: !haveJj() && "jj not installed" }, async () => {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-move-fresh-"));
  const previousHome = process.env.HOME;
  try {
    const origin = path.join(root, "origin.git");
    const laptop = path.join(root, "repo");
    sh(root, "git", ["init", "-q", "--bare", "-b", "main", origin]);
    sh(root, "git", ["clone", "-q", origin, laptop]);
    await fsp.writeFile(path.join(laptop, "a.txt"), "a\n");
    sh(laptop, "git", ["add", "a.txt"]);
    sh(laptop, "git", ["commit", "-qm", "init"]);
    sh(laptop, "git", ["push", "-q", "origin", "main"]);
    sh(laptop, "jj", ["git", "init", "--colocate"]);
    const workspace = path.join(laptop, ".rudder-workspaces", "repo-1", "codex-task-1");
    await fsp.mkdir(path.dirname(workspace), { recursive: true });
    sh(laptop, "jj", ["workspace", "add", workspace, "--name", "rudder-codex", "-r", "@-"]);
    await fsp.writeFile(path.join(workspace, "b.txt"), "b\n");
    await writeRun(laptop, "codex-run", {
      status: "paused",
      backend: "codex",
      task: "add b",
      workspace: { enabled: true, path: workspace, workspaceName: "rudder-codex" },
      session: { nativeSessionId: "019a0000-0000-7000-8000-000000000000" },
      turns: [{ prompt: "add b", source: "user" }, { prompt: "also test it", source: "user" }],
    });
    process.env.HOME = path.join(root, "no-home");
    process.env.CODEX_HOME = path.join(root, "no-codex");
    const stage = path.join(root, "stage");
    await fsp.mkdir(stage);
    const manifest = await buildMoveBundle(laptop, await findMoveCandidates(laptop, ["codex-run"]), stage);
    const [agent] = manifest.agents;
    assert.equal(agent.sessionId, "", "no rollout on disk, so nothing to resume");
    assert.match(agent.freshPrompt, /add b/);
    assert.match(agent.freshPrompt, /jj diff/);

    const homeClone = path.join(root, "home-clone");
    sh(root, "git", ["clone", "-q", origin, homeClone]);
    const result = await adoptMovedAgents(homeClone, stage, path.join(root, "home-home"));
    assert.deepEqual(result.failed, []);
    const record = await readRun(homeClone, "codex-run");
    assert.equal(record.migration.pendingFresh, true);
    assert.match(record.migration.freshPrompt, /jj diff/);
    assert.match(sh(result.adopted[0].workspacePath, "jj", ["diff", "--git"]), /\+b/);
  } finally {
    delete process.env.CODEX_HOME;
    if (previousHome === undefined) {
      delete process.env.HOME;
    } else {
      process.env.HOME = previousHome;
    }
    await fsp.rm(root, { recursive: true, force: true });
  }
});
