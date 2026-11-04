import { pathToFileURL } from "node:url";
import { openBoardBrowser, requestBoardBootstrap } from "../board/host-access.js";
import { projectFileLink } from "./references.js";
import { validateFilePath, validateFileScope } from "./service.js";
import type { ProjectFileReference } from "./types.js";

/** Host-only action. Obtain the existing board's one-use ticket over its private
 * Unix socket, keep it in the fragment, and invoke the local browser. Neither
 * the browser URL nor the ticket is printed or sent over a new listener.
 */
export async function openProjectFile(repoRoot: string, reference: ProjectFileReference, dependencies = {
  bootstrap: requestBoardBootstrap,
  open: openBoardBrowser,
}): Promise<void> {
  const scope = validateFileScope(reference.scope);
  const relative = validateFilePath(reference.path);
  if (reference.kind !== undefined && !["file", "folder"].includes(reference.kind)) throw new Error("Invalid task file kind");
  const url = new URL(await dependencies.bootstrap(repoRoot, "project"));
  const target = new URL(projectFileLink(url.pathname, { scope, path: relative, kind: reference.kind }), url.origin);
  target.hash = url.hash;
  await dependencies.open(target.href);
}

/** Native callers launch this TS helper on a background thread, then return a
 * typed success/failure to the App loop. Arguments contain no credentials.
 */
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const [repoRoot, raw] = process.argv.slice(2);
    if (!repoRoot || !raw || raw.length > 8192) throw new Error("Invalid project file action");
    await openProjectFile(repoRoot, JSON.parse(raw) as ProjectFileReference);
  } catch {
    process.stderr.write("Project files could not be opened. Open this project's Rudder board and retry.\n");
    process.exitCode = 1;
  }
}
