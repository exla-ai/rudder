import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { createNodeWorkspace, forgetWorkspace } from "../dist/jj.js";
import { buildWarmSnapshot, createForkWorkspace, currentBaseCommit, inFork, pruneWarmSnapshots, readySnapshots, seedFromPrevious, warmBuildSkipReason, warmCommands, warmDir } from "../dist/warm-snapshot.js";
import { rudderJjPath } from "../dist/util.js";

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

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * A launch starts the project's detached warm builder, which follows the base.
 * Let a build in progress finish, then stop the builder (its process group).
 */
async function waitForBuilders(repo) {
  const names = () => (fs.existsSync(warmDir(repo)) ? fs.readdirSync(warmDir(repo)) : []);
  for (let i = 0; i < 150 && names().some((n) => n.endsWith(".building")); i += 1) await sleep(200);
  try {
    const pid = Number(fs.readFileSync(path.join(warmDir(repo), "builder.pid"), "utf8"));
    if (pid) process.kill(-pid, "SIGTERM");
  } catch {
    /* no builder running */
  }
}

/** Removes a test root, including OverlayFS work dirs (mode 000) a stopped build left. */
function removeRoot(root) {
  spawnSync("chmod", ["-R", "u+rwX", root]);
  fs.rmSync(root, { recursive: true, force: true });
}

/** Every file under cwd but runtime dirs: path -> "file|exec <bytes>" or "link <target>". */
const MANIFEST = `
const fs = require("node:fs");
const skip = new Set([".git", ".jj", ".rudder-workspaces", "node_modules"]);
const out = {};
(function walk(dir) {
  for (const name of fs.readdirSync(dir).sort()) {
    if (dir === "." && skip.has(name)) continue;
    const p = dir === "." ? name : dir + "/" + name;
    const st = fs.lstatSync(p);
    if (st.isDirectory()) walk(p);
    else if (st.isSymbolicLink()) out[p] = "link " + fs.readlinkSync(p);
    else out[p] = (st.mode & 0o111 ? "exec " : "file ") + fs.readFileSync(p, "latin1");
  }
})(".");
process.stdout.write(JSON.stringify(out));
`;

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

test("seedFromPrevious copies only untracked outputs from the previous snapshot", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-warm-seed-"));
  try {
    const prev = path.join(root, "prev");
    const next = path.join(root, "next");
    fs.mkdirSync(path.join(prev, "node_modules", "dep"), { recursive: true });
    fs.mkdirSync(path.join(prev, ".jj"));
    fs.mkdirSync(next);
    fs.writeFileSync(path.join(prev, "node_modules", "dep", "index.js"), "dep\n");
    fs.writeFileSync(path.join(prev, "src.txt"), "old\n");
    fs.writeFileSync(path.join(next, "src.txt"), "new\n");
    return seedFromPrevious(prev, next).then(() => {
      assert.equal(fs.readFileSync(path.join(next, "node_modules", "dep", "index.js"), "utf8"), "dep\n");
      assert.equal(fs.readFileSync(path.join(next, "src.txt"), "utf8"), "new\n");
      assert.equal(fs.existsSync(path.join(next, ".jj")), false);
    }).finally(() => fs.rmSync(root, { recursive: true, force: true }));
  } catch (error) {
    fs.rmSync(root, { recursive: true, force: true });
    throw error;
  }
});

test("ten agents fork the warm snapshot in seconds without copying files", { skip: process.platform !== "linux" || !rudderJjPath() }, async (t) => {
  // OverlayFS needs a real disk for upper layers (Cloud's /tmp is not one).
  const scratch = process.env.RUDDER_TEST_DISK ?? (fs.existsSync("/workspace/rudder-scratch") ? "/workspace/rudder-scratch" : os.tmpdir());
  const root = fs.mkdtempSync(path.join(scratch, "rudder-warm-"));
  const repo = path.join(root, "repo");
  const forks = [];
  t.after(async () => {
    for (const fork of forks) await forgetWorkspace({ repoRoot: repo, ...fork }).catch(() => undefined);
    await waitForBuilders(repo);
    removeRoot(root);
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
    assert.equal(countFiles(`${fork.workspacePath}/.jj/working_copy/upper`), 0);
  }

  const inFork = (i, script) => spawnSync(enter(i), ["sh", "-ec", script], { cwd: forks[i].workspacePath, env, encoding: "utf8" });
  const edit = inFork(0, "echo changed > src/f1.txt; echo new > new.txt; cat node_modules/dep/index.js; jj commit -m agentwork");
  assert.equal(edit.status, 0, edit.stderr);
  assert.match(edit.stdout, /dep/);

  // rudder-overlay snapshot reads only the upper layer: exactly the changed paths.
  const changed = inFork(2, "echo x > src/f7.txt; rm src/f8.txt; echo y > added.txt; echo z > node_modules/dep/extra.js");
  assert.equal(changed.status, 0, changed.stderr);
  const snapStarted = Date.now();
  const summary = sh(forks[2].workspacePath, rudderJjPath(), ["diff", "--summary"]);
  t.diagnostic(`fork snapshot (jj diff) took ${Date.now() - snapStarted} ms`);
  assert.deepEqual(summary.trim().split("\n").sort(), ["A added.txt", "D src/f8.txt", "M src/f7.txt"]);

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

  // Branching an agent (dashboard `b`) forks too, starting at that agent's change.
  const change = sh(repo, "jj", ["log", "--no-graph", "-r", "description(substring:agentwork)", "-T", "change_id"]).trim();
  const branch = await createNodeWorkspace({ repoRoot: repo, nodeId: "branch0", task: "branch", atChangeId: change });
  forks.push({ workspaceName: branch.workspaceName, workspacePath: branch.path });
  assert.ok(fs.existsSync(`${branch.path}.fork/enter`), "branch is an overlay fork");
  const seen = spawnSync(`${branch.path}.fork/enter`, ["sh", "-ec", "cat src/f1.txt new.txt node_modules/dep/index.js; jj log --no-graph -r @- -T change_id"], { cwd: branch.path, env, encoding: "utf8" });
  assert.equal(seen.status, 0, seen.stderr);
  assert.equal(seen.stdout, `changed\nnew\ndep\n${change}`);
});

test("a fork moved onto a newer base matches it byte for byte, symlinks and modes included", { skip: process.platform !== "linux" || !rudderJjPath() }, async (t) => {
  const scratch = process.env.RUDDER_TEST_DISK ?? (fs.existsSync("/workspace/rudder-scratch") ? "/workspace/rudder-scratch" : os.tmpdir());
  const root = fs.mkdtempSync(path.join(scratch, "rudder-warm-move-"));
  const repo = path.join(root, "repo");
  const forks = [];
  t.after(async () => {
    for (const fork of forks) await forgetWorkspace({ repoRoot: repo, ...fork }).catch(() => undefined);
    await waitForBuilders(repo);
    removeRoot(root);
  });
  const put = (rel, text, mode = 0o644) => {
    const p = path.join(repo, rel);
    fs.rmSync(p, { recursive: true, force: true });
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, text);
    fs.chmodSync(p, mode);
  };
  const link = (rel, target) => {
    const p = path.join(repo, rel);
    fs.rmSync(p, { recursive: true, force: true });
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.symlinkSync(target, p);
  };
  const rm = (rel) => fs.rmSync(path.join(repo, rel), { recursive: true, force: true });
  for (let i = 0; i < 50; i += 1) put(`src/f${i}.txt`, `${i}\n`);
  put("gone/a.txt", "a\n");
  put("gone/sub/b.txt", "b\n");
  put("dir2file/inner.txt", "inner\n");
  put("file2dir", "file\n");
  put("bin/on.sh", "on\n");
  put("bin/off.sh", "off\n", 0o755);
  link("ln/retarget", "one");
  put("ln/f2l", "file\n");
  link("ln/l2f", "link");
  link("ln/del", "../src");
  put('odd/del "me".txt', "del\n");
  put("odd/mod\nme.txt", "old\n");
  put(".gitignore", "node_modules\n.rudder-workspaces\n*.log\n");
  // The build leaves untracked output in a directory the base turns into a file.
  put("rudder.toml", '[warm]\ncommands = ["mkdir -p node_modules/dep && echo dep > node_modules/dep/index.js && echo built > dir2file/out.log"]\n');
  sh(repo, "git", ["init", "-q"]);
  sh(repo, "git", ["add", "."]);
  sh(repo, "git", ["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
  sh(repo, "jj", ["git", "init", "--colocate"]);
  const snapshot = await buildWarmSnapshot(repo, await currentBaseCommit(repo));
  assert.ok(snapshot, "warm snapshot built");

  // Main moves on: deletions, file/directory and file/symlink swaps, mode-only
  // changes, odd names, and attributes that would convert files on checkout.
  rm("gone");
  put("dir2file", "now a file\n");
  rm("file2dir");
  put("file2dir/inner.txt", "now a dir\n");
  put("src/f1.txt", "changed\n");
  fs.chmodSync(path.join(repo, "bin/on.sh"), 0o755);
  fs.chmodSync(path.join(repo, "bin/off.sh"), 0o644);
  link("ln/retarget", "two");
  link("ln/f2l", "now a link");
  put("ln/l2f", "now a file\n");
  rm("ln/del");
  link("ln/new", "../src/f0.txt");
  rm('odd/del "me".txt');
  put("odd/mod\nme.txt", "new\n");
  for (const name of ["with space.txt", 'quote"d.txt', "single'q.txt", "new\nline.txt", "-dash.txt", "back\\slash.txt", "ünï 文.txt", "$x*?.txt"]) put(`odd/${name}`, `${name}\n`);
  put(".gitattributes", "*.txt text eol=crlf ident\n");
  put("ident.txt", "$Id$\n");
  sh(repo, "jj", ["commit", "-m", "main moved"]);
  const base = await currentBaseCommit(repo);
  assert.notEqual(base, snapshot.commit);

  // The main checkout holds the base as jj wrote it.
  const manifest = (cwd, command, args = []) => {
    const ran = spawnSync(command, [...args, "-e", MANIFEST], { cwd, env, encoding: "utf8" });
    assert.equal(ran.status, 0, ran.stderr);
    return JSON.parse(ran.stdout);
  };
  const want = manifest(repo, process.execPath);
  assert.equal(want["ln/retarget"], "link two");
  assert.equal(want["bin/on.sh"], "exec on\n");
  assert.equal(want["ident.txt"], "file $Id$\n");
  assert.equal(want["odd/new\nline.txt"], "file new\nline.txt\n");
  assert.equal(want["gone/a.txt"], undefined);

  const ws = path.join(repo, ".rudder-workspaces", "moved");
  const made = await createForkWorkspace({ repoRoot: repo, targetPath: ws, workspaceName: "rudder-moved" });
  forks.push({ workspaceName: "rudder-moved", workspacePath: ws });
  if (/overlay mount unavailable/.test(made.reason ?? "")) {
    t.skip("rootless OverlayFS unavailable here");
    return;
  }
  assert.equal(made.reason, undefined);
  assert.equal(made.snapshot.key, snapshot.key);
  const enter = `${ws}.fork/enter`;
  const status = spawnSync(enter, ["sh", "-ec", "jj log --no-graph -r @- -T 'commit_id ++ \"\\n\"'; jj diff -r @ --summary; cat node_modules/dep/index.js"], { cwd: ws, env, encoding: "utf8" });
  assert.equal(status.status, 0, status.stderr);
  // On the base with nothing to commit, and still on the warm lower.
  assert.equal(status.stdout, `${base}\ndep\n`);
  assert.deepEqual(manifest(ws, enter, [process.execPath]), want);
});

test("refreshes stack on the previous snapshot, follow the base, compact when deep, and keep what forks use", { skip: process.platform !== "linux" || !rudderJjPath() }, async (t) => {
  const scratch = process.env.RUDDER_TEST_DISK ?? (fs.existsSync("/workspace/rudder-scratch") ? "/workspace/rudder-scratch" : os.tmpdir());
  const root = fs.mkdtempSync(path.join(scratch, "rudder-warm-stack-"));
  const repo = path.join(root, "repo");
  const forks = [];
  t.after(async () => {
    for (const fork of forks) await forgetWorkspace({ repoRoot: repo, ...fork }).catch(() => undefined);
    await waitForBuilders(repo);
    removeRoot(root);
  });
  const put = (rel, text) => {
    fs.mkdirSync(path.dirname(path.join(repo, rel)), { recursive: true });
    fs.writeFileSync(path.join(repo, rel), text);
  };
  for (let i = 0; i < 200; i += 1) put(`src/f${i}.txt`, `${i}\n`);
  put("gone/a.txt", "a\n");
  put(".gitignore", "node_modules\n.rudder-workspaces\n*.log\n");
  // Every build appends to a file the builds before it left, and copies a source file.
  put("rudder.toml", '[warm]\ncommands = ["mkdir -p node_modules/dep && echo run >> node_modules/dep/runs && cp src/f1.txt node_modules/dep/built"]\n');
  sh(repo, "git", ["init", "-q"]);
  sh(repo, "git", ["add", "."]);
  sh(repo, "git", ["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
  sh(repo, "jj", ["git", "init", "--colocate"]);
  // About ten files change per move: edits, an addition and a deletion.
  const move = (n) => {
    for (let i = 0; i < 8; i += 1) put(`src/f${i}.txt`, `${i} v${n}\n`);
    put(`added${n}.txt`, `${n}\n`);
    fs.rmSync(path.join(repo, n === 2 ? "gone" : `added${n - 1}.txt`), { recursive: true, force: true });
    sh(repo, "jj", ["commit", "-m", `move ${n}`]);
  };
  const manifest = (cwd, command, args = []) => {
    const ran = spawnSync(command, [...args, "-e", MANIFEST], { cwd, env, encoding: "utf8" });
    assert.equal(ran.status, 0, ran.stderr);
    return JSON.parse(ran.stdout);
  };
  const fork = async (nodeId) => {
    const ws = await createNodeWorkspace({ repoRoot: repo, nodeId, task: nodeId });
    forks.push({ workspaceName: ws.workspaceName, workspacePath: ws.path });
    return { ...ws, enter: `${ws.path}.fork/enter` };
  };

  const s1 = await buildWarmSnapshot(repo, await currentBaseCommit(repo));
  assert.ok(s1, "first snapshot built");
  assert.equal(s1.layers, undefined);
  move(2);
  const s2 = await buildWarmSnapshot(repo, await currentBaseCommit(repo));
  assert.deepEqual(s2.layers, [s2.dir, s1.dir]);
  assert.ok(countFiles(s2.dir) <= 15, `a thin layer, not a copy (${countFiles(s2.dir)} files)`);

  // A fork of the stack holds the base byte for byte plus both builds' output,
  // and the .gitignore that only the bottom layer has still applies.
  // (Taken first: a launch adds Rudder's lines to the checkout's .gitignore.)
  const want2 = manifest(repo, process.execPath);
  const f2 = await fork("stack2");
  if (!fs.existsSync(f2.enter)) {
    t.skip("rootless OverlayFS unavailable here; launches used full checkouts");
    return;
  }
  assert.deepEqual(manifest(f2.path, f2.enter, [process.execPath]), want2);
  const seen = spawnSync(f2.enter, ["sh", "-ec", "cat node_modules/dep/runs node_modules/dep/built; echo x > out.log; echo y > new.txt; jj diff --summary"], { cwd: f2.path, env, encoding: "utf8" });
  assert.equal(seen.status, 0, seen.stderr);
  assert.equal(seen.stdout, "run\nrun\n1 v2\nA new.txt\n");

  // The base moves: the builder that launch started refreshes in the background.
  move(3);
  const base3 = await currentBaseCommit(repo);
  let s3;
  for (let i = 0; i < 150 && !s3; i += 1) {
    s3 = readySnapshots(repo).find((snap) => snap.commit === base3);
    if (!s3) await sleep(200);
  }
  assert.ok(s3, "the builder followed the base");
  assert.deepEqual(s3.layers, [s3.dir, s2.dir, s1.dir]);

  // A refresh on a stack as deep as the limit is a plain copy.
  await waitForBuilders(repo);
  move(4);
  const s4 = await buildWarmSnapshot(repo, await currentBaseCommit(repo), { maxLayers: 3 });
  assert.equal(s4.layers, undefined);
  assert.deepEqual(fs.readdirSync(path.join(s4.dir, ".jj")), [], "not a jj workspace, only a mount point");
  assert.equal(fs.readFileSync(path.join(s4.dir, "node_modules/dep/runs"), "utf8"), "run\n".repeat(4));
  const want4 = manifest(repo, process.execPath);
  const f4 = await fork("flat4");
  assert.deepEqual(manifest(f4.path, f4.enter, [process.execPath]), want4);

  // That build pruned s3 but kept the layers the first fork mounts (s2 on s1).
  const keys = () => readySnapshots(repo).map((snap) => snap.key).sort();
  assert.deepEqual(keys(), [s1.key, s2.key, s4.key].sort());
  assert.equal(fs.existsSync(s3.dir), false);
  // Once that fork is gone, so are its layers.
  await forgetWorkspace({ repoRoot: repo, ...forks.shift() });
  await pruneWarmSnapshots(repo);
  assert.deepEqual(keys(), [s4.key]);
  assert.equal(fs.existsSync(s2.dir) || fs.existsSync(s1.dir), false);
});

// macOS: forks are APFS clones of the warm snapshot (rudder-jj rudder-clone-add).
const onMac = { skip: process.platform !== "darwin" || !rudderJjPath() };
const BIG = 32 * 1024 * 1024;
const freeBytes = (dir) => { const s = fs.statfsSync(dir); return s.bavail * s.bsize; };
const jjOut = (cwd, args) => sh(cwd, "jj", args);
const workspaceNames = (repo) => jjOut(repo, ["workspace", "list", "-T", 'name ++ "\\n"']).trim().split("\n");

/** A colocated repo whose warm build installs node_modules and churns tracked files. */
async function macRepo(t, warm = "mkdir -p node_modules/dep && echo dep > node_modules/dep/index.js && echo churn >> src/f1.txt && echo built > out.txt") {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "rudder-clone-")));
  const repo = path.join(root, "repo");
  const forks = [];
  t.after(async () => {
    for (const fork of forks) await forgetWorkspace({ repoRoot: repo, ...fork }).catch(() => undefined);
    await waitForBuilders(repo);
    removeRoot(root);
  });
  fs.mkdirSync(path.join(repo, "src"), { recursive: true });
  for (let i = 0; i < 200; i += 1) fs.writeFileSync(path.join(repo, "src", `f${i}.txt`), `${i}\n`);
  fs.writeFileSync(path.join(repo, ".gitignore"), "node_modules\n.rudder-workspaces\n");
  fs.writeFileSync(path.join(repo, "rudder.toml"), `[warm]\ncommands = [${JSON.stringify(warm)}]\n`);
  sh(repo, "git", ["init", "-q"]);
  sh(repo, "git", ["add", "."]);
  sh(repo, "git", ["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
  sh(repo, "jj", ["git", "init", "--colocate"]);
  const base = await currentBaseCommit(repo);
  const snapshot = await buildWarmSnapshot(repo, base);
  assert.ok(snapshot, "warm snapshot built");
  const fork = async (nodeId, extra = {}) => {
    const ws = await createNodeWorkspace({ repoRoot: repo, nodeId, task: `task ${nodeId}`, ...extra });
    forks.push({ workspaceName: ws.workspaceName, workspacePath: ws.path });
    return ws;
  };
  return { repo, base, snapshot, fork };
}

test("macOS: an agent workspace is a clean APFS clone of the warm snapshot", onMac, async (t) => {
  const { repo, base, snapshot, fork } = await macRepo(t);
  // Heavy dependencies: a clone must not copy them.
  fs.writeFileSync(path.join(snapshot.dir, "node_modules", "big.bin"), Buffer.alloc(BIG, 7));

  let ws;
  let used = Infinity;
  for (let i = 0; i < 3 && used >= BIG / 2; i += 1) {
    const before = freeBytes(repo);
    ws = await fork(`clone${i}`);
    used = Math.min(used, before - freeBytes(repo));
  }
  assert.ok(used < BIG / 2, `a fork used ${used} bytes of disk; its files must be clones`);

  // An ordinary jj workspace: no overlay state, nothing to enter, no .git.
  assert.equal(fs.existsSync(`${ws.path}.fork`), false);
  assert.equal(fs.existsSync(path.join(ws.path, ".git")), false);
  assert.deepEqual(inFork(ws.path, "claude", ["-p"]), { command: "claude", args: ["-p"] });
  assert.match(jjOut(ws.path, ["status"]), /The working copy has no changes/);
  assert.equal(jjOut(ws.path, ["log", "--no-graph", "-r", "@-", "-T", "commit_id"]), base);
  assert.match(jjOut(ws.path, ["log", "--no-graph", "-r", "@", "-T", "description"]), /task clone/);

  // Warm dependencies came along; files the build churned are back at the commit.
  assert.equal(fs.readFileSync(path.join(ws.path, "node_modules", "dep", "index.js"), "utf8"), "dep\n");
  assert.equal(fs.statSync(path.join(ws.path, "node_modules", "big.bin")).size, BIG);
  assert.equal(fs.readFileSync(path.join(ws.path, "src", "f1.txt"), "utf8"), "1\n");
  assert.equal(fs.existsSync(path.join(ws.path, "out.txt")), false);

  // The snapshot's recorded file states are trusted, not rehashed: a same-size
  // edit with the old mtime goes unseen, a real edit is seen.
  const f2 = path.join(ws.path, "src", "f2.txt");
  // jj keeps whole milliseconds; land mid-millisecond so float rounding in
  // utimes cannot move the mtime into the next or previous one.
  const sameMs = (Math.floor(fs.statSync(f2).mtimeMs) + 0.5) / 1000;
  fs.writeFileSync(f2, "X\n");
  fs.utimesSync(f2, sameMs, sameMs);
  fs.writeFileSync(path.join(ws.path, "src", "f3.txt"), "changed\n");
  assert.equal(jjOut(ws.path, ["diff", "--summary"]), "M src/f3.txt\n");

  // Writes stay private to the fork.
  fs.writeFileSync(path.join(ws.path, "node_modules", "dep", "index.js"), "mine\n");
  assert.equal(fs.readFileSync(path.join(snapshot.dir, "node_modules", "dep", "index.js"), "utf8"), "dep\n");
  assert.equal(fs.readFileSync(path.join(snapshot.dir, "src", "f1.txt"), "utf8"), "1\nchurn\n");

  // Removal works as for a full checkout, and later forks still work.
  await forgetWorkspace({ repoRoot: repo, workspaceName: ws.workspaceName, workspacePath: ws.path });
  assert.equal(fs.existsSync(ws.path), false);
  assert.ok(!workspaceNames(repo).includes(ws.workspaceName));
  const again = await fork("again");
  assert.match(jjOut(again.path, ["status"]), /The working copy has no changes/);
});

test("macOS: a clone fork starts at current main, or at a branched change", onMac, async (t) => {
  const { repo, snapshot, fork } = await macRepo(t);
  fs.writeFileSync(path.join(repo, "src", "f2.txt"), "newer\n");
  fs.rmSync(path.join(repo, "src", "f3.txt"));
  fs.writeFileSync(path.join(repo, "added.txt"), "added\n");
  jjOut(repo, ["commit", "-m", "main moved"]);
  const base = await currentBaseCommit(repo);
  assert.notEqual(base, snapshot.commit);

  const ws = await fork("current");
  assert.equal(jjOut(ws.path, ["log", "--no-graph", "-r", "@-", "-T", "commit_id"]), base);
  assert.match(jjOut(ws.path, ["status"]), /The working copy has no changes/);
  assert.equal(fs.readFileSync(path.join(ws.path, "src", "f2.txt"), "utf8"), "newer\n");
  assert.equal(fs.existsSync(path.join(ws.path, "src", "f3.txt")), false);
  assert.equal(fs.readFileSync(path.join(ws.path, "added.txt"), "utf8"), "added\n");
  assert.equal(fs.readFileSync(path.join(ws.path, "node_modules", "dep", "index.js"), "utf8"), "dep\n");
  // As on Linux, the builder the launch started refreshes for the new base (when the disk has room).
  const roomy = warmBuildSkipReason(warmDir(repo)) === null;
  let started = false;
  for (let i = 0; i < (roomy ? 75 : 1) && !started; i += 1) {
    started = fs.readdirSync(warmDir(repo)).some((n) => n.startsWith(base.slice(0, 12)));
    if (!started) await sleep(200);
  }
  assert.equal(started, roomy, "warm build started for the new base");

  // Branching an agent forks from that agent's change.
  fs.writeFileSync(path.join(ws.path, "agent.txt"), "agent\n");
  jjOut(ws.path, ["commit", "-m", "agentwork"]);
  const change = jjOut(repo, ["log", "--no-graph", "-r", "description(substring:agentwork)", "-T", "change_id"]).trim();
  const branch = await fork("branch", { atChangeId: change });
  assert.equal(jjOut(branch.path, ["log", "--no-graph", "-r", "@-", "-T", "change_id"]), change);
  assert.match(jjOut(branch.path, ["status"]), /The working copy has no changes/);
  assert.equal(fs.readFileSync(path.join(branch.path, "agent.txt"), "utf8"), "agent\n");
  assert.equal(fs.readFileSync(path.join(branch.path, "src", "f2.txt"), "utf8"), "newer\n");
  assert.equal(fs.readFileSync(path.join(branch.path, "node_modules", "dep", "index.js"), "utf8"), "dep\n");
});

test("warm builds wait for disk room on macOS and always run on Linux", () => {
  const dir = os.tmpdir();
  assert.equal(warmBuildSkipReason(dir, 0), null);
  const tooMuch = warmBuildSkipReason(dir, Number.MAX_SAFE_INTEGER);
  if (process.platform === "darwin") assert.match(tooMuch ?? "", /GB free, a warm build waits for/);
  else assert.equal(tooMuch, null);
});

test("macOS: a failed clone leaves nothing behind and the launch makes a full checkout", onMac, async (t) => {
  const { repo, base, snapshot, fork } = await macRepo(t);
  // clonefile refuses to write into a read-only directory: a reason, no workspace.
  const locked = path.join(repo, ".rudder-workspaces", "locked");
  fs.mkdirSync(locked, { recursive: true });
  fs.chmodSync(locked, 0o500);
  let failed;
  try {
    failed = await createForkWorkspace({ repoRoot: repo, targetPath: path.join(locked, "ws"), workspaceName: "rudder-locked" });
  } finally {
    fs.chmodSync(locked, 0o700);
  }
  assert.match(failed.reason ?? "", /APFS clone .* failed/);
  assert.equal(fs.existsSync(path.join(locked, "ws")), false);
  assert.ok(!workspaceNames(repo).includes("rudder-locked"));

  // A snapshot that is not a jj workspace: the launch falls back to a full checkout.
  const broken = path.join(warmDir(repo), "broken");
  fs.mkdirSync(path.join(broken, ".jj"), { recursive: true });
  fs.writeFileSync(path.join(warmDir(repo), "broken.ready"), JSON.stringify({ key: "broken", commit: base, dir: broken, builtAt: "2999-01-01T00:00:00.000Z" }));
  const ws = await fork("fallback");
  assert.match(jjOut(ws.path, ["status"]), /The working copy has no changes/);
  assert.equal(fs.existsSync(path.join(ws.path, "node_modules")), false, "full checkout, not a clone");
  assert.equal(fs.readFileSync(path.join(ws.path, "src", "f1.txt"), "utf8"), "1\n");
  assert.ok(fs.existsSync(path.join(snapshot.dir, "node_modules")), "the real snapshot is untouched");
});
