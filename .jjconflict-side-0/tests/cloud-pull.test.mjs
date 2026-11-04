// Pulling one agent from the home machine to a laptop (src/cloud-pull.ts),
// against real git + jj: the home clone exports a run (uncommitted edits and
// conversation included), and a laptop clone of the same origin adopts it.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { exportRunForPull, importPulledRun, pullFromHome } from "../dist/cloud-pull.js";
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

const RUN_ID = "1792000000000000002-add-pull-5678";
const SESSION = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

// origin + a home clone whose agent has an uncommitted edit, and a laptop clone.
async function fixture() {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-cloud-pull-"));
  const origin = path.join(root, "origin.git");
  const home = path.join(root, "home", "code", "repo");
  const homeHome = path.join(root, "home-home");
  const laptop = path.join(root, "laptop", "repo");
  const laptopHome = path.join(root, "laptop-home");
  await fsp.mkdir(path.dirname(home), { recursive: true });
  await fsp.mkdir(path.dirname(laptop), { recursive: true });
  await fsp.mkdir(homeHome, { recursive: true });
  await fsp.mkdir(laptopHome, { recursive: true });

  sh(root, "git", ["init", "-q", "--bare", "-b", "main", origin]);
  sh(root, "git", ["clone", "-q", origin, home]);
  await fsp.writeFile(path.join(home, "README.md"), "hello\n");
  sh(home, "git", ["add", "README.md"]);
  sh(home, "git", ["commit", "-qm", "init"]);
  sh(home, "git", ["push", "-q", "origin", "main"]);
  sh(home, "git", ["remote", "set-head", "origin", "main"]);
  sh(home, "jj", ["git", "init", "--colocate"]);
  sh(root, "git", ["clone", "-q", origin, laptop]);

  const workspace = path.join(home, ".rudder-workspaces", "repo-abc123", "add-pull-cafebabe");
  await fsp.mkdir(path.dirname(workspace), { recursive: true });
  sh(home, "jj", ["workspace", "add", workspace, "--name", "rudder-addpull", "-r", "main@origin"]);
  // Never committed or described: the pull snapshots it.
  await fsp.writeFile(path.join(workspace, "pulled.txt"), "work in progress on the cloud\n");
  await writeRun(home, RUN_ID, {
    status: "running",
    mode: "execute",
    backend: "claude",
    model: "opus",
    task: "add the pull command",
    taskSummary: "add pull",
    nodeId: "n3",
    planId: "plan-1-0",
    workspace: { enabled: true, path: workspace, workspaceName: "rudder-addpull" },
    session: { nativeSessionId: SESSION },
    process: { pid: 77 },
  });
  const transcriptDir = path.join(homeHome, ".claude", "projects", encodeClaudeProjectsCwd(workspace));
  await fsp.mkdir(transcriptDir, { recursive: true });
  await fsp.writeFile(path.join(transcriptDir, `${SESSION}.jsonl`), '{"type":"user","message":"add the pull command"}\n');
  return { root, origin, home, homeHome, laptop, laptopHome, workspace };
}

async function exportAsHome(fx, out) {
  const previous = process.env.HOME;
  process.env.HOME = fx.homeHome;
  try {
    return await exportRunForPull(fx.home, RUN_ID, out);
  } finally {
    process.env.HOME = previous;
  }
}

test("a home agent's work and conversation come to the laptop as a paused jj workspace", { skip: !haveJj() && "jj not installed" }, async () => {
  const fx = await fixture();
  try {
    const archive = path.join(fx.root, "pull.tgz");
    const exported = await exportAsHome(fx, archive);
    assert.match(exported.head, /^[0-9a-f]{40}$/);
    assert.equal(exported.base, sh(fx.home, "git", ["rev-parse", "refs/remotes/origin/main"]).trim());
    assert.equal(exported.title, "add pull");
    assert.equal(exported.task, "add the pull command");
    assert.equal(exported.backend, "claude");
    assert.equal(exported.model, "opus");
    assert.equal(exported.sessionId, SESSION);
    assert.equal(sh(fx.home, "git", ["for-each-ref", "refs/rudder-move/"]).trim(), "", "no temporary refs left on the home clone");
    assert.equal(sh(fx.root, "git", ["--git-dir", fx.origin, "for-each-ref", "refs/heads/"]).trim().split("\n").length, 1, "nothing pushed");

    // The laptop still has the workspace this agent left behind when it moved
    // to the cloud, under the same path: the pull must not overwrite it.
    const leftBehind = path.join(fx.laptop, ".rudder-workspaces", "repo-abc123", "add-pull-cafebabe");
    await fsp.mkdir(leftBehind, { recursive: true });
    await fsp.writeFile(path.join(leftBehind, "old.txt"), "keep me\n");
    await writeRun(fx.laptop, RUN_ID, { status: "migrated", movedTo: { kind: "home" }, backend: "claude", task: "add the pull command", workspace: { enabled: true, path: leftBehind } });

    const pulled = await importPulledRun(fx.laptop, archive, fx.laptopHome);
    assert.equal(pulled.runId, RUN_ID);
    assert.equal(pulled.resumable, true);
    assert.notEqual(pulled.path, leftBehind);
    assert.ok(pulled.path.startsWith(`${leftBehind}-`), pulled.path);
    assert.equal(await fsp.readFile(path.join(leftBehind, "old.txt"), "utf8"), "keep me\n");
    assert.equal(await fsp.readFile(path.join(pulled.path, "pulled.txt"), "utf8"), "work in progress on the cloud\n");
    assert.equal(sh(pulled.path, "jj", ["log", "--no-graph", "-r", "@", "-T", "commit_id"]).trim(), exported.head);

    const record = await readRun(fx.laptop, RUN_ID);
    assert.equal(record.status, "paused");
    assert.equal(record.vcs, "jj");
    assert.equal(record.mode, "execute");
    assert.equal(record.repoRoot, fx.laptop);
    assert.equal(record.workspace.path, pulled.path);
    assert.equal(record.workspace.enabled, true);
    assert.equal(record.session.nativeSessionId, SESSION);
    assert.equal(record.migration.pulledFrom, "home");
    assert.equal(record.migration.fromCommit, exported.head);
    assert.ok(record.migration.at);
    for (const key of ["process", "movedTo", "nodeId", "planId"]) {
      assert.equal(record[key], undefined, key);
    }
    const transcript = path.join(fx.laptopHome, ".claude", "projects", encodeClaudeProjectsCwd(pulled.path), `${SESSION}.jsonl`);
    assert.match(await fsp.readFile(transcript, "utf8"), /add the pull command/);

    // Pulling the same commit again is a no-op, not a second workspace.
    const again = await importPulledRun(fx.laptop, archive, fx.laptopHome);
    assert.equal(again.path, pulled.path);

    // A live local agent with the same id is never replaced.
    await writeRun(fx.laptop, RUN_ID, { status: "running", backend: "claude", task: "x" });
    await fsp.writeFile(path.join(fx.workspace, "pulled.txt"), "newer\n");
    const newer = path.join(fx.root, "newer.tgz");
    await exportAsHome(fx, newer);
    await assert.rejects(importPulledRun(fx.laptop, newer, fx.laptopHome), /already exists in this checkout/);
  } finally {
    await fsp.rm(fx.root, { recursive: true, force: true });
  }
});

test("exporting refuses unknown and unsafe run ids", { skip: !haveJj() && "jj not installed" }, async () => {
  const fx = await fixture();
  try {
    const out = path.join(fx.root, "x.tgz");
    await assert.rejects(exportRunForPull(fx.home, "../../etc", out), /bad run id/);
    await assert.rejects(exportRunForPull(fx.home, "no-such-run", out), /no agent no-such-run/);
  } finally {
    await fsp.rm(fx.root, { recursive: true, force: true });
  }
});

test("rudder cloud pull asks the control plane, retries a waking home once, and adopts the download", { skip: !haveJj() && "jj not installed" }, async () => {
  const fx = await fixture();
  const cwd = process.cwd();
  const previousHome = process.env.HOME;
  try {
    const archive = path.join(fx.root, "pull.tgz");
    const exported = await exportAsHome(fx, archive);
    const bytes = await fsp.readFile(archive);
    const calls = [];
    let pullAttempts = 0;
    const client = {
      async request(pathname, init) {
        calls.push({ pathname, ...init });
        if (pathname === "/api/rudder/home") {
          return { projects: [{ repo: "acme/repo" }] };
        }
        if (pathname === "/api/rudder/home/pull") {
          pullAttempts += 1;
          if (pullAttempts === 1) {
            throw new Error("Rudder Cloud request failed: the cloud machine is waking (503)");
          }
          return { ...exported, repo: "acme/repo", download: "/api/rudder/home/syncs/blob-1" };
        }
        throw new Error(`unexpected ${pathname}`);
      },
      async download(pathname) {
        assert.equal(pathname, "/api/rudder/home/syncs/blob-1");
        return bytes;
      },
    };
    process.chdir(fx.laptop);
    process.env.HOME = fx.laptopHome;
    const pulled = await pullFromHome([RUN_ID, "--json"], { json: true }, { client, githubSlug: async () => "acme/repo" });
    assert.equal(pullAttempts, 2);
    assert.deepEqual(calls.find((call) => call.pathname === "/api/rudder/home/pull").body, { repo: "acme/repo", runId: RUN_ID });
    assert.equal(pulled.runId, RUN_ID);
    assert.equal(pulled.title, "add pull");
    assert.equal(await fsp.readFile(path.join(pulled.path, "pulled.txt"), "utf8"), "work in progress on the cloud\n");

    await assert.rejects(
      pullFromHome([RUN_ID], { json: true }, { client, githubSlug: async () => "acme/other" }),
      /acme\/other is not open on your cloud machine/,
    );
    await assert.rejects(pullFromHome([RUN_ID], { json: true }, { client, githubSlug: async () => null }), /local clone of the GitHub repo/);
    await assert.rejects(pullFromHome(["../x"], { json: true }, { client, githubSlug: async () => "acme/repo" }), /Usage: rudder cloud pull/);
  } finally {
    process.chdir(cwd);
    process.env.HOME = previousHome;
    await fsp.rm(fx.root, { recursive: true, force: true });
  }
});
