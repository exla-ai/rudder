import fsp from "node:fs/promises";
import path from "node:path";
import { createHash } from "node:crypto";
import { createRecoveryBundle, exportRecoveryBundle, legacyRecoveryRecord, verifyRecoveryBundle, type RecoveryRoot, type RecoveryManifest } from "./recovery.js";
import { writeJson } from "../util.js";
import type { JsonValue } from "../types.js";

async function exists(file: string): Promise<boolean> {
  try { await fsp.lstat(file); return true; } catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return false; throw error; }
}
/** Capture one stopped project. The host must hold the native MigrationSession
 * owner lock during inventory and capture. Session files are explicit because
 * backends store transcripts outside the project; never capture a HOME tree.
 * Missing known workspaces/sessions fail rather than silently lose recovery.
 */
export async function captureLegacyProject(options: {
  repository: string; bundle: string; projectId: string; migrationId: string;
  sessions: Array<{ runId: string; files: string[] }>; rollbackCheckpoint?: string;
}): Promise<RecoveryManifest> {
  const repo = await fsp.realpath(options.repository);
  const roots: RecoveryRoot[] = [{ name: "integration-source", path: repo, role: "source", exclude: [".git", ".jj", ".rudder", ".rudder-workspaces", ".rudder-worktrees"] }];
  for (const name of [".git", ".jj"]) {
    const file = path.join(repo, name);
    if (!(await exists(file))) throw new Error("Migration needs an ordinary recoverable jj 0.40.0 integration checkout");
    if (!(await fsp.lstat(file)).isDirectory()) throw new Error("Integration history must be local directories; export linked history before migration");
    roots.push({ name: name === ".git" ? "git-history" : "jj-history", path: file, role: "history" });
  }
  const runStore = path.join(repo, ".rudder", "runs");
  if (await exists(runStore)) {
    if (!(await fsp.lstat(runStore)).isDirectory()) throw new Error("Cannot capture a linked run store");
    roots.push({ name: "legacy-runs", path: runStore, role: "legacy-state" });
    const capturedPaths = new Set<string>([repo]);
    const sessions = new Map(options.sessions.map(s => [s.runId, s.files]));
    if (sessions.size !== options.sessions.length) throw new Error("Duplicate session inventory");
    let index = 0;
    for (const entry of (await fsp.readdir(runStore, { withFileTypes: true })).sort((a, b) => a.name.localeCompare(b.name))) {
      if (entry.isSymbolicLink()) throw new Error("Cannot capture a linked run record");
      if (!entry.isDirectory()) continue;
      const recordPath = path.join(runStore, entry.name, "run.json");
      if (!(await fsp.lstat(recordPath)).isFile()) throw new Error("Cannot capture a linked run record");
      const raw = JSON.parse(await fsp.readFile(recordPath, "utf8"));
      const record = legacyRecoveryRecord(raw);
      const workspace = record.workspace as { enabled?: boolean; path?: string } | undefined;
      if (workspace?.enabled && workspace.path) {
        const real = await fsp.realpath(workspace.path);
        if (!capturedPaths.has(real)) {
          capturedPaths.add(real);
          roots.push({ name: `workspace-${index++}`, path: real, role: "source", exclude: [".rudder"] });
        }
      }
      const session = record.session as { nativeSessionId?: string } | undefined;
      if (session?.nativeSessionId && !sessions.get(String(record.id))?.length) throw new Error(`Supply transcript files for legacy session ${record.id}`);
    }
  }
  const undo = path.join(repo, ".rudder", "undo-stack.json");
  if (await exists(undo)) {
    if (!(await fsp.lstat(undo)).isFile()) throw new Error("Cannot capture linked undo history");
    roots.push({ name: "legacy-undo", path: undo, role: "legacy-state" });
  }
  let transcript = 0;
  for (const session of options.sessions) for (const file of session.files) {
    if (!(await fsp.lstat(file)).isFile()) throw new Error("Session transcript must be an ordinary file");
    roots.push({ name: `session-${transcript++}`, path: file, role: "sessions", run_id: session.runId });
  }
  return createRecoveryBundle(options.bundle, options.projectId, options.migrationId, roots, options.rollbackCheckpoint);
}

/** Export a compatibility view beside the untouched raw records. Explicit user
 * resume through a compatible owner is required; stale process/PTY references
 * cannot restart work. Raw run/event/session files remain in the bundle.
 */
export async function exportLegacyRunProjections(exportedRuns: string, destination: string): Promise<void> {
  await fsp.mkdir(destination, { mode: 0o700 });
  for (const entry of await fsp.readdir(exportedRuns, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const raw = JSON.parse(await fsp.readFile(path.join(exportedRuns, entry.name, "run.json"), "utf8"));
    const record = legacyRecoveryRecord(raw);
    await writeJson(path.join(destination, `${entry.name}.json`), record as JsonValue, { mode: 0o600 });
  }
}

/** Cooperative refusal only; incompatible old clients require physical shutdown.
 * Call from TS writer admission alongside assertLegacyWriterAllowed. Native
 * engine_host uses v4_migration::refuse_implicit_startup at the same boundary.
 */
export async function assertNoPendingV4Migration(repository: string): Promise<void> {
  for (const name of ["v4-migration.json", "v4-recovery.json"]) {
    if (await exists(path.join(repository, ".rudder", "engine", name))) throw new Error("Project migration requires explicit recovery with its recorded owner and run epochs before a scheduler can start");
  }
}

/** Reassemble an ordinary integration checkout with its retained .git/.jj data,
 * raw legacy state, workspace source trees and transcripts. No agent starts.
 * Historical absolute paths in raw records stay intact; compatibility adapters
 * use the returned root inventory to select the exported paths explicitly.
 */
export async function exportLegacyProjectRecovery(bundle: string, destination: string): Promise<{ integrationPath: string; roots: RecoveryRoot[]; compatibleRuns?: string }> {
  const manifest = await exportRecoveryBundle(bundle, destination);
  const integrationPath = path.join(destination, "integration-source");
  for (const [root, name] of [["git-history", ".git"], ["jj-history", ".jj"]]) {
    if (!manifest.roots.some(r => r.name === root)) throw new Error("Recovery has no complete integration history");
    if (await exists(path.join(integrationPath, name))) throw new Error("Recovery source already contains history metadata");
    await fsp.rename(path.join(destination, root), path.join(integrationPath, name));
  }
  let compatibleRuns: string | undefined;
  if (manifest.roots.some(r => r.name === "legacy-runs")) {
    compatibleRuns = path.join(destination, "compatible-records");
    await exportLegacyRunProjections(path.join(destination, "legacy-runs"), compatibleRuns);
  }
  return { integrationPath, roots: manifest.roots.map(root => ({ ...root, path: path.join(destination, root.name === "git-history" ? "integration-source/.git" : root.name === "jj-history" ? "integration-source/.jj" : root.name) })), compatibleRuns };
}

/** Explicit compatible recovery handoff into a NEW checkout. Raw originals stay
 * beside the checkout, while only paused projections become its runnable store.
 * This opens no engine and starts no session. Workspace and transcript locations
 * are remapped through the verified exported root inventory, never guessed.
 */
export async function prepareLegacyProjectRecovery(options: {
  bundle: string; destination: string; projectId: string; migrationId: string;
  rollbackCheckpoint?: string;
}): Promise<{ integrationPath: string; roots: RecoveryRoot[]; compatibleRuns?: string }> {
  const manifest = await verifyRecoveryBundle(options.bundle);
  if (manifest.project_id !== options.projectId || manifest.migration_id !== options.migrationId
    || manifest.rollback_checkpoint !== options.rollbackCheckpoint) throw new Error("Recovery selection differs from its checkpoint");
  const exported = await exportLegacyProjectRecovery(options.bundle, options.destination);
  // Refusal evidence, never a live owner grant. A fresh native Fabric owner
  // must retain its checkout lock, verify this archive and import these paused
  // records before any explicit resume. Interrupted adaptation stays refused.
  const recoveryFile = path.join(exported.integrationPath,".rudder","engine","v4-recovery.json");
  const recovery = { format_version: 1, repository: await fsp.realpath(exported.integrationPath),
    source_project_id: options.projectId, migration_id: options.migrationId,
    bundle: await fsp.realpath(options.bundle),
    recovery_manifest_sha256: createHash("sha256").update(await fsp.readFile(path.join(options.bundle,"manifest.json"))).digest("hex"),
    rollback_checkpoint: options.rollbackCheckpoint ?? null, run_ids: [] as string[], ready: false };
  await writeJson(recoveryFile, recovery, {mode:0o600});
  const mappings = manifest.roots.map((root, index) => ({ before: path.resolve(root.path), after: exported.roots[index].path }))
    .sort((a,b) => b.before.length - a.before.length);
  const remap = (value: string): string => {
    const input = path.resolve(value);
    const mapping = mappings.find(root => input === root.before || input.startsWith(root.before + path.sep));
    if (!mapping) throw new Error("Recovery workspace has no captured source root");
    return path.join(mapping.after, path.relative(mapping.before, input));
  };
  if (exported.compatibleRuns) {
    const files = await fsp.readdir(exported.compatibleRuns);
    if (files.length > 1024) throw new Error("Recovery run inventory exceeds bound");
    for (const file of files) {
      if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,255}\.json$/.test(file)) throw new Error("Invalid compatible recovery run identity");
      const record = legacyRecoveryRecord(JSON.parse(await fsp.readFile(path.join(exported.compatibleRuns,file), "utf8")));
      const id = file.slice(0,-5);
      if (record.id !== id) throw new Error("Recovery run identity differs");
      const workspace = record.workspace as {enabled?: boolean; path?: string} | undefined;
      if (workspace?.enabled && workspace.path) record.workspace = {...workspace, path: remap(workspace.path)};
      record.repoRoot = exported.integrationPath;
      record.migrationRecovery = { ...(record.migrationRecovery as object),
        source_project_id: options.projectId, source_migration_id: options.migrationId,
        session_files: exported.roots.filter(root => root.role === "sessions" && root.run_id === id).map(root=>root.path) };
      await writeJson(path.join(exported.integrationPath,".rudder","runs",id,"run.json"), record as JsonValue, {mode:0o600});
      recovery.run_ids.push(id);
    }
  }
  recovery.ready = true;
  await writeJson(recoveryFile, recovery, {mode:0o600});
  return exported;
}
