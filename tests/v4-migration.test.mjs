import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import path from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { v4ScratchPrefix } from './helpers/v4-scratch.mjs';
import {
  captureLegacyProject, exportLegacyProjectRecovery, assertNoPendingV4Migration,
  createRecoveryBundle, verifyRecoveryBundle, exportRecoveryBundle, legacyRecoveryRecord,
} from "../dist/migration.js";

async function fixture(t) {
  const root = await fsp.mkdtemp(v4ScratchPrefix('migration'));
  t.after(() => fsp.rm(root, { recursive: true, force: true }));
  const repo = path.join(root, "repo");
  await fsp.mkdir(repo);
  const run = (...args) => {
    const result = spawnSync("jj", ["--ignore-working-copy", "--repository", repo, ...args], { encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
    return result.stdout;
  };
  const init = spawnSync("jj", ["git", "init", "--colocate", repo], { encoding: "utf8" });
  assert.equal(init.status, 0, init.stderr);
  const version = spawnSync("jj", ["--version"], { encoding: "utf8" }).stdout;
  assert.match(version, /^jj 0\.40\.0/);
  await fsp.writeFile(path.join(repo, "user edits ü.txt"), "source edits\n");
  await fsp.mkdir(path.join(repo, "empty"));
  await fsp.symlink("user edits ü.txt", path.join(repo, "source-link"));
  await fsp.writeFile(path.join(repo, "executable"), "#!/bin/sh\nexit 0\n", { mode: 0o755 });
  const workspace = path.join(root, "legacy-workspace");
  await fsp.mkdir(workspace);
  await fsp.writeFile(path.join(workspace, "pending.txt"), "stopped work is recoverable\n");
  const runs = path.join(repo, ".rudder", "runs");
  await fsp.mkdir(path.join(runs, "old-run"), { recursive: true });
  const record = {
    id: "old-run", status: "running", backend: "codex", vcs: "jj", task: "preserve edits",
    worktree: { enabled: true, path: workspace, jjChangeId: "opaque-old-change" },
    session: { nativeSessionId: "session-1" }, process: { pid: 1234 }, terminal: { kind: "tmux" },
    merge: { operationId: "old-op", mergeChangeId: "old-merge" },
    delivery: { status: "pending", target: "preserve delivery fact" }, futureField: { opaque: true },
  };
  await fsp.writeFile(path.join(runs, "old-run", "run.json"), JSON.stringify(record));
  await fsp.writeFile(path.join(runs, "old-run", "events.ndjson"), '{"event":"session"}\n');
  await fsp.writeFile(path.join(repo, ".rudder", "undo-stack.json"), '[{"opId":"old-op"}]');
  const transcript = path.join(root, "session-1.jsonl");
  await fsp.writeFile(transcript, '{"session_id":"session-1","text":"retained conversation"}\n');
  return { root, repo, workspace, runs, transcript, record, run, bundle: path.join(root, "bundle") };
}

test("upgrade checkpoint exports real jj 0.40 history, source and sessions without daemon", async t => {
  const f = await fixture(t);
  const operation = f.run("op", "log", "--no-graph", "--limit", "1", "--template", "id");
  const manifest = await captureLegacyProject({ repository: f.repo, bundle: f.bundle, projectId: "project", migrationId: "migration", sessions: [{ runId: "old-run", files: [f.transcript] }] });
  assert.equal(manifest.jj_version, "0.40.0");
  assert.equal(manifest.roots.find(r => r.role === "sessions").run_id, "old-run");
  assert.equal((await fsp.stat(f.bundle)).mode & 0o777, 0o700);
  // Remove every original ordinary source/session/run path. Retrieval uses only
  // the complete bundle, with no daemon socket or private mount available.
  await fsp.rm(f.repo, { recursive: true });
  await fsp.rm(f.workspace, { recursive: true });
  await fsp.rm(f.transcript);
  const destination = path.join(f.root, "exported");
  const exported = await exportLegacyProjectRecovery(f.bundle, destination);
  const source = exported.integrationPath;
  assert.equal(await fsp.readFile(path.join(source, "user edits ü.txt"), "utf8"), "source edits\n");
  assert.equal(await fsp.readlink(path.join(source, "source-link")), "user edits ü.txt");
  assert.equal((await fsp.stat(path.join(source, "executable"))).mode & 0o777, 0o755);
  const history = spawnSync("jj", ["--ignore-working-copy", "--repository", source, "op", "log", "--no-graph", "--limit", "1", "--template", "id"], { encoding: "utf8" });
  assert.equal(history.status, 0, history.stderr);
  assert.equal(history.stdout, operation);
  const status = spawnSync("jj", ["--repository", source, "status"], { encoding: "utf8" });
  assert.equal(status.status, 0, status.stderr);
  assert.match(status.stdout, /user edits ü.txt/);
  assert.equal(await fsp.readFile(path.join(destination, "workspace-0", "pending.txt"), "utf8"), "stopped work is recoverable\n");
  assert.match(await fsp.readFile(path.join(destination, "session-0"), "utf8"), /retained conversation/);
  const raw = JSON.parse(await fsp.readFile(path.join(destination, "legacy-runs", "old-run", "run.json"), "utf8"));
  assert.deepEqual(raw, f.record);
  assert.equal(exported.compatibleRuns, path.join(destination, "compatible-records"));
  const recovered = JSON.parse(await fsp.readFile(path.join(destination, "compatible-records", "old-run.json"), "utf8"));
  assert.equal(recovered.status, "paused");
  assert.equal(recovered.process, undefined);
  assert.equal(recovered.terminal, undefined);
  assert.deepEqual(recovered.workspace, f.record.worktree);
  assert.deepEqual(recovered.session, f.record.session);
  assert.deepEqual(recovered.merge, f.record.merge);
  assert.deepEqual(recovered.delivery, f.record.delivery);
  assert.deepEqual(recovered.futureField, { opaque: true });
});

test("legacy record compatibility preserves terminal states and never resumes stopped runs", () => {
  for (const status of ["paused", "cancelled", "failed", "completed", "merged", "merge-conflict", "migrated"]) {
    const record = { id: status, status, process: { pid: 5 }, opaque: 9 };
    const projected = legacyRecoveryRecord(record);
    assert.equal(projected.status, status);
    assert.equal(projected.process, undefined);
    assert.equal(projected.migrationRecovery.automatic_resume, false);
    assert.equal(record.process.pid, 5);
  }
  for (const status of ["created", "running", "steering", "verifying", "orphaned"]) assert.equal(legacyRecoveryRecord({ id: status, status }).status, "paused");
});

test("missing known sessions or workspace never produce a complete checkpoint", async t => {
  const f = await fixture(t);
  await assert.rejects(captureLegacyProject({ repository: f.repo, bundle: f.bundle, projectId: "project", migrationId: "migration", sessions: [] }), /Supply transcript files/);
  await assert.rejects(fsp.access(path.join(f.bundle, "manifest.json")));
  await fsp.rm(f.workspace, { recursive: true });
  await assert.rejects(captureLegacyProject({ repository: f.repo, bundle: f.bundle, projectId: "project", migrationId: "migration", sessions: [{ runId: "old-run", files: [f.transcript] }] }));
});

test("corrupt or missing blobs prevent any offline export", async t => {
  const f = await fixture(t);
  const manifest = await createRecoveryBundle(f.bundle, "project", "migration", [{ name: "session", path: f.transcript, role: "sessions" }]);
  const object = path.join(f.bundle, "objects", manifest.entries[0].object.digest);
  await fsp.writeFile(object, "corrupted");
  const target = path.join(f.root, "exported");
  await assert.rejects(exportRecoveryBundle(f.bundle, target), /checksum/);
  await assert.rejects(fsp.access(target));
  await fsp.rm(object);
  await assert.rejects(verifyRecoveryBundle(f.bundle));
});

test("interrupted capture is not a checkpoint; an existing destination is preserved", async t => {
  const f = await fixture(t);
  await fsp.mkdir(f.bundle);
  await fsp.mkdir(path.join(f.bundle, "objects"));
  await assert.rejects(verifyRecoveryBundle(f.bundle));
  await fsp.rm(f.bundle, { recursive: true });
  await createRecoveryBundle(f.bundle, "project", "migration", [{ name: "session", path: f.transcript, role: "sessions" }]);
  const destination = path.join(f.root, "user-checkout");
  await fsp.mkdir(destination);
  await fsp.writeFile(path.join(destination, "keep"), "user edits");
  await assert.rejects(exportRecoveryBundle(f.bundle, destination), /EEXIST/);
  assert.equal(await fsp.readFile(path.join(destination, "keep"), "utf8"), "user edits");
});

test("bundle traversal through a symlink and malformed entry paths are rejected", async t => {
  const f = await fixture(t);
  const manifest = await createRecoveryBundle(f.bundle, "project", "migration", [{ name: "workspace", path: f.workspace, role: "source" }]);
  const manifestPath = path.join(f.bundle, "manifest.json");
  manifest.entries[1].path = "../outside";
  await fsp.writeFile(manifestPath, JSON.stringify(manifest));
  await assert.rejects(verifyRecoveryBundle(f.bundle), /Invalid recovery entry/);
  manifest.entries[1].path = "linked/child";
  await fsp.writeFile(manifestPath, JSON.stringify(manifest));
  await assert.rejects(verifyRecoveryBundle(f.bundle), /symlink/);
});

test("cooperative legacy writer startup refuses migration markers, even unreadable ones", async t => {
  const f = await fixture(t);
  await assertNoPendingV4Migration(f.repo);
  const engine = path.join(f.repo, ".rudder", "engine");
  await fsp.mkdir(engine);
  await fsp.writeFile(path.join(engine, "v4-migration.json"), "interrupted write");
  await assert.rejects(assertNoPendingV4Migration(f.repo), /explicit recovery/);
});

test("capture refuses recursive output and linked integration history", async t => {
  const f = await fixture(t);
  await assert.rejects(createRecoveryBundle(path.join(f.repo, "bundle"), "project", "migration", [{ name: "source", path: f.repo, role: "source" }]), /outside captured roots/);
  await fsp.rename(path.join(f.repo, ".git"), path.join(f.root, "external-git"));
  await fsp.symlink(path.join(f.root, "external-git"), path.join(f.repo, ".git"));
  await assert.rejects(captureLegacyProject({ repository: f.repo, bundle: f.bundle, projectId: "project", migrationId: "migration", sessions: [] }), /Integration history/);
});


test("post-pause recovery bundles retain the exact opaque rollback checkpoint", async t => {
  const f = await fixture(t);
  const checkpoint = "12345678-1234-4123-8123-123456789abc";
  await createRecoveryBundle(f.bundle, "project", "migration", [{ name: "source", path: f.workspace, role: "source" }], checkpoint);
  assert.equal((await verifyRecoveryBundle(f.bundle)).rollback_checkpoint, checkpoint);
  await assert.rejects(createRecoveryBundle(path.join(f.root, "wrong"), "project", "migration", [{ name: "source", path: f.workspace, role: "source" }], "stale-or-malformed"), /Invalid rollback checkpoint/);
});
