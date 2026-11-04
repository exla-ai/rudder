import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { createNodeWorkspace, forgetWorkspace } from "../dist/jj.js";
import { buildWarmSnapshot, currentBaseCommit, warmCommands } from "../dist/warm-snapshot.js";

const FILES = 3000;
const FORKS = 10;
const env = { ...process.env, JJ_USER: "Rudder Test", JJ_EMAIL: "test@invalid.example" };
const sh = (cwd, file, args) => execFileSync(file, args, { cwd, env, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });

function countFiles(dir) {
  let n = 0;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    n += entry.isDirectory() ? countFiles(path.join(dir, entry.name)) : 1;
  }
  return n;
}

test("warmCommands reads rudder.toml and detects lockfiles", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-warm-cmd-"));
  try {
    fs.writeFileSync(path.join(dir, "package-lock.json"), "{}");
    fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify({ scripts: { build: "tsc" } }));
    fs.writeFileSync(path.join(dir, "uv.lock"), "");
    assert.deepEqual(warmCommands(dir), ["npm ci", "npm run build", "uv sync"]);
    fs.writeFileSync(path.join(dir, "rudder.toml"), '[warm]\ncommands = ["make deps", "make"]\n[other]\n');
    assert.deepEqual(warmCommands(dir), ["make deps", "make"]);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("ten agents fork the warm snapshot in seconds without copying files", { skip: process.platform !== "linux" }, async (t) => {
  // OverlayFS needs a real disk for upper layers (Cloud's /tmp is not one).
  const scratch = process.env.RUDDER_TEST_DISK ?? (fs.existsSync("/workspace/rudder-scratch") ? "/workspace/rudder-scratch" : os.tmpdir());
  const root = fs.mkdtempSync(path.join(scratch, "rudder-warm-"));
  const repo = path.join(root, "repo");
  const forks = [];
  t.after(async () => {
    for (const fork of forks) await forgetWorkspace({ repoRoot: repo, ...fork }).catch(() => undefined);
    fs.rmSync(root, { recursive: true, force: true });
  });
  fs.mkdirSync(path.join(repo, "src"), { recursive: true });
  for (let i = 0; i < FILES; i += 1) fs.writeFileSync(path.join(repo, "src", `f${i}.txt`), `${i}\n`);
  fs.writeFileSync(path.join(repo, ".gitignore"), "node_modules\n.rudder-workspaces\n");
  fs.writeFileSync(path.join(repo, "rudder.toml"), '[warm]\ncommands = ["mkdir -p node_modules/dep && echo dep > node_modules/dep/index.js"]\n');
  sh(repo, "git", ["init", "-q"]);
  sh(repo, "git", ["add", "."]);
  sh(repo, "git", ["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
  sh(repo, "jj", ["git", "init", "--colocate"]);

  const base = await currentBaseCommit(repo);
  assert.ok(base);
  const snapshot = await buildWarmSnapshot(repo, base);
  assert.ok(snapshot, "warm snapshot built");

  const started = Date.now();
  for (let i = 0; i < FORKS; i += 1) {
    const ws = await createNodeWorkspace({ repoRoot: repo, nodeId: `fork${i}`, task: `task ${i}` });
    forks.push({ workspaceName: ws.workspaceName, workspacePath: ws.path });
  }
  const elapsed = Date.now() - started;
  const enter = (i) => `${forks[i].workspacePath}.fork/enter`;
  if (!fs.existsSync(enter(0))) {
    t.skip("rootless OverlayFS unavailable here; launches used full checkouts");
    return;
  }
  const du = spawnSync("du", ["-skc", forks[0].workspacePath, `${forks[0].workspacePath}.fork`], { encoding: "utf8" });
  const disk = Number(du.stdout.trim().split("\n").pop().split("\t")[0]);
  t.diagnostic(`${FORKS} forks of ${FILES} files took ${elapsed} ms (${Math.round(elapsed / FORKS)} ms each), ${disk} KiB per fork`);
  assert.ok(elapsed < 20_000, `forks took ${elapsed} ms`);
  for (const fork of forks) {
    // Only jj metadata on disk; no source file was copied.
    assert.deepEqual(fs.readdirSync(fork.workspacePath), [".jj"]);
    assert.equal(countFiles(`${fork.workspacePath}.fork/upper`), 0);
  }

  const inFork = (i, script) => spawnSync(enter(i), ["sh", "-ec", script], { cwd: forks[i].workspacePath, env, encoding: "utf8" });
  const edit = inFork(0, "echo changed > src/f1.txt; echo new > new.txt; cat node_modules/dep/index.js; jj commit -m agentwork");
  assert.equal(edit.status, 0, edit.stderr);
  assert.match(edit.stdout, /dep/);

  const other = inFork(1, "cat src/f1.txt; ls new.txt 2>/dev/null || echo absent; cat node_modules/dep/index.js; id -u; jj status");
  assert.equal(other.status, 0, other.stderr);
  assert.match(other.stdout, /^1\nabsent\ndep\n/);
  assert.match(other.stdout, new RegExp(`\\n${process.getuid()}\\n`));
  assert.match(other.stdout, /The working copy has no changes/);

  // The fork's commit is in the main repo and lands there.
  const landed = sh(repo, "jj", ["file", "show", "-r", "description(substring:agentwork)", "new.txt"]);
  assert.equal(landed, "new\n");
  sh(repo, "jj", ["new", "@", "description(substring:agentwork)", "-m", "integrate"]);
  assert.equal(fs.readFileSync(path.join(repo, "new.txt"), "utf8"), "new\n");
  assert.equal(fs.readFileSync(path.join(snapshot.dir, "src", "f1.txt"), "utf8"), "1\n", "snapshot untouched");
});
