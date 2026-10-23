import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { colocateRefusal, ensureColocated } from "../dist/jj.js";

function tmp(name) {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), `rudder-colocate-${name}-`)));
}

function gitInit(dir) {
  fs.mkdirSync(dir, { recursive: true });
  execFileSync("git", ["init", "-q", dir]);
}

test("a plain folder is never turned into a repo", async () => {
  const dir = tmp("plain");
  fs.writeFileSync(path.join(dir, "notes.txt"), "hi");
  assert.match(colocateRefusal(dir), /isn't a git repo/);
  await assert.rejects(ensureColocated(dir), /isn't a git repo/);
  assert.equal(fs.existsSync(path.join(dir, ".git")), false);
  assert.equal(fs.existsSync(path.join(dir, ".jj")), false);
});

test("a subfolder of a git repo is not colocated on its own", () => {
  const dir = tmp("sub");
  gitInit(dir);
  fs.mkdirSync(path.join(dir, "pkg"));
  assert.match(colocateRefusal(path.join(dir, "pkg")), /isn't a git repo/);
});

test("a folder that holds other repos is refused even with its own .git", () => {
  const dir = tmp("parent");
  gitInit(dir);
  gitInit(path.join(dir, "alpha"));
  gitInit(path.join(dir, "beta"));
  assert.match(colocateRefusal(dir), /holds other projects \(alpha, beta\)/);
});

test("the home folder is refused", () => {
  assert.match(colocateRefusal(os.homedir()), /will not set up version control/);
});

test("a git repo root may be colocated", () => {
  const dir = tmp("repo");
  gitInit(dir);
  gitInit(path.join(dir, "vendored"));
  assert.equal(colocateRefusal(dir), null);
});
