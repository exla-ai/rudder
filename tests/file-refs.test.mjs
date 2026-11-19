import test from "node:test";
import assert from "node:assert/strict";
import { mkdir, mkdtemp, realpath, rm, symlink, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { build } from "esbuild";
import { fileRefOfCode, fileRefOfHref } from "../cloud/app/file-refs.js";

async function load(entry) {
  const result = await build({ entryPoints: [new URL(entry, import.meta.url).pathname], bundle: true, write: false, platform: "node", format: "esm", target: "node22" });
  return import(`data:text/javascript;base64,${Buffer.from(result.outputFiles[0].text).toString("base64")}`);
}
const { privateReason, readFileRef, PRIVATE_FILE, SYSTEM_FILE } = await load("../src/project-files/file-refs.ts");
const { FileRefRoutes } = await load("../cloud/src/project-files/file-refs.ts");

const ref = (path, line = 0, col = 0) => ({ path, line, col });

test("inline code is a file reference only when it is exactly a path", () => {
  for (const [code, want] of [
    ["src/foo.ts", ref("src/foo.ts")],
    ["src/foo.ts:42", ref("src/foo.ts", 42)],
    ["src/foo.ts:42:7", ref("src/foo.ts", 42, 7)],
    ["src/foo.ts:42-50", ref("src/foo.ts", 42)],
    ["/home/rudder/.claude/plans/dreamy-launching-firefly.md", ref("/home/rudder/.claude/plans/dreamy-launching-firefly.md")],
    ["~/.claude/plans/plan.md", ref("~/.claude/plans/plan.md")],
    ["./scripts/run.sh", ref("./scripts/run.sh")],
    ["../shared/types.ts", ref("../shared/types.ts")],
    ["README.md", ref("README.md")],
    ["package.json", ref("package.json")],
    ["Dockerfile", ref("Dockerfile")],
    ["cloud/Dockerfile", ref("cloud/Dockerfile")],
    ["app/[id]/page.tsx", ref("app/[id]/page.tsx")],
    ["file:///workspace/rudder-scratch/notes.md", ref("/workspace/rudder-scratch/notes.md")],
  ]) assert.deepEqual(fileRefOfCode(code), want, code);
  for (const code of [
    "console.log", "res.json", "mutex.lock", ".map", "e.target", "v1.2.3", "0.5", "localhost:3000", "12:30",
    "/api/app/file-ref", "text/plain", "origin/main", "@types/node", "src/", "src/**/*.ts", "npm run check",
    "std::fs::read", "https://example.com/a.md", "C:\\code\\a.ts", "~", "..", "src/../a.ts", "a//b.ts", "",
  ]) assert.equal(fileRefOfCode(code), null, code);
});

test("a Markdown link is a file reference when its target is local", () => {
  for (const [href, want] of [
    ["/home/rudder/.claude/plans/plan.md", ref("/home/rudder/.claude/plans/plan.md")],
    ["~/notes/today.md", ref("~/notes/today.md")],
    ["src/foo.ts", ref("src/foo.ts")],
    ["src/foo.ts#L42", ref("src/foo.ts", 42)],
    ["src/foo.ts#L42C3-L50", ref("src/foo.ts", 42, 3)],
    ["src/foo.ts:9", ref("src/foo.ts", 9)],
    ["docs/My%20Plan.md", ref("docs/My Plan.md")],
    ["file:///workspace/code/a.md", ref("/workspace/code/a.md")],
    ["file://localhost/workspace/code/a.md#L3", ref("/workspace/code/a.md", 3)],
  ]) assert.deepEqual(fileRefOfHref(href), want, href);
  for (const href of [
    "https://github.com/a/b", "mailto:a@b.c", "#section", "?q=1", "//cdn.example.com/x.js", "/app/settings", "/api/app/state",
    "file://otherhost/etc/passwd", "file:///", "src/", "~", "",
  ]) assert.equal(fileRefOfHref(href), null, href);
});

test("the deny list keeps keys, tokens and system files out of the viewer", () => {
  const homes = ["/home/rudder", "/workspace/home"];
  for (const file of [
    "/home/rudder/.ssh/id_ed25519", "/home/rudder/.ssh/config", "/home/rudder/.aws/credentials", "/home/rudder/.config/gh/hosts.yml",
    "/home/rudder/.npmrc", "/home/rudder/.netrc", "/home/rudder/.git-credentials", "/home/rudder/.fly-token", "/home/rudder/.claude.json",
    "/home/rudder/.bash_history", "/home/rudder/.local/share/keyrings/login.keyring", "/workspace/home/.config/netlify/config.json",
    "/home/rudder/.claude/.credentials.json", "/workspace/home/.codex/auth.json", "/home/rudder/.claude/api-key.txt",
    "/workspace/code/app/.env", "/workspace/code/app/.env.local", "/workspace/code/app/certs/server.pem", "/workspace/code/app/tls.key",
    "/workspace/code/app/.git/config", "/workspace/code/app/.jj/repo/store", "/workspace/code/app/.rudder/runs/r1/run.json",
    "/workspace/code/app/RUDDER_SHARED.md", "/workspace/code/app/deploy/credentials.json", "/tmp/x/.docker/config.json",
    "/home/rudder/.claude/settings.json", "/workspace/home/.claude/settings.json", "/home/rudder/.claude/settings.local.json",
    "/workspace/code/app/.claude/settings.local.json", "/home/rudder/.codex/config.toml", "/workspace/home/.codex/config.toml",
  ]) assert.equal(privateReason(file, homes), PRIVATE_FILE, file);
  for (const file of ["/proc/self/environ", "/sys/kernel/notes", "/dev/sda", "/run/secrets/token"]) assert.equal(privateReason(file, homes), SYSTEM_FILE, file);
  for (const file of [
    "/home/rudder/.claude/plans/dreamy-launching-firefly.md", "/workspace/home/.claude/projects/p/memory/MEMORY.md",
    "/home/rudder/.codex/AGENTS.md", "/home/rudder/projects/a/README.md", "/workspace/rudder-scratch/file-links/notes.md",
    "/workspace/code/app/src/keyboard.ts", "/workspace/code/app/.github/workflows/ci.yml",
    // Only the home folders' own settings are private: a project's are not.
    "/workspace/code/app/.claude/settings.json", "/workspace/code/app/.vscode/settings.json", "/workspace/code/app/.cargo/config.toml",
  ]) assert.equal(privateReason(file, homes), null, file);
});

/** A home with a project checkout, an agent workspace, a plan and a secret. */
async function machine(t) {
  const root = await realpath(await mkdtemp(path.join(os.tmpdir(), "rudder-file-refs-")));
  t.after(() => rm(root, { recursive: true, force: true }));
  const home = path.join(root, "home"), checkout = path.join(root, "code", "app"), agent = path.join(root, "ws", "agent");
  for (const dir of [path.join(home, ".claude", "plans"), path.join(home, ".ssh"), path.join(checkout, "src"), path.join(agent, "src"), path.join(root, "outside")]) {
    await mkdir(dir, { recursive: true });
  }
  await writeFile(path.join(home, ".claude", "plans", "plan.md"), "# Plan\n");
  await writeFile(path.join(home, ".ssh", "id_ed25519"), "PRIVATE KEY\n");
  await writeFile(path.join(checkout, "src", "both.ts"), "checkout\n");
  await writeFile(path.join(checkout, "src", "only-checkout.ts"), "checkout only\n");
  await writeFile(path.join(agent, "src", "both.ts"), "agent\n");
  await writeFile(path.join(checkout, "image.png"), Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]));
  await writeFile(path.join(checkout, "blob.txt"), Buffer.from([0, 1, 2, 3]));
  await writeFile(path.join(checkout, "big.txt"), "x".repeat(2 * 1024 * 1024 + 1));
  await symlink(path.join(home, ".ssh", "id_ed25519"), path.join(checkout, "innocent.txt"));
  const grant = (workspaceId, dir) => ({ scope: { projectId: "p", workspaceId, attemptId: workspaceId, ownerEpoch: 1 }, root: dir, access: { kind: "host" } });
  const dependencies = {
    grants: async () => [grant("checkout", checkout), grant("run_1", agent)],
    read: async () => { throw new Error("host grants never read through the file service"); },
    policy: { home, homes: [home], roots: [home, path.join(root, "code"), path.join(root, "ws")] },
  };
  const open = (p, extra = {}) => readFileRef({ version: 1, runId: "run_1", path: p, ...extra }, dependencies, AbortSignal.timeout(5000));
  return { root, home, checkout, agent, open };
}

test("a file reference resolves in the agent's workspace, then the checkout, then the home's own folders", async (t) => {
  const m = await machine(t);
  let doc = await m.open("src/both.ts");
  assert.equal(doc.text, "agent\n", "the agent's workspace comes first");
  assert.equal(doc.path, path.join(m.agent, "src", "both.ts"));
  doc = await m.open("src/only-checkout.ts");
  assert.equal(doc.text, "checkout only\n");
  doc = await m.open("~/.claude/plans/plan.md");
  assert.deepEqual([doc.path, doc.kind, doc.text], [path.join(m.home, ".claude", "plans", "plan.md"), "markdown", "# Plan\n"]);
  assert.deepEqual(await m.open(doc.path, { knownRevision: doc.revision }), { path: doc.path, revision: doc.revision, mime: "text/markdown", kind: "markdown", unchanged: true });
  await assert.rejects(m.open("src/missing.ts"), /^ProjectFileError: File not found: src\/missing\.ts$/);
  await assert.rejects(m.open("~/.ssh/id_ed25519"), (e) => e.message === PRIVATE_FILE);
  await assert.rejects(m.open("innocent.txt"), (e) => e.message === PRIVATE_FILE, "a symlink to a private file stays private");
  await assert.rejects(m.open(path.join(m.root, "outside")), /Only files under/);
  await assert.rejects(m.open("image.png"), /Only text files/);
  await assert.rejects(m.open("blob.txt"), /Only text files/);
  await assert.rejects(m.open("big.txt"), /over 2 MB/);
  await assert.rejects(m.open("src"), /That is a folder/);
  await assert.rejects(readFileRef({ version: 1, runId: "../x", path: "a" }, {}, AbortSignal.timeout(1000)), /Invalid file request/);
});

test("only the machine's own account opens files, and an older machine offers Update now", async () => {
  const calls = [];
  const routes = new FileRefRoutes({
    ownHome: (account) => (account === "owner" ? "home_1" : "home_2"),
    workspace: () => ({ homeId: "home_1" }),
    rpc: async (homeId, method, params) => {
      calls.push([homeId, method, params]);
      if (params.request.path === "old") throw Object.assign(new Error("Your cloud machine runs Rudder 5.0.10, which does not have this yet."), { status: 400, needsUpdate: true });
      return { path: "/workspace/code/app/a.md", revision: "a".repeat(64), mime: "text/markdown", kind: "markdown", text: "# A" };
    },
  });
  const get = (account, query) => routes.handle({ method: "GET" }, new URL(`https://x/api/app/file-ref?${new URLSearchParams(query)}`), account, AbortSignal.timeout(1000));
  const query = { workspaceId: "repo_1", runId: "run_1", path: "a.md" };
  assert.deepEqual(await get("owner", query), { path: "/workspace/code/app/a.md", revision: "a".repeat(64), kind: "markdown", mime: "text/markdown", text: "# A" });
  assert.deepEqual(calls, [["home_1", "fileRef", { workspaceId: "repo_1", request: { version: 1, runId: "run_1", path: "a.md" } }]]);
  await assert.rejects(get("teammate", query), (e) => e.status === 403);
  assert.equal(calls.length, 1, "a teammate never reaches the home");
  await assert.rejects(get("owner", { ...query, path: "old" }), (e) => e.status === 400 && e.needsUpdate === true);
});
