import { engineFileGrants } from "./engine.js";
import { openProjectFile } from "./open.js";
import { taskFileReference } from "./references.js";

/** Host action using a fresh owner binding. Browser reads authorize it again. */
export async function runProjectFilesCommand(repo: string, args: readonly string[], dependencies = {
  grants: engineFileGrants, open: openProjectFile,
}): Promise<void> {
  if (args.length > 2) throw new Error("Usage: rudder project files [workspace-id] [relative-path]");
  const grants = await dependencies.grants(repo, AbortSignal.timeout(3000));
  const matches = args[0] ? grants.filter(grant => grant.scope.workspaceId === args[0]) : grants;
  if (matches.length !== 1) {
    throw new Error(grants.length === 0 ? "No current task workspace is available."
      : `Choose a current workspace: ${grants.map(grant => grant.scope.workspaceId).join(", ")}`);
  }
  const grant = matches[0]!;
  const supplied = args[1] ?? "";
  const folder = supplied === "" || supplied.endsWith("/");
  const reference = taskFileReference(grant.scope, folder ? supplied.replace(/\/$/, "") : supplied,
    grant.root, folder ? "folder" : "file");
  await dependencies.open(repo, reference);
}
