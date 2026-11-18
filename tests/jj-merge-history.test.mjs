import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { createRunJjWorkspace, ensureColocated, mergeJjRunIntoCurrentWorkspace } from "../dist/jj.js";
import { createRunRecord } from "../dist/state.js";

// Real jj and git: the shapes these pin are jj's colocation behavior (where
// Git's HEAD and index end up), which no fake can stand in for.

function run(cwd, program, args) {
  return execFileSync(program, args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trimEnd();
}
const git = (cwd, args) => run(cwd, "git", args);
const jj = (cwd, args) => run(cwd, "jj", args);

function haveJj() {
  try {
    execFileSync("jj", ["--version"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

/** A user's ordinary Git repo on main, colocated the way Rudder does it. */
async function userRepo(t) {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-merge-history-"));
  const saved = { RUDDER_HOME: process.env.RUDDER_HOME, RUDDER_JJ_BIN: process.env.RUDDER_JJ_BIN };
  process.env.RUDDER_HOME = path.join(root, "home");
  // Stock jj for Rudder and the assertions alike.
  process.env.RUDDER_JJ_BIN = "";
  t.after(async () => {
    for (const [key, value] of Object.entries(saved)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
    await fsp.rm(root, { recursive: true, force: true });
  });
  const repo = path.join(root, "repo");
  git(root, ["init", "-q", "-b", "main", repo]);
  await fsp.writeFile(path.join(repo, "a.txt"), "a\n");
  git(repo, ["add", "a.txt"]);
  git(repo, ["-c", "user.name=Rudder Test", "-c", "user.email=test@example.com", "commit", "-q", "-m", "add a"]);
  await ensureColocated(repo);
  jj(repo, ["config", "set", "--repo", "user.name", "Rudder Test"]);
  jj(repo, ["config", "set", "--repo", "user.email", "test@example.com"]);
  return repo;
}

/** An agent in its own jj workspace that wrote `files` and finished. */
async function finishedAgent(repo, id, task, files) {
  const workspace = await createRunJjWorkspace({ repoRoot: repo, runId: id, task });
  for (const [name, content] of Object.entries(files)) {
    await fsp.writeFile(path.join(workspace.path, name), content);
  }
  jj(workspace.path, ["describe", "-m", `rudder: ${task} [done]`]);
  const record = await createRunRecord({
    id,
    repoRoot: repo,
    task,
    backend: "codex",
    targetBranch: "main",
    baseCommit: git(repo, ["rev-parse", "main"]),
    useWorkspace: true,
    workspacePath: workspace.path,
    workspaceName: workspace.workspaceName,
  });
  const change = jj(workspace.path, ["log", "--no-graph", "-r", "@", "-T", "change_id"]);
  return { record, path: workspace.path, change };
}

/** The agent's commit as it stands now (landing re-describes it, and moves the
 * workspace's `@` to a new change on top of main). */
const commitOf = (agent) =>
  jj(agent.path, ["--ignore-working-copy", "log", "--no-graph", "-r", agent.change, "-T", "commit_id"]);

const parents = (repo, rev) => git(repo, ["log", "-1", "--format=%P", rev]).split(" ");
const subjects = (repo) => git(repo, ["log", "--format=%s", "main"]).split("\n");
// `## main` alone: on branch main, nothing staged, nothing modified.
const gitStatus = (repo) => git(repo, ["status", "--porcelain=v1", "--branch"]);

test("a merged agent lands on main alone and git is left on main with a clean tree", { skip: !haveJj() }, async (t) => {
  const repo = await userRepo(t);
  const before = git(repo, ["rev-parse", "main"]);
  const agent = await finishedAgent(repo, "add-b-k1a2b", "add b", { "b.txt": "b\n" });

  const merged = await mergeJjRunIntoCurrentWorkspace(agent.record, true);

  assert.equal(merged.status, "merged");
  assert.ok(merged.merge?.operationId, "the pre-merge operation is the undo waypoint");
  const main = git(repo, ["rev-parse", "main"]);
  assert.equal(main, commitOf(agent), "main fast-forwards to the agent's change");
  assert.deepEqual(parents(repo, main), [before]);
  assert.deepEqual(subjects(repo), ["rudder: add b", "add a"]);
  assert.ok(!subjects(repo).includes(""), "main never gains a commit with no message");
  assert.equal(git(repo, ["show", "main:b.txt"]), "b");
  assert.equal(merged.merge?.localCommit, main);
  assert.equal(gitStatus(repo), "## main");
  assert.equal(jj(repo, ["log", "--no-graph", "-r", "@", "-T", "empty"]), "true");
});

test("an agent started before main moved lands as a merge whose first parent is main", { skip: !haveJj() }, async (t) => {
  const repo = await userRepo(t);
  const first = await finishedAgent(repo, "add-b-k3c4d", "add b", { "b.txt": "b\n" });
  const second = await finishedAgent(repo, "add-c-k5e6f", "add c", { "c.txt": "c\n" });
  // The user's own uncommitted edit in the checkout rides along, uncommitted.
  await fsp.writeFile(path.join(repo, "a.txt"), "a, edited by the user\n");

  await mergeJjRunIntoCurrentWorkspace(first.record, true);
  const merged = await mergeJjRunIntoCurrentWorkspace(second.record, true);

  assert.equal(merged.status, "merged");
  assert.deepEqual(parents(repo, "main"), [commitOf(first), commitOf(second)]);
  assert.deepEqual(git(repo, ["log", "--first-parent", "--format=%s", "main"]).split("\n"), [
    "rudder: add c",
    "rudder: add b",
    "add a",
  ]);
  assert.equal(git(repo, ["log", "-1", "--format=%s", "main^2"]), "rudder: add c");
  assert.ok(!subjects(repo).includes(""), "main never gains a commit with no message");
  assert.equal(git(repo, ["show", "main:a.txt"]), "a", "the user's edit is not merged into main");
  assert.equal(gitStatus(repo), "## main\n M a.txt");
});

test("a conflicted merge is resolved in the checkout, then lands with main as first parent", { skip: !haveJj() }, async (t) => {
  const repo = await userRepo(t);
  const first = await finishedAgent(repo, "edit-a-k7g8h", "edit a", { "a.txt": "first\n" });
  const second = await finishedAgent(repo, "edit-a-again-k9i0j", "edit a again", { "a.txt": "second\n" });
  await mergeJjRunIntoCurrentWorkspace(first.record, true);

  const conflicted = await mergeJjRunIntoCurrentWorkspace(second.record, true);

  assert.equal(conflicted.status, "merge-conflict");
  assert.deepEqual(conflicted.merge?.conflictedFiles, ["a.txt"]);
  assert.equal(git(repo, ["rev-parse", "main"]), commitOf(first), "main waits for the resolution");
  assert.equal(
    jj(repo, ["log", "--no-graph", "-r", "@", "-T", "change_id"]),
    conflicted.merge?.mergeChangeId,
    "the resolver works on the merge in the checkout",
  );

  await fsp.writeFile(path.join(repo, "a.txt"), "resolved\n");
  const merged = await mergeJjRunIntoCurrentWorkspace(conflicted, true);

  assert.equal(merged.status, "merged");
  assert.deepEqual(parents(repo, "main"), [commitOf(first), commitOf(second)]);
  assert.equal(git(repo, ["show", "main:a.txt"]), "resolved");
  assert.ok(!subjects(repo).includes(""), "main never gains a commit with no message");
  assert.equal(gitStatus(repo), "## main");
});

test("automatic integration restores the pre-merge state on conflict", { skip: !haveJj() }, async (t) => {
  const repo = await userRepo(t);
  const first = await finishedAgent(repo, "edit-a-k1l2m", "edit a", { "a.txt": "first\n" });
  const second = await finishedAgent(repo, "edit-a-again-k3n4o", "edit a again", { "a.txt": "second\n" });
  await mergeJjRunIntoCurrentWorkspace(first.record, true);
  const checkout = jj(repo, ["log", "--no-graph", "-r", "@", "-T", "commit_id"]);

  const merged = await mergeJjRunIntoCurrentWorkspace(second.record, true, { abortOnConflict: true });

  assert.equal(merged.merge?.status, "conflict");
  assert.deepEqual(merged.merge?.conflictedFiles, ["a.txt"]);
  assert.equal(merged.merge?.mergeChangeId, undefined);
  assert.match(merged.merge?.error ?? "", /nothing was merged/);
  assert.equal(git(repo, ["rev-parse", "main"]), commitOf(first));
  assert.equal(jj(repo, ["log", "--no-graph", "-r", "@", "-T", "commit_id"]), checkout);
  assert.equal(jj(repo, ["log", "--no-graph", "-r", "conflicts()", "-T", "commit_id"]), "");
});

test("a merged agent's workspace moves off main, so later edits there never rewrite it", { skip: !haveJj() }, async (t) => {
  const repo = await userRepo(t);
  const agent = await finishedAgent(repo, "add-b-k5p6q", "add b", { "b.txt": "b\n" });
  await mergeJjRunIntoCurrentWorkspace(agent.record, true);
  const main = git(repo, ["rev-parse", "main"]);

  // A resumed agent edits; any jj command run there snapshots the edit.
  await fsp.writeFile(path.join(agent.path, "late.txt"), "late\n");
  jj(agent.path, ["status"]);

  assert.equal(
    jj(repo, ["--ignore-working-copy", "log", "--no-graph", "-r", "main", "-T", "commit_id"]),
    main,
    "main's commit is not rewritten",
  );
  assert.equal(commitOf(agent), main);
  assert.deepEqual(jj(repo, ["--ignore-working-copy", "file", "list", "-r", "main"]).split("\n"), ["a.txt", "b.txt"]);
  assert.equal(jj(agent.path, ["log", "--no-graph", "-r", "@-", "-T", "commit_id"]), main);
  assert.equal(jj(agent.path, ["diff", "--name-only"]), "late.txt", "the edit is a new change on top of main");
  assert.equal(gitStatus(repo), "## main");
});
