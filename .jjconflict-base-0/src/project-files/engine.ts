import { execFile } from "node:child_process";
import { realpath } from "node:fs/promises";
import path from "node:path";
import { parseEngineStatus } from "../engine.js";
import { resolveNativeBinaryPath } from "../native-binary.js";
import { ProjectFileError, validateFileScope, type FileGrant } from "./service.js";
import type { FileScope } from "./types.js";

export function sameFileScope(a: FileScope, b: FileScope): boolean {
  return a.projectId === b.projectId && a.workspaceId === b.workspaceId && a.attemptId === b.attemptId && a.ownerEpoch === b.ownerEpoch;
}

/** Private owner socket, not owner.json or projected runs, supplies live custody.
 * The native App only snapshots bindings. Filesystem IO stays in this helper.
 */
export async function engineFileGrants(repo: string, signal: AbortSignal, nativeBinary = resolveNativeBinaryPath()): Promise<FileGrant[]> {
  const { root, raw, status } = await queryOwnerSurface(repo, "files", signal, nativeBinary);
  const grants = (JSON.parse(raw) as { file_grants?: unknown }).file_grants;
  if (!status.running || status.authority !== "fabric" || await realpath(status.repo) !== root
    || !status.owner || !Array.isArray(grants) || grants.length > 16) {
    throw new ProjectFileError("denied", "Project file authority is unavailable");
  }
  const result = grants.map((item: unknown): FileGrant => {
    const grant = item as FileGrant;
    const scope = validateFileScope(grant?.scope);
    if (scope.projectId !== status.owner!.project_id || scope.ownerEpoch !== status.owner!.epoch
      || typeof grant.root !== "string" || !path.isAbsolute(grant.root) || grant.root.length > 4096) {
      throw new ProjectFileError("denied", "Project file binding changed");
    }
    return { scope, root: grant.root };
  });
  signal.throwIfAborted();
  return result;
}

/** Queries only the authenticated native owner. Responses are bounded and the
 * actual canonical checkout must match before caller-specific data is used. */
export async function queryOwnerSurface(repo: string, operation: "files" | "screen", signal: AbortSignal,
  nativeBinary = resolveNativeBinaryPath(), runId?: string) {
  signal.throwIfAborted();
  if (!nativeBinary) throw new ProjectFileError("denied", "Project authority is unavailable");
  const root = await realpath(repo);
  const args = ["engine", operation, "--repo", root, "--json"];
  if (runId !== undefined) args.push("--run", runId);
  const raw = await new Promise<string>((resolve, reject) => {
    execFile(nativeBinary, args, { cwd: root, signal, timeout: 3000, maxBuffer: 128 * 1024 },
      (error, stdout) => error ? reject(new ProjectFileError("denied", "Project authority is unavailable")) : resolve(stdout));
  });
  const status = parseEngineStatus(raw);
  if (!status.running || status.authority !== "fabric" || await realpath(status.repo) !== root || !status.owner) {
    throw new ProjectFileError("denied", "Project authority is unavailable");
  }
  signal.throwIfAborted();
  return { root, raw, status };
}
