import { execFile, type ExecFileException } from "node:child_process";
import { realpath } from "node:fs/promises";
import path from "node:path";
import { parseEngineStatus } from "../engine.js";
import { resolveNativeBinaryPath } from "../native-binary.js";
import { ProjectFileError, validateFileAccess, validateFileScope, type FileGrant } from "./service.js";
import type { FileScope } from "./types.js";

export function sameFileScope(a: FileScope, b: FileScope): boolean {
  return a.projectId === b.projectId && a.workspaceId === b.workspaceId && a.attemptId === b.attemptId && a.ownerEpoch === b.ownerEpoch;
}

/** Private owner socket, not owner.json or projected runs, supplies live custody.
 * The native App only snapshots bindings. Filesystem IO stays in this helper.
 */
export async function engineFileGrants(repo: string, signal: AbortSignal, nativeBinary = resolveNativeBinaryPath()): Promise<FileGrant[]> {
  const { raw, status } = await queryOwnerSurface(repo, "files", signal, nativeBinary);
  const grants = (JSON.parse(raw) as { file_grants?: unknown }).file_grants;
  if (!Array.isArray(grants) || grants.length > 16) {
    throw new ProjectFileError("denied", "The project owner sent an invalid file grant list");
  }
  const result = grants.map((item: unknown): FileGrant => {
    const grant = item as FileGrant;
    const scope = validateFileScope(grant?.scope);
    if (scope.projectId !== status.owner.project_id || scope.ownerEpoch !== status.owner.epoch
      || typeof grant.root !== "string" || !path.isAbsolute(grant.root) || grant.root.length > 4096) {
      throw new ProjectFileError("denied", "Project file binding changed");
    }
    return { scope, root: grant.root, access: validateFileAccess(grant.access) };
  });
  signal.throwIfAborted();
  return result;
}

/** Queries only the authenticated native owner: the Fabric engine or a
 * dashboard (legacy-native), which answer the same control socket. Responses
 * are bounded and the actual canonical checkout must match before
 * caller-specific data is used. */
export async function queryOwnerSurface(repo: string, operation: "files" | "screen", signal: AbortSignal,
  nativeBinary = resolveNativeBinaryPath(), runId?: string) {
  signal.throwIfAborted();
  if (!nativeBinary) throw new ProjectFileError("denied", "Rudder's native binary is missing from this install");
  const root = await realpath(repo);
  const args = ["engine", operation, "--repo", root, "--json"];
  if (runId !== undefined) args.push("--run", runId);
  const raw = await new Promise<string>((resolve, reject) => {
    execFile(nativeBinary, args, { cwd: root, signal, timeout: 3000, maxBuffer: 128 * 1024 },
      (error, stdout, stderr) => error ? reject(new ProjectFileError("denied", ownerFailure(error, stderr))) : resolve(stdout));
  });
  const status = parseEngineStatus(raw);
  if (!status.running) throw new ProjectFileError("denied", "No dashboard or engine owns this project right now");
  if (status.authority === "legacy-ts") {
    throw new ProjectFileError("denied", "This project's owner is an old Rudder scheduler; open the project in a current dashboard");
  }
  if (await realpath(status.repo) !== root) throw new ProjectFileError("denied", "The project owner names a different checkout");
  const owner = status.owner;
  if (!owner || typeof owner.project_id !== "string" || !owner.project_id || !Number.isSafeInteger(owner.epoch) || owner.epoch < 1) {
    throw new ProjectFileError("denied", "The project owner did not report its identity");
  }
  signal.throwIfAborted();
  return { root, raw, status: { ...status, owner } };
}

/** The native owner's own reason, first line only and bounded. */
function ownerFailure(error: ExecFileException, stderr: string): string {
  if (error.killed || error.name === "AbortError") return "The project owner did not answer in time";
  const line = stderr.split("\n").map((text) => text.trim()).find(Boolean)?.replace(/^Error:\s*/, "");
  return (line || error.message.split("\n")[0]).slice(0, 300);
}
