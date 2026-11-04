import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { syncCheckout, exportWork, importWork } from "../dist/checkout-sync.js";

const sh = (cwd, cmd, args) => execFileSync(cmd, args, { cwd, stdio: ["ignore", "pipe", "pipe"], env: { ...process.env, GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@t", GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@t", JJ_USER: "t", JJ_EMAIL: "t@t" } }).toString();

// origin (bare) <- upstream (pushes new commits) ; clone = the checkout we sync
function setup() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-sync-"));
  const origin = path.join(dir, "origin.git");
  sh(dir, "git", ["init", "-q", "--bare", "-b", "main", origin]);
  const upstream = path.join(dir, "upstream");
  sh(dir, "git", ["clone", "-q", origin, upstream]);
  fs.writeFileSync(path.join(upstream, "a.txt"), "1\n");
  sh(upstream, "git", ["add", "."]);
  sh(upstream, "git", ["commit", "-qm", "one"]);
  sh(upstream, "git", ["push", "-q", "origin", "HEAD:main"]);
  const clone = path.join(dir, "clone");
  sh(dir, "git", ["clone", "-q", origin, clone]);
  const push = (name) => {
    fs.writeFileSync(path.join(upstream, name), "x\n");
    sh(upstream, "git", ["add", "."]);
    sh(upstream, "git", ["commit", "-qm", name]);
    sh(upstream, "git", ["push", "-q", "origin", "HEAD:main"]);
  };
  return { clone, push };
}

test("git: a clean checkout behind main is fast-forwarded; a dirty one is left alone", async () => {
  const { clone, push } = setup();
  push("b.txt");
  let report = await syncCheckout(clone);
  assert.equal(report.vcs, "git");
  assert.equal(report.moved, true);
  assert.ok(fs.existsSync(path.join(clone, "b.txt")));
  push("c.txt");
  fs.writeFileSync(path.join(clone, "a.txt"), "local edit\n");
  report = await syncCheckout(clone);
  assert.equal(report.moved, false);
  assert.equal(report.dirty, true);
  assert.equal(report.behind, 1);
  assert.ok(!fs.existsSync(path.join(clone, "c.txt")));
});

test("jj: an empty working copy moves to the newest main; unpushed work is reported, not moved", async (t) => {
  try {
    execFileSync("jj", ["--version"]);
  } catch {
    t.skip("jj not installed");
    return;
  }
  const { clone, push } = setup();
  sh(clone, "jj", ["git", "init", "--colocate"]);
  sh(clone, "jj", ["new", "main@origin"]);
  push("b.txt");
  let report = await syncCheckout(clone);
  assert.equal(report.vcs, "jj");
  assert.equal(report.behind, 1);
  assert.equal(report.moved, true, report.note);
  assert.ok(fs.existsSync(path.join(clone, "b.txt")));
  assert.equal((await syncCheckout(clone)).note, "up to date");

  fs.writeFileSync(path.join(clone, "mine.txt"), "work\n");
  push("c.txt");
  report = await syncCheckout(clone);
  assert.equal(report.moved, false);
  assert.equal(report.dirty, true);
  assert.ok(fs.existsSync(path.join(clone, "mine.txt")));
  assert.ok(!fs.existsSync(path.join(clone, "c.txt")));
});

test("work travels between two clones without GitHub: hand-off, hand-back, and both editing", async (t) => {
  try {
    execFileSync("jj", ["--version"]);
  } catch {
    t.skip("jj not installed");
    return;
  }
  const { clone: cloud } = setup();
  sh(cloud, "jj", ["git", "init", "--colocate"]);
  sh(cloud, "jj", ["new", "main@origin"]);
  const laptop = path.join(path.dirname(cloud), "laptop");
  sh(path.dirname(cloud), "git", ["clone", "-q", path.join(path.dirname(cloud), "origin.git"), laptop]);
  const bundle = path.join(path.dirname(cloud), "w.bundle");
  const send = async (from, to, side) => {
    fs.rmSync(bundle, { force: true });
    const sent = await exportWork(from, bundle);
    assert.ok(sent, "there was work to send");
    return await importWork(to, bundle, sent.head, side);
  };

  // Work exists only on the cloud, uncommitted (the snap case).
  fs.writeFileSync(path.join(cloud, "model.py"), "v1\n");
  let got = await send(cloud, laptop, "cloud");
  assert.equal(got.outcome, "moved", got.note);
  assert.equal(fs.readFileSync(path.join(laptop, "model.py"), "utf8"), "v1\n");
  assert.equal(await exportWork(cloud, bundle).then(() => null), null);

  // The laptop carries on and hands it back; the cloud fast-forwards.
  fs.writeFileSync(path.join(laptop, "model.py"), "v2\n");
  got = await send(laptop, cloud, "laptop");
  assert.equal(got.outcome, "moved", got.note);
  assert.equal(fs.readFileSync(path.join(cloud, "model.py"), "utf8"), "v2\n");

  // Sending the same thing again changes nothing.
  fs.rmSync(bundle, { force: true });
  const again = await exportWork(cloud, bundle);
  assert.equal((await importWork(laptop, bundle, again.head, "cloud")).outcome, "have");

  // The same edit made on both sides is not a conflict.
  fs.writeFileSync(path.join(cloud, "same.txt"), "s\n");
  fs.writeFileSync(path.join(laptop, "same.txt"), "s\n");
  got = await send(cloud, laptop, "cloud");
  assert.equal(got.outcome, "moved", got.note);

  // Both edit: nothing is overwritten, the other version waits at a bookmark.
  fs.writeFileSync(path.join(cloud, "model.py"), "cloud\n");
  fs.writeFileSync(path.join(laptop, "model.py"), "laptop\n");
  got = await send(cloud, laptop, "cloud");
  assert.equal(got.outcome, "diverged", got.note);
  assert.equal(fs.readFileSync(path.join(laptop, "model.py"), "utf8"), "laptop\n");
  assert.match(sh(laptop, "jj", ["bookmark", "list"]), /rudder-sync\/cloud/);
  // A gitignored file never travels.
  assert.ok(!fs.existsSync(path.join(laptop, ".env")));
});
