/** Private, offline recovery bundles. These are not Cloud session-transfer
 * manifests, engine authority records, or runnable CAS roots. No daemon needed.
 */
import fsp from "node:fs/promises";
import path from "node:path";
import { createHash, randomUUID } from "node:crypto";
import { writeJson } from "../util.js";
import type { JsonValue } from "../types.js";
import type { FabricObjectRef } from "../fabric-remote.js";

export type RecoveryRoot = { name: string; path: string; role: "source" | "history" | "sessions" | "legacy-state"; exclude?: string[]; run_id?: string };
type Entry = { root: string; path: string; mode: number; type: "directory" | "file" | "symlink"; object?: FabricObjectRef };
export type RecoveryManifest = {
  format_version: 1; kind: "v4_legacy_recovery"; jj_version: "0.40.0";
  project_id: string; migration_id: string; created_at: string;
  roots: RecoveryRoot[]; entries: Entry[]; rollback_checkpoint?: string;
};
const digest = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");
const component = (s: string) => /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(s) && s !== "." && s !== "..";
const relative = (s: string) => s === "" || (!s.includes("\\") && !s.includes("\0") && s.split("/").every(part => part.length > 0 && part !== "." && part !== ".."));
const json = (value: unknown) => value as JsonValue;
async function sync(file: string): Promise<void> {
  const handle = await fsp.open(file, "r");
  try { await handle.sync(); } finally { await handle.close(); }
}

/** All roots must be quiescent under the caller's retained owner lock. Capture
 * ordinary source plus complete .git/.jj history explicitly. HOME credentials
 * are never discovered or copied. The directory itself is a retention pin:
 * do not remove it until import and the rollback retention window finish.
 */
export async function createRecoveryBundle(bundle: string, projectId: string, migrationId: string, roots: RecoveryRoot[], rollbackCheckpoint?: string): Promise<RecoveryManifest> {
  if (!projectId || !migrationId || !roots.length || new Set(roots.map(r => r.name)).size !== roots.length || roots.some(r => !component(r.name) || r.exclude?.some(omit => !omit || !relative(omit)))) throw new Error("Invalid recovery identity or roots");
  if (rollbackCheckpoint !== undefined && !/^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(rollbackCheckpoint)) throw new Error("Invalid rollback checkpoint");
  const output = path.resolve(bundle);
  for (const root of roots) {
    const input = await fsp.realpath(root.path);
    // Refuse recursive capture, including a bundle parent reached through links.
    const parent = await fsp.realpath(path.dirname(output));
    const resolvedOutput = path.join(parent, path.basename(output));
    if (resolvedOutput === input || resolvedOutput.startsWith(input + path.sep)) throw new Error("Recovery bundle must be outside captured roots");
  }
  await fsp.mkdir(output, { mode: 0o700 });
  await fsp.mkdir(path.join(output, "objects"), { mode: 0o700 });
  const manifest: RecoveryManifest = { format_version: 1, kind: "v4_legacy_recovery", jj_version: "0.40.0", project_id: projectId, migration_id: migrationId, created_at: new Date().toISOString(), roots: roots.map(r => ({ ...r, path: path.resolve(r.path) })), entries: [] };
  if (rollbackCheckpoint !== undefined) manifest.rollback_checkpoint = rollbackCheckpoint;
  async function visit(root: RecoveryRoot, rel: string): Promise<void> {
    if (root.exclude?.some(omit => rel === omit || rel.startsWith(omit + path.sep))) return;
    const file = path.join(root.path, rel);
    const stat = await fsp.lstat(file);
    const entry: Entry = { root: root.name, path: rel.split(path.sep).join("/"), mode: stat.mode & 0o777, type: stat.isDirectory() ? "directory" : stat.isSymbolicLink() ? "symlink" : "file" };
    if (!relative(entry.path)) throw new Error("Unsupported recovery filename");
    if (stat.isDirectory()) {
      manifest.entries.push(entry);
      for (const child of (await fsp.readdir(file)).sort()) await visit(root, path.join(rel, child));
    } else {
      if (!stat.isFile() && !stat.isSymbolicLink()) throw new Error("Recovery cannot capture sockets, devices or pipes; quiesce the owner first");
      const bytes = stat.isSymbolicLink() ? Buffer.from(await fsp.readlink(file)) : await fsp.readFile(file);
      entry.object = { kind: "blob", algorithm: "sha256", digest: digest(bytes), size_bytes: bytes.length };
      const objectPath = path.join(output, "objects", entry.object.digest);
      await fsp.writeFile(objectPath, bytes, { mode: 0o600 });
      await sync(objectPath);
      manifest.entries.push(entry);
    }
  }
  // An interrupted capture has no manifest and can never be used as a checkpoint.
  for (const root of roots) await visit(root, "");
  await sync(path.join(output, "objects"));
  await writeJson(path.join(output, "manifest.json"), json(manifest), { mode: 0o600 });
  await sync(path.join(output, "manifest.json"));
  await sync(output);
  await sync(path.dirname(output));
  return manifest;
}

export async function verifyRecoveryBundle(bundle: string): Promise<RecoveryManifest> {
  const manifest = JSON.parse(await fsp.readFile(path.join(bundle, "manifest.json"), "utf8")) as RecoveryManifest;
  if (manifest?.format_version !== 1 || manifest.kind !== "v4_legacy_recovery" || manifest.jj_version !== "0.40.0" || !manifest.project_id || !manifest.migration_id || !Array.isArray(manifest.roots) || !Array.isArray(manifest.entries)) throw new Error("Unsupported recovery manifest");
  if (manifest.rollback_checkpoint !== undefined && !/^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(manifest.rollback_checkpoint)) throw new Error("Invalid rollback checkpoint");
  const roots = new Set(manifest.roots.map(r => r.name));
  if (!roots.size || roots.size !== manifest.roots.length || [...roots].some(r => !component(r))) throw new Error("Invalid recovery roots");
  const seen = new Map<string, Entry>();
  for (const entry of manifest.entries) {
    const key = `${entry.root}/${entry.path}`;
    if (!roots.has(entry.root) || !relative(entry.path) || seen.has(key) || !Number.isInteger(entry.mode) || entry.mode < 0 || entry.mode > 0o777 || !["directory", "file", "symlink"].includes(entry.type)) throw new Error("Invalid recovery entry");
    if (entry.path) {
      const parent = path.posix.dirname(entry.path);
      if (seen.get(`${entry.root}/${parent === "." ? "" : parent}`)?.type !== "directory") throw new Error("Recovery entry traverses a missing directory or symlink");
    }
    seen.set(key, entry);
    if (entry.type === "directory") { if (entry.object) throw new Error("Unexpected directory object"); continue; }
    const ref = entry.object;
    if (!ref || ref.kind !== "blob" || ref.algorithm !== "sha256" || !/^[a-f0-9]{64}$/.test(ref.digest) || !Number.isSafeInteger(ref.size_bytes) || ref.size_bytes < 0) throw new Error("Invalid recovery object");
    const objectPath = path.join(bundle, "objects", ref.digest);
    if (!(await fsp.lstat(objectPath)).isFile()) throw new Error("Recovery object is not an ordinary file");
    const bytes = await fsp.readFile(objectPath);
    if (bytes.length !== ref.size_bytes || digest(bytes) !== ref.digest) throw new Error("Recovery object checksum mismatch");
  }
  if ([...roots].some(root => !seen.has(`${root}/`))) throw new Error("Missing recovery root");
  return manifest;
}

/** Produces ordinary files in a NEW directory. It never overlays a checkout,
 * changes legacy run records, opens SQLite, starts an agent, or needs a daemon.
 */
export async function exportRecoveryBundle(bundle: string, destination: string): Promise<RecoveryManifest> {
  const manifest = await verifyRecoveryBundle(bundle);
  const staging = `${path.resolve(destination)}.${randomUUID()}.recovering`;
  // Reserve destination first. Never rename over even an existing empty folder.
  await fsp.mkdir(destination, { mode: 0o700 });
  await fsp.mkdir(staging, { mode: 0o700 });
  try {
    for (const entry of manifest.entries) {
      const file = path.join(staging, entry.root, entry.path);
      if (entry.type === "directory") await fsp.mkdir(file, { mode: 0o700 });
      else {
        const bytes = await fsp.readFile(path.join(bundle, "objects", entry.object!.digest));
        // Recheck after the verification pass, before trusting mutable storage.
        if (digest(bytes) !== entry.object!.digest || bytes.length !== entry.object!.size_bytes) throw new Error("Recovery object changed during export");
        if (entry.type === "symlink") await fsp.symlink(bytes.toString(), file);
        else { await fsp.writeFile(file, bytes, { mode: entry.mode }); await fsp.chmod(file, entry.mode); }
      }
    }
    // Restrictive directory permissions during materialization, then original modes.
    for (const entry of [...manifest.entries].reverse()) if (entry.type === "directory") await fsp.chmod(path.join(staging, entry.root, entry.path), entry.mode);
    await fsp.rename(staging, destination);
  } catch (error) {
    await fsp.rm(staging, { recursive: true, force: true });
    await fsp.rmdir(destination).catch(() => undefined);
    throw error;
  }
  return manifest;
}

/** Compatibility projection only. Preserve opaque old fields, sessions, merge
 * receipts, original status and paths. Interrupted work becomes paused; terminal
 * work keeps its delivery facts. This does not write to the original store.
 */
export function legacyRecoveryRecord(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Invalid legacy run record");
  const raw = value as Record<string, unknown>;
  if (typeof raw.id !== "string" || typeof raw.status !== "string") throw new Error("Legacy record has no identity or status");
  const result = { ...raw };
  if (!result.workspace && result.worktree) result.workspace = result.worktree;
  delete result.process;
  delete result.terminal;
  if (["created", "running", "steering", "verifying", "orphaned"].includes(raw.status)) result.status = "paused";
  const previous = raw.migrationRecovery && typeof raw.migrationRecovery === "object" && !Array.isArray(raw.migrationRecovery)
    ? raw.migrationRecovery as Record<string, unknown> : {};
  result.migrationRecovery = { ...previous, format_version: 1, original_status: previous.original_status ?? raw.status, automatic_resume: false };
  return result;
}
