import { constants } from "node:fs";
import { lstat, open, realpath } from "node:fs/promises";
import type { FileHandle } from "node:fs/promises";
import path from "node:path";
import { classification, FILE_LIMITS, mediaDocument, ProjectFileError, revisionOf, textOf, type FileGrant } from "./service.js";
import type { FileDocument, FileScope } from "./types.js";

/** A file a chat message names (cloud/app/file-refs.js): a Markdown link or
 * inline code holding a path, opened read only beside the chat. Only the
 * home's own account asks (cloud/src/project-files/file-refs.ts).
 *
 * A relative path is the agent's: it resolves against the agent's live
 * workspace first, then the project checkout. A file inside a Linux overlay
 * fork is read through the file service, which sees the agent's own mount.
 * Any other file comes from the home's filesystem, when it is under one of
 * the home user's folders, is a regular text file, image or PDF within the
 * preview limits, and neither the requested nor resolved path is denied below. */
export const FILE_REF_LIMITS = { textBytes: 2 * 1024 * 1024, pathChars: 4096 } as const;
const CHECKOUT = "checkout";
const RUN_ID = /^[A-Za-z0-9._-]{1,256}$/;
const REVISION = /^[a-f0-9]{64}$/;
const CONTROL = /[\x00-\x1f\x7f]/;

export type FileRefRequest = { version: 1; runId: string; path: string; knownRevision?: string };
/** Where host files may come from, and the home folders whose hidden files are private. */
export type FileRefPolicy = {
  /** `~` in a path. */
  home: string;
  /** The home user's home folders: HOME, and the volume folder its dotfiles link into on Cloud. */
  homes: readonly string[];
  /** Host files are read only under these. */
  roots: readonly string[];
};
export type FileRefDependencies = {
  /** The project's live file grants: its checkout, and each running agent's workspace. */
  grants(signal: AbortSignal): Promise<FileGrant[]>;
  /** Read through the file service, for a fork only its agent's mount holds. */
  read(scope: FileScope, relative: string, knownRevision: string | undefined, signal: AbortSignal): Promise<FileDocument>;
  policy: FileRefPolicy;
};

// ---------- Deny list ----------

export const PRIVATE_FILE = "This file can hold keys or tokens, so the viewer does not open it";
export const SYSTEM_FILE = "System files do not open in the viewer";
const SYSTEM = ["/proc", "/sys", "/dev", "/run"];
/** Folders, at any depth, that hold keys, tokens or repository internals. */
const PRIVATE_FOLDERS = new Set([".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker", ".fly", ".gcloud", "gcloud",
  ".password-store", "keyrings", ".terraform.d", ".git", ".jj", ".rudder", ".rudder-workspaces", ".rudder-worktrees"]);
/** Files, at any depth, that hold keys or tokens. */
const PRIVATE_NAMES = new Set([".npmrc", ".netrc", ".pypirc", ".git-credentials", ".credentials.json", "credentials",
  "credentials.json", "credentials.toml", "auth.json", ".claude.json", "RUDDER_SHARED.md", ".vault-token", ".htpasswd", ".pgpass",
  // Claude Code's local settings, in a project or a home folder, can hold API keys in `env`.
  "settings.local.json"]);
// .env files, key files, SSH keys and shell or REPL histories (commands typed with tokens in them).
const PRIVATE_NAME = /^\.env(?:\.|$)|\.(?:pem|key|p12|pfx|jks|keystore|kdbx|ppk|gpg)$|^id_(?:rsa|dsa|ecdsa|ed25519)|^\.[\w.-]*history$|_history$/i;
/** The hidden folders of a home folder that open: agents keep plans, memory
 * and instructions there. Every other hidden file or folder directly in a
 * home folder (.config, .local, .bashrc, a token file) stays private. */
const OPEN_HOME_FOLDERS = new Set([".claude", ".codex"]);
/** Their settings, which can hold API keys (an `env` block, an MCP server's key). */
const HOME_SETTINGS = new Set([".claude/settings.json", ".codex/config.toml"]);
/** Inside those, anything named like a secret. */
const SECRET_LIKE = /(?:^|[._-])(?:auth|oauth|credentials?|secrets?|tokens?|passwords?|api[._-]?keys?|private[._-]?keys?)(?:[._-]|$)/i;

/** `file` relative to `folder`, or null when it is not inside it. */
export function inside(folder: string, file: string): string | null {
  if (file === folder) return "";
  const prefix = folder.endsWith("/") ? folder : `${folder}/`;
  return file.startsWith(prefix) ? file.slice(prefix.length) : null;
}

/** Why the viewer does not open this absolute, normalized path, or null.
 * `own` is the chat's own folders (its agent's workspace and the checkout):
 * a workspace lives under `.rudder-workspaces`, so for a file in one of them
 * only the path below that folder is checked for private names. */
export function privateReason(file: string, homes: readonly string[], own: readonly string[] = []): string | null {
  if (SYSTEM.some((root) => inside(root, file) !== null)) return SYSTEM_FILE;
  const below = own.map((folder) => inside(folder, file)).filter((rest): rest is string => rest !== null);
  const parts = (below.length ? below.reduce((a, b) => (a.length <= b.length ? a : b)) : file).split("/").filter(Boolean);
  if (parts.some((part) => PRIVATE_FOLDERS.has(part) || PRIVATE_NAMES.has(part) || PRIVATE_NAME.test(part))) return PRIVATE_FILE;
  for (const home of homes) {
    const rest = inside(home, file);
    if (!rest) continue;
    const [top, ...below] = rest.split("/");
    if (top.startsWith(".") && !OPEN_HOME_FOLDERS.has(top)) return PRIVATE_FILE;
    if (HOME_SETTINGS.has(rest)) return PRIVATE_FILE;
    if (OPEN_HOME_FOLDERS.has(top) && below.some((part) => SECRET_LIKE.test(part))) return PRIVATE_FILE;
  }
  return null;
}

/** Why a host file outside the home user's folders does not open, or null. */
export function outsideReason(file: string, roots: readonly string[]): string | null {
  return roots.some((root) => inside(root, file) !== null) ? null : `Only files under ${roots.join(", ")} open in the viewer`;
}

// ---------- Reading ----------

const unsupported = () => new ProjectFileError("unsupported", "This file type has no preview");
const tooLarge = (limit: number) => new ProjectFileError("large", `This file is over ${limit / (1024 * 1024)} MB, too large to show here`);

function validRequest(value: unknown): FileRefRequest {
  const request = value as FileRefRequest | undefined;
  if (request?.version !== 1 || typeof request.runId !== "string" || !RUN_ID.test(request.runId)
    || typeof request.path !== "string" || !request.path || request.path.length > FILE_REF_LIMITS.pathChars || CONTROL.test(request.path)
    || (request.knownRevision !== undefined && !(typeof request.knownRevision === "string" && REVISION.test(request.knownRevision)))) {
    throw new ProjectFileError("invalid", "Invalid file request");
  }
  return request;
}

/** Read the file `request.path` names, as the chat's agent meant it. The
 * document's `path` is the absolute path it resolved to: later reads (the
 * viewer's refresh) ask for that. */
export async function readFileRef(value: unknown, dependencies: FileRefDependencies, signal: AbortSignal): Promise<FileDocument> {
  const request = validRequest(value);
  const input = request.path;
  const grants = await dependencies.grants(signal);
  const places = [request.runId, CHECKOUT].map((id) => grants.find((grant) => grant.scope.workspaceId === id)).filter((grant): grant is FileGrant => !!grant);
  const policy = await canonical(dependencies.policy);
  let candidates: string[];
  if (input === "~" || input.startsWith("~/")) candidates = [path.join(policy.home, input.slice(1))];
  else if (path.isAbsolute(input)) candidates = [path.normalize(input)];
  else candidates = [...new Set(places.map((place) => path.resolve(place.root, input)))];
  if (!candidates.length) throw new ProjectFileError("denied", "This project's files are not available right now");
  for (const [index, file] of candidates.entries()) {
    try {
      return await readAt(file, places, request.knownRevision, dependencies, policy, signal);
    } catch (error) {
      if (!(error instanceof ProjectFileError && error.code === "missing")) throw error;
      if (index === candidates.length - 1) throw new ProjectFileError("missing", `File not found: ${input}`);
    }
  }
  throw new ProjectFileError("missing", `File not found: ${input}`);
}

/** The policy's folders, also as the paths they resolve to. */
async function canonical(policy: FileRefPolicy): Promise<FileRefPolicy> {
  const both = async (folders: readonly string[]) => [...new Set([...folders, ...await Promise.all(folders.map((folder) => realpath(folder)))])];
  return { home: policy.home, homes: await both(policy.homes), roots: await both(policy.roots) };
}

async function readAt(file: string, places: FileGrant[], knownRevision: string | undefined, dependencies: FileRefDependencies,
  policy: FileRefPolicy, signal: AbortSignal): Promise<FileDocument> {
  const own = [...new Set([...places.map((place) => place.root), ...await Promise.all(places.map((place) => realpath(place.root).catch(() => place.root)))])];
  // (A fork's folder that only its agent's mount holds has no other name here.)
  const reason = privateReason(file, policy.homes, own);
  if (reason) throw new ProjectFileError("denied", reason);
  // A fork's files exist only in its agent's mount: the file service reads them there.
  const fork = places.find((place) => place.access.kind === "namespace" && inside(place.root, file) !== null);
  if (!fork) return readHost(file, knownRevision, policy, own, signal);
  const relative = inside(fork.root, file)!;
  if (!relative) throw new ProjectFileError("unsupported", "That is a folder. Only files open in the viewer");
  const kind = classification(file).kind;
  const limit = kind === "image" || kind === "pdf" ? FILE_LIMITS.mediaBytes : FILE_REF_LIMITS.textBytes;
  let doc: FileDocument;
  try { doc = await dependencies.read(fork.scope, relative, knownRevision, signal); }
  catch (error) { throw error instanceof ProjectFileError && error.code === "large" ? tooLarge(limit) : error; }
  if (doc.kind === "image" || doc.kind === "pdf") return { ...doc, path: file };
  if (!["text", "markdown", "html"].includes(doc.kind)) throw unsupported();
  if (typeof doc.text === "string" && Buffer.byteLength(doc.text) > limit) throw tooLarge(limit);
  return shown(file, doc.revision, doc.text, doc.unchanged === true);
}

/** Markdown previews; everything else shows as text with its syntax colored. */
function shown(file: string, revision: string, text: string | undefined, unchanged: boolean): FileDocument {
  const format = classification(file);
  const kind = format.kind === "markdown" ? "markdown" : "text";
  return unchanged ? { path: file, revision, mime: format.mime, kind, unchanged: true } : { path: file, revision, mime: format.mime, kind, text };
}

async function readHost(file: string, knownRevision: string | undefined, policy: FileRefPolicy, own: readonly string[], signal: AbortSignal): Promise<FileDocument> {
  const check = (at: string) => {
    const reason = outsideReason(at, policy.roots) ?? privateReason(at, policy.homes, own);
    if (reason) throw new ProjectFileError("denied", reason);
  };
  check(file);
  let real: string;
  let handle: FileHandle;
  try {
    // What the path names after every symlink is checked too: a link in a
    // project never reaches a private file.
    real = await realpath(file);
    check(real);
    handle = await open(real, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  } catch (error) { throw fsError(error); }
  try {
    const stat = await handle.stat();
    if (stat.isDirectory()) throw new ProjectFileError("unsupported", "That is a folder. Only files open in the viewer");
    if (!stat.isFile()) throw new ProjectFileError("unsupported", "Only regular files open in the viewer");
    // The descriptor is the file the checks passed: a folder above it swapped
    // for a symlink after realpath is refused.
    const named = process.platform === "linux" ? null : await lstat(real);
    const opened = named ? named.ino === stat.ino && named.dev === stat.dev : await realpath(`/proc/self/fd/${handle.fd}`) === real;
    if (!opened) throw new ProjectFileError("changed", "The file moved while opening it");
    const format = classification(file);
    const media = format.kind === "image" || format.kind === "pdf";
    const limit = media ? FILE_LIMITS.mediaBytes : FILE_REF_LIMITS.textBytes;
    if (stat.size > limit) throw tooLarge(limit);
    const bytes = Buffer.alloc(stat.size + 1);
    let offset = 0;
    while (offset < bytes.length) {
      signal.throwIfAborted();
      const read = await handle.read(bytes, offset, Math.min(64 * 1024, bytes.length - offset), offset);
      if (!read.bytesRead) break;
      offset += read.bytesRead;
    }
    if (offset !== stat.size) throw new ProjectFileError("changed", "The file changed while reading it");
    const content = bytes.subarray(0, offset);
    if (media) return mediaDocument(file, format, content, knownRevision);
    const revision = revisionOf(content);
    if (revision === knownRevision) return shown(file, revision, undefined, true);
    const text = textOf(content);
    if (text === null) throw unsupported();
    return shown(file, revision, text, false);
  } catch (error) { throw error instanceof ProjectFileError ? error : fsError(error); }
  finally { await handle.close(); }
}

/** A filesystem failure as a message the viewer shows; never a raw errno with a path. */
function fsError(error: unknown): unknown {
  if (error instanceof ProjectFileError) return error;
  const code = (error as NodeJS.ErrnoException)?.code;
  if (code === "ENOENT" || code === "ENOTDIR") return new ProjectFileError("missing", "File not found");
  if (code === "EACCES" || code === "EPERM") return new ProjectFileError("denied", "The cloud machine's user cannot read this file");
  if (code === "ELOOP") return new ProjectFileError("denied", "This path loops through symlinks");
  if (code === "ENAMETOOLONG") return new ProjectFileError("invalid", "This path is too long");
  return new ProjectFileError("changed", "The file could not be read");
}
