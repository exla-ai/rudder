import path from "node:path";
import { validateFilePath, validateFileScope } from "./service.js";
import type { FileScope, ProjectFileReference } from "./types.js";

/** Bind structured task output to the task's trusted execution identity. Do not
 * infer ownership from arbitrary terminal text, a URL, or a matching basename.
 */
export function taskFileReference(scope: FileScope, outputPath: string, trustedRoot?: string, kind?: "file" | "folder"): ProjectFileReference {
  const identity = validateFileScope(scope);
  let relative = outputPath;
  if (path.isAbsolute(outputPath)) {
    if (!trustedRoot || !path.isAbsolute(trustedRoot)) throw new Error("Task output requires a trusted workspace root");
    relative = path.relative(trustedRoot, outputPath);
  }
  return { scope: identity, path: validateFilePath(relative), ...(kind ? { kind } : {}) };
}

/** The link contains identity and a relative path, never a local absolute root
 * or bearer credential. The board must authorize the target again when opened.
 */
export function projectFileLink(boardProjectPath: string, reference: ProjectFileReference): string {
  if (!/^\/rudder\/[^/?#]+$/.test(boardProjectPath)) throw new Error("Use an existing board project route");
  const scope = validateFileScope(reference.scope);
  const query = new URLSearchParams({ file: validateFilePath(reference.path), projectId: scope.projectId,
    workspaceId: scope.workspaceId, attemptId: scope.attemptId, ownerEpoch: String(scope.ownerEpoch) });
  if (reference.kind) query.set("fileKind", reference.kind);
  return `${boardProjectPath}?${query}`;
}
