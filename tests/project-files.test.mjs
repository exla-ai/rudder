import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, symlink, rm, truncate, rename, realpath } from "node:fs/promises";
import fs from "node:fs";
import os from "node:os";
import { build } from "esbuild";
import path from "node:path";
import http from "node:http";

// Compile only this feature, independently of shared dist output used by other workers.
async function load(entry) {
  const result = await build({ entryPoints: [new URL(entry, import.meta.url).pathname], bundle: true, write: false, platform: "node", format: "esm", target: "node22" });
  return import(`data:text/javascript;base64,${Buffer.from(result.outputFiles[0].text).toString("base64")}`);
}
const { ProjectFileService, FILE_LIMITS, validateFilePath } = await load("../src/project-files/service.ts");
const { taskFileReference, projectFileLink } = await load("../src/project-files/references.ts");
const { openProjectFile } = await load("../src/project-files/open.ts");
const { handleProjectFiles, parseProjectFileQuery } = await load("../src/project-files/http.ts");
const { CloudProjectFiles } = await load("../cloud/src/project-files/relay.ts");
const scope = { projectId: "project-a", workspaceId: "workspace-a", attemptId: "attempt-a", ownerEpoch: 9 };
const req = (operation, relative, identity = scope) => ({ version: 1, operation, path: relative, scope: identity });
function query(operation = "read", relative = "README.md", identity = scope) {
  return new URL(`http://127.0.0.1/api/project-files?${new URLSearchParams({ version: "1", operation, ...identity, path: relative })}`);
}
async function fixture(t, authorize) {
  const base = await mkdtemp(path.join(fs.existsSync("/workspace") ? "/workspace" : await realpath(os.tmpdir()), "project-files-test-"));
  const root = path.join(base, "project");
  await mkdir(root);
  await mkdir(path.join(root, "docs"));
  await writeFile(path.join(root, "README.md"), "# Project\n\nOriginal document\n");
  await writeFile(path.join(root, "docs", "index.html"), '<h1>Site</h1><script>parent.fetch("/api/control")</script>');
  await writeFile(path.join(root, "docs", "style.css"), "body { color: red }");
  await writeFile(path.join(root, "image.png"), Buffer.from("89504e470d0a1a0a00000000", "hex"));
  await writeFile(path.join(root, "document.pdf"), "%PDF-1.7\nfixture, not a valid rendered PDF");
  await writeFile(path.join(root, "binary.bin"), Buffer.from([0, 255, 2, 3]));
  await writeFile(path.join(base, "outside.txt"), "outside-private-data");
  const service = new ProjectFileService(async (identity, signal) => {
    if (authorize) return authorize(identity, root, signal);
    assert.deepEqual(identity, scope);
    return { scope, root };
  });
  t.after(() => rm(base, { recursive: true, force: true }));
  return { base, root, service };
}

test("project tree, task file references and nearby folders use explicit execution identity", async (t) => {
  const { root, service } = await fixture(t);
  const listing = await service.dispatch(req("list", ""));
  assert.equal(listing.entries[0].path, "docs");
  const reference = taskFileReference(scope, path.join(root, "docs/index.html"), root);
  assert.deepEqual(reference, { scope, path: "docs/index.html" });
  assert.equal((await service.dispatch(req("list", "docs"))).entries.length, 2);
  const link = projectFileLink("/rudder/project-a", reference);
  assert.equal(new URL(link, "http://localhost").searchParams.get("attemptId"), scope.attemptId);
  assert.ok(!link.includes(root));
  const folder = taskFileReference(scope, path.join(root, "docs"), root, "folder");
  assert.equal(new URL(projectFileLink("/rudder/project-a", folder), "http://localhost").searchParams.get("fileKind"), "folder");
  assert.throws(() => taskFileReference(scope, path.join(root, "../outside.txt"), root));
});

test("host open action preserves the one-use board fragment and opens a scoped folder", async () => {
  let opened;
  await openProjectFile("/trusted/project", { scope, path: "docs", kind: "folder" }, {
    bootstrap: async (root, target) => {
      assert.equal(root, "/trusted/project");
      assert.equal(target, "project");
      return "http://127.0.0.1:4774/rudder/project-a#rudder-board-ticket=private-ticket-fixture";
    },
    open: async (url) => { opened = new URL(url); },
  });
  assert.equal(opened.hash, "#rudder-board-ticket=private-ticket-fixture");
  assert.equal(opened.searchParams.get("fileKind"), "folder");
  assert.equal(opened.searchParams.get("workspaceId"), scope.workspaceId);
  assert.equal(opened.searchParams.has("token"), false);
});

test("Markdown, HTML and CSS remain source data, media is bounded and MIME checked", async (t) => {
  const { root, service } = await fixture(t);
  const markdown = await service.dispatch(req("read", "README.md"));
  assert.equal(markdown.kind, "markdown");
  assert.match(markdown.revision, /^[a-f0-9]{64}$/);
  assert.match(markdown.text, /Original/);
  assert.equal((await service.dispatch(req("read", "docs/index.html"))).kind, "html");
  assert.equal((await service.dispatch(req("read", "docs/style.css"))).mime, "text/css");
  assert.equal((await service.dispatch(req("read", "image.png"))).mime, "image/png");
  assert.equal((await service.dispatch(req("read", "document.pdf"))).kind, "pdf");
  assert.equal((await service.dispatch(req("read", "binary.bin"))).text, undefined);
  await writeFile(path.join(root, "fake.png"), "<svg onload='alert(1)'>");
  await assert.rejects(service.dispatch(req("read", "fake.png")), { code: "unsupported" });
});

test("saved changes refresh content and revision; removed files report missing", async (t) => {
  const { root, service } = await fixture(t);
  const original = await service.dispatch(req("read", "README.md"));
  await writeFile(path.join(root, "README.md"), "# Saved update");
  const updated = await service.dispatch(req("read", "README.md"));
  assert.notEqual(original.revision, updated.revision);
  assert.equal(updated.text, "# Saved update");
  await rm(path.join(root, "README.md"));
  await assert.rejects(service.dispatch(req("read", "README.md")), { code: "missing" });
});

test("traversal, absolute paths, platform variants and private infrastructure fail closed", async (t) => {
  const { service } = await fixture(t);
  for (const relative of ["../outside.txt", "docs/../../outside.txt", "/etc/passwd", "C:/private", "docs\\..\\outside.txt", "docs//index.html", "./README.md", "README.md\0", ".git/config", ".GIT/config", ".jj/repo/store", ".env", ".env.local", ".rudder-workspaces/other/data", "RUDDER_SHARED.md", "private.key"]) {
    await assert.rejects(service.dispatch(req("read", relative)), (e) => ["denied", "invalid"].includes(e.code), relative);
  }
  assert.throws(() => validateFilePath("a/../b"));
});

test("symlink files and ancestor escapes never expose a neighboring project", async (t) => {
  const { base, root, service } = await fixture(t);
  await symlink(path.join(base, "outside.txt"), path.join(root, "outside-link"));
  await symlink(base, path.join(root, "outside-dir"));
  await assert.rejects(service.dispatch(req("read", "outside-link")), { code: "denied" });
  await assert.rejects(service.dispatch(req("read", "outside-dir/outside.txt")), (e) => ["denied", "missing"].includes(e.code));
  assert.ok(!(await service.dispatch(req("list", ""))).entries.some((e) => e.name.startsWith("outside-")));
});

test("stale epoch, cross-project and cross-attempt grants cannot read even a matching path", async (t) => {
  const { service } = await fixture(t, async (_identity, root) => ({ scope, root }));
  for (const identity of [{ ...scope, ownerEpoch: 8 }, { ...scope, projectId: "other" }, { ...scope, workspaceId: "other" }, { ...scope, attemptId: "other" }]) {
    await assert.rejects(service.dispatch(req("read", "README.md", identity)), { code: "denied" });
  }
});

test("ownership and account access are checked again before publishing a document", async (t) => {
  let calls = 0;
  const { service } = await fixture(t, async (_identity, root) => ({ scope: ++calls === 1 ? scope : { ...scope, ownerEpoch: 10 }, root }));
  await assert.rejects(service.dispatch(req("read", "README.md")), { code: "denied" });
  assert.equal(calls, 2);
});

test("delete, overwrite and atomic replacement during a read are rejected", async (t) => {
  for (const mutation of [async (root) => rm(path.join(root, "README.md")), async (root) => writeFile(path.join(root, "README.md"), "changed"), async (root) => {
    await writeFile(path.join(root, "new.md"), "replacement");
    await rename(path.join(root, "new.md"), path.join(root, "README.md"));
  }]) {
    let calls = 0;
    const { service } = await fixture(t, async (_identity, root) => {
      if (++calls === 2) await mutation(root);
      return { scope, root };
    });
    await assert.rejects(service.dispatch(req("read", "README.md")), (e) => ["changed", "missing"].includes(e.code));
  }
});

test("huge files and directories are bounded without reading entire payloads", async (t) => {
  const { root, service } = await fixture(t);
  await writeFile(path.join(root, "huge.txt"), "");
  await truncate(path.join(root, "huge.txt"), FILE_LIMITS.textBytes + 1);
  await assert.rejects(service.dispatch(req("read", "huge.txt")), { code: "large" });
  await mkdir(path.join(root, "many"));
  await Promise.all(Array.from({ length: FILE_LIMITS.entries + 2 }, (_, i) => writeFile(path.join(root, "many", `${i}.txt`), "x")));
  const listing = await service.dispatch(req("list", "many"));
  assert.equal(listing.entries.length, FILE_LIMITS.entries);
  assert.equal(listing.truncated, true);
});

test("in-flight capacity and cancellation bound even an unresponsive authorizer", async (t) => {
  const { service } = await fixture(t, async () => new Promise(() => {}));
  const controllers = Array.from({ length: FILE_LIMITS.active }, () => new AbortController());
  const pending = controllers.map((c) => service.dispatch(req("read", "README.md"), c.signal));
  await assert.rejects(service.dispatch(req("read", "README.md")), { code: "busy" });
  controllers.forEach((c) => c.abort());
  const results = await Promise.allSettled(pending);
  assert.ok(results.every((r) => r.status === "rejected"));
});

test("HTTP adapter serves only JSON with no-store/nosniff and rejects ambiguous query identity", async (t) => {
  const { service } = await fixture(t);
  const server = http.createServer((request, response) => { void handleProjectFiles(request, response, new URL(request.url, "http://localhost"), service); });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const address = server.address();
  const url = query("read", "docs/index.html");
  const response = await fetch(`http://127.0.0.1:${address.port}${url.pathname}${url.search}`);
  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type"), /^application\/json/);
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.equal(response.headers.get("x-content-type-options"), "nosniff");
  assert.match((await response.json()).text, /<script>/);
  url.searchParams.append("workspaceId", "other");
  assert.throws(() => parseProjectFileQuery(url));
});

test("private Cloud relay checks account/project/owner both sides of exact home RPC", async (t) => {
  const { service } = await fixture(t);
  const calls = [];
  const relay = new CloudProjectFiles({ authorize: async (actor, identity) => {
    calls.push(["authorize", actor]);
    if (actor !== "account-a") throw new Error("forbidden");
    return { scope: identity, homeId: "home-a" };
  }, rpc: async (homeId, operation, request, signal) => {
    calls.push([homeId, operation, request.scope]);
    return service.dispatch(request, signal);
  } });
  assert.match((await relay.query("account-a", query())).text, /Original/);
  assert.equal(calls.length, 3);
  assert.equal(calls[1][0], "home-a");
  await assert.rejects(relay.query("other-account", query()));
});

test("Cloud revocation, stale ownership and moved home never publish a response", async () => {
  for (const change of [{ scope: { ...scope, ownerEpoch: 10 }, homeId: "home-a" }, { scope, homeId: "home-other" }]) {
    let checks = 0;
    const relay = new CloudProjectFiles({ authorize: async () => ++checks === 1 ? { scope, homeId: "home-a" } : change,
      rpc: async () => ({ path: "README.md", revision: "a".repeat(64), kind: "markdown", mime: "text/markdown", text: "private" }) });
    await assert.rejects(relay.query("account-a", query()), { status: 403 });
  }
});

test("Cloud relay rejects malformed or oversized replies and supports aborting hung RPC", async () => {
  const dependencies = { authorize: async () => ({ scope, homeId: "home-a" }) };
  for (const result of [{ path: "other" }, { path: "README.md", revision: "bad" }, { path: "README.md", revision: "a".repeat(64), kind: "text", mime: "text/plain", text: "x".repeat(FILE_LIMITS.textBytes + 1) }]) {
    const relay = new CloudProjectFiles({ ...dependencies, rpc: async () => result });
    await assert.rejects(relay.query("account-a", query()), { status: 502 });
  }
  const relay = new CloudProjectFiles({ ...dependencies, rpc: async () => new Promise(() => {}) });
  const controller = new AbortController();
  const pending = relay.query("account-a", query(), controller.signal);
  controller.abort();
  await assert.rejects(pending);
});

test("Cloud relay filters reply metadata and rejects out-of-folder entries", async () => {
  const authorize = async () => ({ scope, homeId: "home-a" });
  const valid = { path: "README.md", revision: "a".repeat(64), kind: "markdown", mime: "text/markdown", text: "# Read me" };
  const relay = new CloudProjectFiles({ authorize, rpc: async () => ({ ...valid, privateRoot: "/not-for-clients" }) });
  assert.deepEqual(await relay.query("account-a", query()), valid);
  const wrongFolder = new CloudProjectFiles({ authorize, rpc: async () => ({ path: "docs", truncated: false,
    entries: [{ name: "secret", path: "other/secret", kind: "file" }] }) });
  await assert.rejects(wrongFolder.query("account-a", query("list", "docs")), { status: 502 });
});

test("changing the authoritative root alias during authorization invalidates the read", async (t) => {
  let calls = 0;
  let alias;
  const { base, root, service } = await fixture(t, async () => {
    if (++calls === 2) {
      await rm(alias);
      await symlink(base, alias);
    }
    return { scope, root: alias };
  });
  alias = path.join(base, "active-project");
  await symlink(root, alias);
  await assert.rejects(service.dispatch(req("read", "README.md")), { code: "denied" });
});
