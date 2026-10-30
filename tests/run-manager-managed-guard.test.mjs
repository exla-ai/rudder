import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import fs from "node:fs";
import fsp from "node:fs/promises";
import { registerHooks, stripTypeScriptTypes } from "node:module";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const execute = promisify(execFile);
const filename = fileURLToPath(import.meta.url);
const sourceRoot = new URL("../src/", import.meta.url).href;

// Exercise current TS without building dist or running any real Git/jj command.
function loadCurrentSource() {
  registerHooks({
    resolve(specifier, context, next) {
      if (context.parentURL?.startsWith(sourceRoot) && specifier.endsWith(".js")) {
        const candidate = new URL(specifier.slice(0, -3) + ".ts", context.parentURL);
        if (candidate.href.startsWith(sourceRoot) && fs.existsSync(candidate)) {
          return { url: candidate.href, shortCircuit: true };
        }
      }
      return next(specifier, context);
    },
    load(url, context, next) {
      if (url.startsWith(sourceRoot) && url.endsWith(".ts")) {
        return { format: "module", shortCircuit: true,
          source: stripTypeScriptTypes(fs.readFileSync(new URL(url), "utf8"), { mode: "transform", sourceUrl: url }) };
      }
      return next(url, context);
    },
  });
  return import(new URL("../src/run-manager.ts", import.meta.url));
}

async function snapshot(directory) {
  const result = {};
  for (const entry of await fsp.readdir(directory, { withFileTypes: true })) {
    const target = path.join(directory, entry.name);
    result[entry.name] = entry.isDirectory() ? await snapshot(target)
      : entry.isSymbolicLink() ? { link: await fsp.readlink(target) }
      : (await fsp.readFile(target)).toString("base64");
  }
  return result;
}

async function runProbe(mode, directory) {
  const manager = await loadCurrentSource();
  const repo = path.join(directory, "repo");
  process.chdir(repo);
  const runRoot = path.join(repo, ".rudder/runs");
  const audit = path.join(directory, "commands.ndjson");
  const commands = async () => (await fsp.readFile(audit, "utf8").catch(() => "")).trim().split("\n").filter(Boolean).map(JSON.parse);
  async function record(id, extra = {}, sidecar) {
    const workspace = path.join(directory, id + "-workspace");
    const owned = path.join(runRoot, id);
    await fsp.mkdir(path.join(workspace, ".git/info"), { recursive: true });
    await fsp.mkdir(owned, { recursive: true });
    for (const name of ["owned.txt", "RUDDER.md", "RUDDER_SHARED.md", ".gitignore", ".git/info/exclude"]) {
      await fsp.writeFile(path.join(workspace, name), `protected ${name}\n`);
    }
    const value = { id, status: "completed", mode: "execute", backend: "codex", vcs: "git",
      task: "[[FAKE_FILE:owned.txt]]\nwrong worker\n[[/FAKE_FILE]]", taskSummary: "Saved title", taskSummaryLlm: true,
      repoRoot: repo, targetBranch: "main", baseCommit: "old-base", createdAt: "2026-10-30T00:00:00Z", updatedAt: "2026-10-30T00:00:00Z",
      workspace: { enabled: true, path: workspace, branch: "old-branch", workspaceName: "old-workspace", jjChangeId: "old-change" },
      autoSteer: { count: 0, max: 0 }, ...extra };
    if (extra.vcs === undefined && Object.hasOwn(extra, "vcs")) delete value.vcs;
    await fsp.writeFile(path.join(owned, "run.json"), JSON.stringify(value));
    await fsp.writeFile(path.join(owned, "events.ndjson"), "{\"type\":\"backend.output\",\"data\":\"saved\"}\n");
    await fsp.writeFile(path.join(owned, "output.txt"), "saved output\n");
    await fsp.writeFile(path.join(owned, "spawn-stderr.log"), "protected stderr\n");
    if (sidecar === "directory") await fsp.mkdir(path.join(owned, "managed-local.json"));
    else if (sidecar === "symlink") await fsp.symlink("missing-reservation", path.join(owned, "managed-local.json"));
    else if (sidecar !== undefined) await fsp.writeFile(path.join(owned, "managed-local.json"), sidecar);
    return { value, workspace, owned };
  }

  if (mode === "blocked") {
    const cases = [
      ...[null, false, 0, "", [], {}, { phase: "assigned", assignment: {} }, { phase: "quarantined" }]
        .map((managedLocal) => ({ extra: { managedLocal } })),
      ...["fabric", "future-vcs", false, null, {}].map((vcs) => ({ extra: { vcs } })),
      { extra: { managedLocal: { phase: "reserved" }, status: "merged" } },
      { extra: { vcs: "jj" }, sidecar: "{malformed" },
      { extra: { vcs: undefined }, sidecar: "" },
      { extra: { vcs: "git" }, sidecar: "directory" },
      { extra: { vcs: "jj" }, sidecar: "symlink" },
    ];
    for (const [index, item] of cases.entries()) {
      const fixture = await record(`protected-${index}`, item.extra, item.sidecar);
      const savedWorkspace = await snapshot(fixture.workspace);
      const savedRecord = await snapshot(fixture.owned);
      const id = fixture.value.id;
      const calls = [
        () => manager.mergeRun(id, true, { silent: true }),
        () => manager.syncRun(id, { silent: true }),
        () => manager.deleteRun(id, { force: true, silent: true }),
        () => manager.deleteRun(id, { force: true, mergeFirst: true, silent: true }),
        () => manager.continueRun({ runId: id, prompt: "resume", interrupt: true, silent: true }),
        () => manager.stopRun(id, { silent: true }),
        () => manager.workerRun(repo, id, "stale-attempt"),
        () => manager.spawnWorker(repo, id, "stale-attempt"),
      ];
      for (const call of calls) {
        await assert.rejects(async () => call(), /managed or unsupported workspace recovery state/, id);
        assert.deepEqual(await snapshot(fixture.workspace), savedWorkspace, id);
        assert.deepEqual(await snapshot(fixture.owned), savedRecord, id);
      }
    }
    await manager.cleanupRuns(true);
    await manager.cleanupRuns(false);
    await manager.writeAgentContext(repo);
    const rows = await commands();
    assert.equal(rows.filter((row) => row.mutation).length, 0);
    assert.ok(rows.every((row) => row.cwd === repo), "context must not probe managed workspace Git metadata");
    for (let index = 0; index < cases.length; index++) {
      const id = `protected-${index}`;
      assert.equal(await fsp.readFile(path.join(directory, id + "-workspace/owned.txt"), "utf8"), "protected owned.txt\n");
      assert.equal(await fsp.readFile(path.join(directory, id + "-workspace/RUDDER.md"), "utf8"), "protected RUDDER.md\n");
      assert.ok(await fsp.stat(path.join(runRoot, id, "run.json")));
    }
  } else if (mode === "context") {
    const fixture = await record("managed", { managedLocal: null });
    const saved = await snapshot(fixture.workspace);
    const alias = path.join(directory, "legacy-alias");
    await fsp.symlink(fixture.workspace, alias);
    const legacy = await record("legacy-alias", { vcs: "git" });
    legacy.value.workspace.path = alias;
    await fsp.writeFile(path.join(legacy.owned, "run.json"), JSON.stringify(legacy.value));
    await manager.writeAgentContext(repo);
    await manager.statusRuns({ json: true });
    await manager.listProjectRuns({ json: true });
    await manager.watchRun({ repoRoot: repo, runId: fixture.value.id, follow: false });
    await manager.printLogs(fixture.value.id);
    assert.deepEqual(await snapshot(fixture.workspace), saved);
    assert.ok((await commands()).every((row) => row.cwd === repo && !row.mutation));

    // Only the saved JSON points at the filesystem root. All fixture writes
    // and snapshots stay inside the disposable directory.
    const rootFixture = await record("managed-filesystem-root", { managedLocal: null });
    rootFixture.value.workspace.path = path.parse(repo).root;
    await fsp.writeFile(path.join(rootFixture.owned, "run.json"), JSON.stringify(rootFixture.value));
    const savedRepo = await snapshot(repo);
    const savedCommands = await commands();
    await manager.writeAgentContext(repo);
    assert.deepEqual(await snapshot(repo), savedRepo, "root protection must also cover its disposable descendants");
    assert.deepEqual(await commands(), savedCommands, "root protection must return before any Git probe");
    assert.deepEqual(await snapshot(fixture.workspace), saved);
  } else {
    for (const [index, vcs] of [undefined, "git", "jj"].entries()) {
      const fixture = await record(`legacy-${index}`, { vcs });
      await manager.deleteRun(fixture.value.id, { force: true, silent: true });
      await assert.rejects(fsp.stat(fixture.owned), { code: "ENOENT" });
    }
    const rows = await commands();
    assert.equal(rows.filter((r) => r.tool === "git" && r.args[0] === "worktree" && r.args[1] === "remove").length, 2);
    assert.equal(rows.filter((r) => r.tool === "jj" && r.args[0] === "workspace" && r.args[1] === "forget").length, 1);
    const stoppable = await record("legacy-stop", { vcs: "git" });
    await manager.stopRun(stoppable.value.id, { silent: true });
    assert.equal(JSON.parse(await fsp.readFile(path.join(stoppable.owned, "run.json"))).status, "cancelled");
  }
}

if (process.argv[2] === "--managed-guard-probe") {
  await runProbe(process.argv[3], process.argv[4]);
} else if (process.argv[2] === "__worker") {
  fs.appendFileSync(process.env.RUDDER_GUARD_AUDIT, JSON.stringify({ tool: "worker", mutation: true }) + "\n");
} else {
  for (const [mode, description] of [
    ["blocked", "managed and quarantined records reject every legacy mutation before touching protected paths"],
    ["context", "readonly views and context propagation protect managed aliases and filesystem roots"],
    ["legacy", "valid Git, jj and pre-VCS records keep their existing delete and stop behavior"],
  ]) {
    test(description, { skip: process.platform === "win32", timeout: 30_000 }, async (t) => {
      const directory = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-managed-guard-"));
      t.after(() => fsp.rm(directory, { recursive: true, force: true }));
      const repo = path.join(directory, "repo");
      const bin = path.join(directory, "bin");
      await fsp.mkdir(repo); await fsp.mkdir(bin);
      const tripwire = `#!${process.execPath}\nconst fs=require('node:fs'),path=require('node:path');const args=process.argv.slice(2),tool=path.basename(process.argv[1]);const lookup=tool==='git'&&args[0]==='rev-parse';fs.appendFileSync(process.env.RUDDER_GUARD_AUDIT,JSON.stringify({tool,args,cwd:process.cwd(),mutation:!lookup})+'\\n');if(lookup)console.log(args[1]==='--show-toplevel'?process.env.RUDDER_GUARD_REPO:'.git/info/exclude');\n`;
      for (const tool of ["git", "jj"]) await fsp.writeFile(path.join(bin, tool), tripwire, { mode: 0o755 });
      await execute(process.execPath, [filename, "--managed-guard-probe", mode, directory], {
        cwd: repo, timeout: 25_000, maxBuffer: 2 * 1024 * 1024,
        env: { ...process.env, PATH: `${bin}${path.delimiter}${process.env.PATH ?? ""}`,
          RUDDER_HOME: path.join(directory, "home"), RUDDER_GUARD_REPO: repo,
          RUDDER_GUARD_AUDIT: path.join(directory, "commands.ndjson"), RUDDER_FAKE_BACKEND: "1",
          RUDDER_DISABLE_AGENT_SKILLS: "1", RUDDER_DISABLE_UPDATE_CHECK: "1", RUDDER_DISABLE_AUTO_UPDATE: "1" },
      });
    });
  }
}
