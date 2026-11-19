import { constants } from "node:fs";
import { lstat, open, realpath } from "node:fs/promises";
import { createHash } from "node:crypto";
import path from "node:path";
import { FILE_LIMITS, ProjectFileError, ProjectFileService } from "./service.js";
import type { FileDocument, FileScope } from "./types.js";

/** One agent's notes: notes.md in its run folder (.rudder/runs/<id>/) on the
 * home, next to its run.json. Only the user writes them, from the app's Notes
 * editor for that chat. Agents are not told about them and nothing else in
 * Rudder writes them.
 *
 * The run folder is the agent's own state, so the notes stay with the row
 * through a rename, a workspace move or a harness switch (all keep the run
 * id), and go when the agent is deleted (remove_native_run_record removes the
 * folder). `.rudder/` is in every project's local Git exclude, so the notes
 * never show up to commit.
 *
 * Reads and saves go through ProjectFileService, narrowed to this one file:
 * a symlink or anything but a regular file is refused, and a save is swapped
 * in by rename only while the file still holds the revision the editor loaded,
 * so two devices never overwrite each other silently. A missing file reads as
 * empty; the first save creates it.
 */
export const NOTES_FILE = "notes.md";
const SCOPE: FileScope = { projectId: "notes", workspaceId: "notes", attemptId: "notes", ownerEpoch: 0 };
const RUN_ID = /^[A-Za-z0-9._-]{1,256}$/;
const REVISION = /^[a-f0-9]{64}$/;
const CONTROL = /[\x00-\x08\x0b\x0c\x0e-\x1f]/;

export type AgentNotes = { text: string; revision: string | null };
export type NotesRequest =
  | { version: 1; operation: "read" }
  | { version: 1; operation: "write"; baseRevision: string | null; text: string };
/** A conflict carries what is on disk now. */
export type NotesSaved = { outcome: "saved"; revision: string } | ({ outcome: "conflict" } & AgentNotes);

/** The one entry point of the home RPC, for a project directory the home chose. */
export async function agentNotes(projectDir: string, runId: string, request: NotesRequest): Promise<AgentNotes | NotesSaved> {
  if (request?.version !== 1) throw new ProjectFileError("invalid", "Unsupported notes request");
  const root = await runFolder(projectDir, runId);
  if (request.operation === "read") return readNotes(root);
  if (request.operation !== "write") throw new ProjectFileError("invalid", "Unsupported notes request");
  const { baseRevision, text } = request;
  if (!(baseRevision === null || (typeof baseRevision === "string" && REVISION.test(baseRevision))) || typeof text !== "string") {
    throw new ProjectFileError("invalid", "Invalid notes save");
  }
  if (Buffer.byteLength(text) > FILE_LIMITS.writeBytes) throw new ProjectFileError("large", "Notes over 1 MiB cannot be saved here");
  if (CONTROL.test(text)) throw new ProjectFileError("invalid", "The text contains control characters");
  return baseRevision === null ? createNotes(root, text) : saveNotes(root, baseRevision, text);
}

/** The agent's run folder. Notes belong to an agent that exists: its run.json is there. */
async function runFolder(projectDir: string, runId: string): Promise<string> {
  if (typeof runId !== "string" || !RUN_ID.test(runId) || runId === "." || runId === "..") throw new ProjectFileError("invalid", "Not an agent id");
  const dir = path.join(projectDir, ".rudder", "runs", runId);
  try { if ((await lstat(path.join(dir, "run.json"))).isFile()) return await realpath(dir); }
  catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
  throw new ProjectFileError("missing", "That agent is not on your cloud machine");
}

const files = (root: string) => new ProjectFileService(async () => ({ scope: SCOPE, root, access: { kind: "host" }, only: NOTES_FILE }));
const missing = (error: unknown) => error instanceof ProjectFileError && error.code === "missing";

async function readNotes(root: string): Promise<AgentNotes> {
  let doc: FileDocument;
  try { doc = await files(root).dispatch({ version: 1, operation: "read", scope: SCOPE, path: NOTES_FILE }) as FileDocument; }
  catch (error) { if (missing(error)) return { text: "", revision: null }; throw error; }
  if (typeof doc.text !== "string") throw new ProjectFileError("unsupported", `${NOTES_FILE} is not UTF-8 text`);
  return { text: doc.text, revision: doc.revision };
}

async function saveNotes(root: string, baseRevision: string, text: string): Promise<NotesSaved> {
  let result;
  try { result = await files(root).dispatch({ version: 1, operation: "write", scope: SCOPE, path: NOTES_FILE, baseRevision, text }); }
  catch (error) { if (missing(error)) return { outcome: "conflict", ...(await readNotes(root)) }; throw error; }
  if ("outcome" in result && result.outcome === "saved") return { outcome: "saved", revision: result.revision };
  return { outcome: "conflict", ...(await readNotes(root)) };
}

/** The first save. Another device creating the file first is a conflict. */
async function createNotes(root: string, text: string): Promise<NotesSaved> {
  let file;
  try { file = await open(path.join(root, NOTES_FILE), constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o644); }
  catch (error) { if ((error as NodeJS.ErrnoException).code === "EEXIST") return { outcome: "conflict", ...(await readNotes(root)) }; throw error; }
  try { await file.writeFile(text); await file.sync(); } finally { await file.close(); }
  return { outcome: "saved", revision: createHash("sha256").update(text).digest("hex") };
}
