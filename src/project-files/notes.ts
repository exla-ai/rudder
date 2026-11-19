import { constants } from "node:fs";
import { appendFile, lstat, mkdir, open, readFile, realpath, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { homedir } from "node:os";
import path from "node:path";
import { runCommand } from "../util.js";
import { FILE_LIMITS, ProjectFileError, ProjectFileService } from "./service.js";
import type { FileDocument, FileScope } from "./types.js";

/** The user's notes for a project: RUDDER_NOTES.md at the root of the
 * checkout on the home. Only the user writes it, from the app's Notes editor.
 * Agents are not told about it and nothing else in Rudder writes it.
 *
 * Reads and saves go through ProjectFileService, narrowed to this one file:
 * a symlink or anything but a regular file is refused, and a save is swapped
 * in by rename only while the file still holds the revision the editor loaded,
 * so two devices never overwrite each other silently. A missing file reads as
 * empty; the first save creates it.
 *
 * Git: a new notes file is kept out of Git by a line in the repository's
 * local exclude (.git/info/exclude), never the tracked .gitignore. The line
 * goes in before the file exists, so no jj snapshot in between picks it up;
 * jj honours that file for untracked paths. "Keep out of Git" off removes the
 * line, and the file shows up to commit like any other (jj tracks it at its
 * next snapshot). Back on, the line returns and a file that is not committed
 * yet is untracked again: an ignore rule alone never untracks a path jj
 * already tracks. A committed file, or one the repository's own ignore rules
 * cover, is reported as it is and left alone.
 */
export const NOTES_FILE = "RUDDER_NOTES.md";
const EXCLUDE_LINE = `/${NOTES_FILE}`;
const SCOPE: FileScope = { projectId: "notes", workspaceId: "notes", attemptId: "notes", ownerEpoch: 0 };
const REVISION = /^[a-f0-9]{64}$/;
const CONTROL = /[\x00-\x08\x0b\x0c\x0e-\x1f]/;

/** excluded: our exclude line keeps it out. included: it shows up in the
 * repository like any other file. committed: it is in HEAD. ignored: the
 * repository's own ignore rules (`by`) cover it. */
export type NotesGit = { state: "excluded" | "included" | "committed" } | { state: "ignored"; by: string };
export type ProjectNotes = { text: string; revision: string | null; git: NotesGit };
export type NotesRequest =
  | { version: 1; operation: "read" }
  | { version: 1; operation: "write"; baseRevision: string | null; text: string }
  | { version: 1; operation: "git"; keepOut: boolean };
/** `git` comes back when the save created the file. A conflict carries what is on disk now. */
export type NotesSaved = { outcome: "saved"; revision: string; git?: NotesGit } | ({ outcome: "conflict" } & ProjectNotes);
export type NotesReply = ProjectNotes | NotesSaved | { git: NotesGit };

/** The one entry point of the home RPC, for a project directory the home chose. */
export async function projectNotes(dir: string, request: NotesRequest): Promise<NotesReply> {
  if (request?.version !== 1) throw new ProjectFileError("invalid", "Unsupported notes request");
  const root = await realpath(dir);
  switch (request.operation) {
    case "read": return readNotes(root);
    case "write": {
      const { baseRevision, text } = request;
      if (!(baseRevision === null || (typeof baseRevision === "string" && REVISION.test(baseRevision))) || typeof text !== "string") {
        throw new ProjectFileError("invalid", "Invalid notes save");
      }
      if (Buffer.byteLength(text) > FILE_LIMITS.writeBytes) throw new ProjectFileError("large", "Notes over 1 MiB cannot be saved here");
      if (CONTROL.test(text)) throw new ProjectFileError("invalid", "The text contains control characters");
      return baseRevision === null ? createNotes(root, text) : saveNotes(root, baseRevision, text);
    }
    case "git":
      if (typeof request.keepOut !== "boolean") throw new ProjectFileError("invalid", "Invalid notes request");
      return { git: await keepOutOfGit(root, request.keepOut) };
    default: throw new ProjectFileError("invalid", "Unsupported notes request");
  }
}

const files = (root: string) => new ProjectFileService(async () => ({ scope: SCOPE, root, access: { kind: "host" }, only: NOTES_FILE }));
const missing = (error: unknown) => error instanceof ProjectFileError && error.code === "missing";

async function readNotes(root: string): Promise<ProjectNotes> {
  const git = await gitState(root);
  let doc: FileDocument;
  try { doc = await files(root).dispatch({ version: 1, operation: "read", scope: SCOPE, path: NOTES_FILE }) as FileDocument; }
  catch (error) { if (missing(error)) return { text: "", revision: null, git }; throw error; }
  if (typeof doc.text !== "string") throw new ProjectFileError("unsupported", `${NOTES_FILE} is not UTF-8 text`);
  return { text: doc.text, revision: doc.revision, git };
}

async function saveNotes(root: string, baseRevision: string, text: string): Promise<NotesSaved> {
  let result;
  try { result = await files(root).dispatch({ version: 1, operation: "write", scope: SCOPE, path: NOTES_FILE, baseRevision, text }); }
  catch (error) { if (missing(error)) return { outcome: "conflict", ...(await readNotes(root)) }; throw error; }
  if ("outcome" in result && result.outcome === "saved") return { outcome: "saved", revision: result.revision };
  return { outcome: "conflict", ...(await readNotes(root)) };
}

/** The first save. Out of Git by default: the exclude line is written before
 * the file exists. Another device creating it first is a conflict. */
async function createNotes(root: string, text: string): Promise<NotesSaved> {
  if ((await gitState(root)).state === "included") await setExcludeLine(root, true);
  let file;
  try { file = await open(path.join(root, NOTES_FILE), constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o644); }
  catch (error) { if ((error as NodeJS.ErrnoException).code === "EEXIST") return { outcome: "conflict", ...(await readNotes(root)) }; throw error; }
  try { await file.writeFile(text); await file.sync(); } finally { await file.close(); }
  return { outcome: "saved", revision: createHash("sha256").update(text).digest("hex"), git: await gitState(root) };
}

async function keepOutOfGit(root: string, keepOut: boolean): Promise<NotesGit> {
  const now = await gitState(root);
  if (now.state === "committed") throw new ProjectFileError("invalid", `${NOTES_FILE} is committed in this repository, so Git keeps it either way`);
  if (now.state === "ignored") throw new ProjectFileError("invalid", `${now.by} already ignores ${NOTES_FILE}. Change it there.`);
  await setExcludeLine(root, keepOut);
  if (keepOut && now.state === "included") await untrack(root);
  return gitState(root);
}

const git = (root: string, args: string[]) => runCommand("git", args, { cwd: root, allowFailure: true });
async function gitOut(root: string, args: string[]): Promise<string> {
  const result = await git(root, args);
  if (result.code !== 0) throw new Error(`git ${args[0]} failed in ${root}: ${result.stderr.trim()}`);
  return result.stdout;
}

/** The repository's local exclude file. Every home project is a Git repository. */
async function excludeFile(root: string): Promise<string> {
  return path.resolve(root, (await gitOut(root, ["rev-parse", "--git-path", "info/exclude"])).trim());
}

/** Where RUDDER_NOTES.md stands. In a colocated jj repository Git's index
 * follows jj's working copy (files jj tracks in @ are intent-to-add there). */
export async function gitState(root: string): Promise<NotesGit> {
  const exclude = await excludeFile(root);
  const head = await git(root, ["rev-parse", "-q", "--verify", "HEAD^{commit}"]);
  if (head.code === 0 && (await gitOut(root, ["ls-tree", "--name-only", "HEAD", "--", NOTES_FILE])).trim()) return { state: "committed" };
  // Tracked but not committed yet: staged in Git, or tracked by jj in @.
  if ((await gitOut(root, ["ls-files", "--", NOTES_FILE])).trim()) return { state: "included" };
  const rule = await git(root, ["check-ignore", "-v", "--no-index", "--", NOTES_FILE]);
  if (rule.code === 1) return { state: "included" };
  if (rule.code !== 0) throw new Error(`git check-ignore failed in ${root}: ${rule.stderr.trim()}`);
  // "<source>:<line>:<pattern>\t<path>". A negated pattern means not ignored.
  const match = /^(.+?):(\d+):(.*)\t/.exec(rule.stdout);
  if (!match) throw new Error(`git check-ignore printed an unexpected line in ${root}: ${rule.stdout.trim()}`);
  const [, source, , pattern] = match;
  if (pattern.startsWith("!")) return { state: "included" };
  if (path.resolve(root, source) === exclude && pattern === EXCLUDE_LINE) return { state: "excluded" };
  return { state: "ignored", by: shownSource(root, source) };
}

function shownSource(root: string, source: string): string {
  if (source.startsWith("~")) return source;
  const at = path.resolve(root, source);
  if (at.startsWith(`${root}/`)) return path.relative(root, at);
  const home = homedir();
  return at.startsWith(`${home}/`) ? `~/${path.relative(home, at)}` : at;
}

async function setExcludeLine(root: string, present: boolean): Promise<void> {
  const file = await excludeFile(root);
  let text = "";
  try { text = await readFile(file, "utf8"); }
  catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
  const lines = text.split("\n");
  if (lines.some((line) => line.trim() === EXCLUDE_LINE) === present) return;
  if (present) {
    await mkdir(path.dirname(file), { recursive: true });
    await appendFile(file, `${text && !text.endsWith("\n") ? "\n" : ""}${EXCLUDE_LINE}\n`);
  } else {
    await writeFile(file, lines.filter((line) => line.trim() !== EXCLUDE_LINE).join("\n"));
  }
}

/** A notes file that is not committed but still tracked: jj tracks it in @
 * once a snapshot saw it, and only `jj file untrack` lets go (it needs the
 * exclude line in place first). Without jj, Git's index holds it only when
 * someone staged it. */
async function untrack(root: string): Promise<void> {
  let jj = false;
  try { jj = (await lstat(path.join(root, ".jj"))).isDirectory(); }
  catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
  if (jj) {
    const tracked = await runCommand("jj", ["file", "list", "--ignore-working-copy", "-r", "@", "--", NOTES_FILE], { cwd: root });
    if (tracked.stdout.trim()) await runCommand("jj", ["file", "untrack", "--", NOTES_FILE], { cwd: root });
    return;
  }
  if ((await gitOut(root, ["ls-files", "--", NOTES_FILE])).trim()) await gitOut(root, ["rm", "--cached", "-q", "--", NOTES_FILE]);
}
