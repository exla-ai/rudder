import type { IncomingMessage } from "node:http";
import { CloudProjectFileError, jsonBody, SAVE_BODY_BYTES, WRITE_TEXT_BYTES } from "./relay.js";

/** A project's notes: RUDDER_NOTES.md in its checkout on the home, which only
 * the user writes (src/project-files/notes.ts there). Only the home's own
 * account reaches them, as with the Files page's agent setup: teammates never
 * do, even in a project shared with them. */
export const PROJECT_NOTES_PATH = "/api/app/project-notes";

type Dependencies = {
  /** The account's own connected home, or null. */
  ownHome(accountId: string): string | null;
  workspace(accountId: string, workspaceId: string): { homeId: string };
  rpc(homeId: string, method: string, params: Record<string, unknown>, signal: AbortSignal): Promise<unknown>;
};
type Request =
  | { version: 1; operation: "read" }
  | { version: 1; operation: "write"; baseRevision: string | null; text: string }
  | { version: 1; operation: "git"; keepOut: boolean };

const REVISION = /^[a-f0-9]{64}$/;
const TOO_LARGE = "Notes over 1 MiB cannot be saved here";
const invalidReply = () => new CloudProjectFileError(502, "Invalid notes reply");
const record = (value: unknown): Record<string, unknown> => {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw invalidReply();
  return value as Record<string, unknown>;
};

function gitFrom(value: unknown) {
  const git = record(value);
  if (git.state === "excluded" || git.state === "included" || git.state === "committed") return { state: git.state };
  if (git.state === "ignored" && typeof git.by === "string" && git.by.length <= 4096 && !/[\x00-\x1f\x7f]/.test(git.by)) return { state: "ignored" as const, by: git.by };
  throw invalidReply();
}

/** Only known fields with bounded values reach the browser. */
function notesFrom(value: unknown) {
  const notes = record(value);
  if (typeof notes.text !== "string" || Buffer.byteLength(notes.text) > 4 * 1024 * 1024
    || !(notes.revision === null || (typeof notes.revision === "string" && REVISION.test(notes.revision)))) throw invalidReply();
  return { text: notes.text, revision: notes.revision as string | null, git: gitFrom(notes.git) };
}

function replyFrom(request: Request, value: unknown) {
  const reply = record(value);
  if (request.operation === "read") return notesFrom(reply);
  if (request.operation === "git") return { git: gitFrom(reply.git) };
  if (reply.outcome === "conflict") return { outcome: "conflict", ...notesFrom(reply) };
  if (reply.outcome !== "saved" || typeof reply.revision !== "string" || !REVISION.test(reply.revision)) throw invalidReply();
  return { outcome: "saved", revision: reply.revision, ...(reply.git === undefined ? {} : { git: gitFrom(reply.git) }) };
}

function actionFrom(body: unknown): { workspaceId: string; request: Request } {
  const value = body as Record<string, unknown> | null;
  const keys = value && typeof value === "object" && !Array.isArray(value) ? Object.keys(value) : null;
  const bad = () => new CloudProjectFileError(400, "Invalid notes request");
  if (!keys || value!.version !== 1 || typeof value!.workspaceId !== "string" || !value!.workspaceId || value!.workspaceId.length > 256) throw bad();
  const workspaceId = value!.workspaceId;
  const only = (allowed: string[]) => { if (keys.length !== allowed.length || keys.some((k) => !allowed.includes(k))) throw bad(); };
  if (value!.operation === "write") {
    only(["version", "workspaceId", "operation", "baseRevision", "text"]);
    const { baseRevision, text } = value!;
    if (!(baseRevision === null || (typeof baseRevision === "string" && REVISION.test(baseRevision))) || typeof text !== "string") throw bad();
    if (Buffer.byteLength(text) > WRITE_TEXT_BYTES) throw new CloudProjectFileError(413, TOO_LARGE);
    return { workspaceId, request: { version: 1, operation: "write", baseRevision, text } };
  }
  if (value!.operation === "git") {
    only(["version", "workspaceId", "operation", "keepOut"]);
    if (typeof value!.keepOut !== "boolean") throw bad();
    return { workspaceId, request: { version: 1, operation: "git", keepOut: value!.keepOut } };
  }
  throw bad();
}

export class ProjectNotesRoutes {
  constructor(private readonly dependencies: Dependencies) {}
  /** GET reads a project's notes; POST saves them or sets "Keep out of Git". Already behind account auth. */
  async handle(req: IncomingMessage, url: URL, accountId: string, signal: AbortSignal): Promise<unknown> {
    let workspaceId: string;
    let request: Request;
    if (req.method === "GET") {
      workspaceId = url.searchParams.get("workspaceId") ?? "";
      if (!workspaceId || workspaceId.length > 256 || [...url.searchParams.keys()].some((k) => k !== "workspaceId") || url.searchParams.getAll("workspaceId").length !== 1) {
        throw new CloudProjectFileError(400, "Select a project");
      }
      request = { version: 1, operation: "read" };
    } else if (req.method === "POST" && !url.search) {
      ({ workspaceId, request } = actionFrom(await jsonBody(req, SAVE_BODY_BYTES, TOO_LARGE, signal)));
    } else throw new CloudProjectFileError(405, "Use GET to read notes and POST to change them");
    const homeId = this.dependencies.ownHome(accountId);
    if (!homeId) throw new CloudProjectFileError(404, "Your cloud machine is not connected");
    if (this.dependencies.workspace(accountId, workspaceId).homeId !== homeId) throw new CloudProjectFileError(403, "Notes are only for projects on your own cloud machine");
    return replyFrom(request, await this.dependencies.rpc(homeId, "projectNotes", { workspaceId, request }, signal));
  }
}
