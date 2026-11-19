import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import { build } from "esbuild";

async function load(entry) {
  const result = await build({ entryPoints: [new URL(entry, import.meta.url).pathname], bundle: true, write: false, platform: "node", format: "esm", target: "node22" });
  return import(`data:text/javascript;base64,${Buffer.from(result.outputFiles[0].text).toString("base64")}`);
}
const { projectNotes } = await load("../src/project-files/notes.ts");
const { ProjectFileRoutes } = await load("../cloud/src/project-files/routes.ts");
const sha = (text) => createHash("sha256").update(text).digest("hex");
const env = { ...process.env, GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@t", GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@t", JJ_USER: "t", JJ_EMAIL: "t@t" };
// projectNotes runs jj itself: the same identity, and jj from PATH instead of
// the rudder-jj beside a native binary.
Object.assign(process.env, { JJ_USER: "t", JJ_EMAIL: "t@t", RUDDER_JJ_BIN: "" });
const sh = (cwd, cmd, args) => execFileSync(cmd, args, { cwd, env, stdio: ["ignore", "pipe", "pipe"] }).toString();
const read = (repo) => projectNotes(repo, { version: 1, operation: "read" });
const write = (repo, baseRevision, text) => projectNotes(repo, { version: 1, operation: "write", baseRevision, text });
const keepOut = (repo, on) => projectNotes(repo, { version: 1, operation: "git", keepOut: on });

async function repo(t, { jj }) {
  const dir = await realpath(await mkdtemp(path.join(os.tmpdir(), "rudder-notes-")));
  t.after(() => rm(dir, { recursive: true, force: true }));
  sh(dir, "git", ["init", "-q", "-b", "main"]);
  await writeFile(path.join(dir, "README.md"), "# App\n");
  sh(dir, "git", ["add", "README.md"]);
  sh(dir, "git", ["commit", "-q", "-m", "init"]);
  if (jj) sh(dir, "jj", ["git", "init", "--colocate"]);
  return dir;
}
const jjChanges = (dir) => sh(dir, "jj", ["status"]).split("\n").filter((line) => /^[AMD] /.test(line));

test("notes in a colocated jj repo: created out of Git, saved by revision, Keep out of Git toggles for git and jj", async (t) => {
  const dir = await repo(t, { jj: true });
  const exclude = path.join(dir, ".git", "info", "exclude");

  // No file yet: an empty editor, created on the first save with the exclude line.
  assert.deepEqual(await read(dir), { text: "", revision: null, git: { state: "included" } });
  const created = await write(dir, null, "first\n");
  assert.deepEqual(created, { outcome: "saved", revision: sha("first\n"), git: { state: "excluded" } });
  assert.match(await readFile(exclude, "utf8"), /^\/RUDDER_NOTES\.md$/m);
  assert.deepEqual(jjChanges(dir), []);
  assert.equal(sh(dir, "git", ["status", "--porcelain"]), "");

  // A save needs the revision it was loaded at: a second device's edit is a conflict, not overwritten.
  assert.deepEqual(await write(dir, sha("first\n"), "from the phone\n"), { outcome: "saved", revision: sha("from the phone\n") });
  const stale = await write(dir, sha("first\n"), "from the laptop\n");
  assert.equal(stale.outcome, "conflict");
  assert.equal(stale.text, "from the phone\n");
  assert.equal(stale.revision, sha("from the phone\n"));
  assert.equal((await write(dir, null, "also new\n")).outcome, "conflict");
  assert.equal(await readFile(path.join(dir, "RUDDER_NOTES.md"), "utf8"), "from the phone\n");

  // Off: the file shows up to commit in Git and jj, and stays tracked by jj after its snapshot.
  assert.deepEqual(await keepOut(dir, false), { git: { state: "included" } });
  assert.doesNotMatch(await readFile(exclude, "utf8"), /RUDDER_NOTES/);
  assert.deepEqual(jjChanges(dir), ["A RUDDER_NOTES.md"]);
  assert.deepEqual((await read(dir)).git, { state: "included" });

  // On again: the exclude line returns and jj lets go of the file, which stays on disk.
  assert.deepEqual(await keepOut(dir, true), { git: { state: "excluded" } });
  assert.deepEqual(jjChanges(dir), []);
  assert.equal(sh(dir, "git", ["status", "--porcelain"]), "");
  assert.equal((await read(dir)).text, "from the phone\n");

  // Committed: reported as it is, and the toggle refuses.
  await keepOut(dir, false);
  sh(dir, "jj", ["commit", "-m", "notes"]);
  assert.deepEqual((await read(dir)).git, { state: "committed" });
  await assert.rejects(keepOut(dir, true), /committed in this repository/);
});

test("notes in a plain Git repo: the repository's own .gitignore wins, a staged file is unstaged by the toggle", async (t) => {
  const dir = await repo(t, { jj: false });
  await writeFile(path.join(dir, ".gitignore"), "RUDDER_NOTES.md\n");
  const created = await write(dir, null, "notes\n");
  assert.deepEqual(created.git, { state: "ignored", by: ".gitignore" });
  assert.doesNotMatch(await readFile(path.join(dir, ".git", "info", "exclude"), "utf8"), /RUDDER_NOTES/);
  await assert.rejects(keepOut(dir, false), /\.gitignore already ignores RUDDER_NOTES\.md/);

  await rm(path.join(dir, ".gitignore"));
  assert.deepEqual((await read(dir)).git, { state: "included" });
  sh(dir, "git", ["add", "RUDDER_NOTES.md"]);
  assert.deepEqual(await keepOut(dir, true), { git: { state: "excluded" } });
  assert.equal(sh(dir, "git", ["status", "--porcelain"]), "");
});

test("the Cloud notes route carries the home's answers, and a sleeping home stays a 503 the app can tell apart", async (t) => {
  const dir = await repo(t, { jj: false });
  let home = (params) => projectNotes(dir, params.request);
  const routes = new ProjectFileRoutes({
    workspace: (_account, workspaceId) => {
      if (workspaceId !== "wsp_app") throw Object.assign(new Error("project not found"), { status: 404 });
      return { homeId: "home_1" };
    },
    ownHome: () => "home_1",
    rpc: async (homeId, method, params) => {
      assert.deepEqual([homeId, method, params.workspaceId], ["home_1", "projectNotes", "wsp_app"]);
      return JSON.parse(JSON.stringify(await home(params)));
    },
  });
  const server = createServer((req, res) => { void routes.handle(req, res, new URL(req.url, "http://cloud"), "account_1"); });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const base = `http://127.0.0.1:${server.address().port}/api/app/project-notes`;
  const call = async (query, body) => {
    const res = await fetch(body ? base : `${base}?${new URLSearchParams(query)}`, body ? { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) } : {});
    return { status: res.status, body: await res.json() };
  };

  // What notes.js sends, end to end.
  assert.deepEqual(await call({ workspaceId: "wsp_app" }), { status: 200, body: { text: "", revision: null, git: { state: "included" } } });
  assert.deepEqual(await call(null, { version: 1, workspaceId: "wsp_app", operation: "write", baseRevision: null, text: "hi\n" }),
    { status: 200, body: { outcome: "saved", revision: sha("hi\n"), git: { state: "excluded" } } });
  assert.deepEqual(await call(null, { version: 1, workspaceId: "wsp_app", operation: "git", keepOut: false }), { status: 200, body: { git: { state: "included" } } });

  // Errors keep their own status: HomeControlSessions answers a home's refusal with 400, homeRpc a sleeping home with 503.
  assert.deepEqual(await call({ workspaceId: "wsp_other" }), { status: 404, body: { error: "project not found" } });
  home = () => { throw Object.assign(new Error("This project is still cloning"), { status: 400 }); };
  assert.deepEqual(await call({ workspaceId: "wsp_app" }), { status: 400, body: { error: "This project is still cloning" } });
  home = () => { throw Object.assign(new Error("the cloud machine is not connected yet; it may still be starting"), { status: 503 }); };
  assert.deepEqual(await call(null, { version: 1, workspaceId: "wsp_app", operation: "write", baseRevision: sha("hi\n"), text: "more\n" }),
    { status: 503, body: { error: "the cloud machine is not connected yet; it may still be starting" } });
});
