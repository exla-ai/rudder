import type { IncomingMessage } from "node:http";
import { CloudProjectFileError, jsonBody, SAVE_BODY_BYTES, WRITE_TEXT_BYTES } from "./relay.js";

/** One agent's notes: notes.md in its run folder on the home, which only the
 * user writes (src/project-files/agent-notes.ts there). Only the home's own
 * account reaches them, as with the Files page's agent setup: teammates never
 * do, even in a project shared with them. */
export const AGENT_NOTES_PATH = "/api/app/agent-notes";

type Dependencies = {
  /** The account's own connected home, or null. */
  ownHome(accountId: string): string | null;
  workspace(accountId: string, workspaceId: string): { homeId: string };
  rpc(homeId: string, method: string, params: Record<string, unknown>, signal: AbortSignal): Promise<unknown>;
};
type Request =
  | { version: 1; operation: "read" }
  | { version: 1; operation: "write"; baseRevision: string | null; text: string };

const RUN_ID = /^[A-Za-z0-9._-]{1,256}$/;
const REVISION = /^[a-f0-9]{64}$/;
const TOO_LARGE = "Notes over 1 MiB cannot be saved here";
const invalidReply = () => new CloudProjectFileError(502, "Invalid notes reply");
const record = (value: unknown): Record<string, unknown> => {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw invalidReply();
  return value as Record<string, unknown>;
};
const validId = (value: unknown, max: number): value is string => typeof value === "string" && value.length > 0 && value.length <= max;

/** Only known fields with bounded values reach the browser. */
function notesFrom(value: unknown) {
  const notes = record(value);
  if (typeof notes.text !== "string" || Buffer.byteLength(notes.text) > 4 * 1024 * 1024
    || !(notes.revision === null || (typeof notes.revision === "string" && REVISION.test(notes.revision)))) throw invalidReply();
  return { text: notes.text, revision: notes.revision as string | null };
}

function replyFrom(request: Request, value: unknown) {
  const reply = record(value);
  if (request.operation === "read") return notesFrom(reply);
  if (reply.outcome === "conflict") return { outcome: "conflict", ...notesFrom(reply) };
  if (reply.outcome !== "saved" || typeof reply.revision !== "string" || !REVISION.test(reply.revision)) throw invalidReply();
  return { outcome: "saved", revision: reply.revision };
}

type Target = { workspaceId: string; runId: string };

function saveFrom(body: unknown): Target & { request: Request } {
  const value = body as Record<string, unknown> | null;
  const keys = value && typeof value === "object" && !Array.isArray(value) ? Object.keys(value) : null;
  const allowed = ["version", "workspaceId", "runId", "operation", "baseRevision", "text"];
  if (!keys || keys.length !== allowed.length || keys.some((k) => !allowed.includes(k)) || value!.version !== 1 || value!.operation !== "write"
    || !validId(value!.workspaceId, 256) || !validId(value!.runId, 256) || !RUN_ID.test(value!.runId)) {
    throw new CloudProjectFileError(400, "Invalid notes request");
  }
  const { baseRevision, text } = value!;
  if (!(baseRevision === null || (typeof baseRevision === "string" && REVISION.test(baseRevision))) || typeof text !== "string") {
    throw new CloudProjectFileError(400, "Invalid notes request");
  }
  if (Buffer.byteLength(text) > WRITE_TEXT_BYTES) throw new CloudProjectFileError(413, TOO_LARGE);
  return { workspaceId: value!.workspaceId, runId: value!.runId, request: { version: 1, operation: "write", baseRevision, text } };
}

function readFrom(url: URL): Target {
  const keys = [...url.searchParams.keys()];
  const workspaceId = url.searchParams.get("workspaceId");
  const runId = url.searchParams.get("runId");
  if (keys.length !== 2 || !validId(workspaceId, 256) || !validId(runId, 256) || !RUN_ID.test(runId)) {
    throw new CloudProjectFileError(400, "Select an agent");
  }
  return { workspaceId, runId };
}

export class AgentNotesRoutes {
  constructor(private readonly dependencies: Dependencies) {}
  /** GET reads an agent's notes; POST saves them. Already behind account auth. */
  async handle(req: IncomingMessage, url: URL, accountId: string, signal: AbortSignal): Promise<unknown> {
    let target: Target;
    let request: Request;
    if (req.method === "GET") {
      target = readFrom(url);
      request = { version: 1, operation: "read" };
    } else if (req.method === "POST" && !url.search) {
      ({ request, ...target } = saveFrom(await jsonBody(req, SAVE_BODY_BYTES, TOO_LARGE, signal)));
    } else throw new CloudProjectFileError(405, "Use GET to read notes and POST to save them");
    const homeId = this.dependencies.ownHome(accountId);
    if (!homeId) throw new CloudProjectFileError(404, "Your cloud machine is not connected");
    const reply = await ownStatus(async () => {
      if (this.dependencies.workspace(accountId, target.workspaceId).homeId !== homeId) throw new CloudProjectFileError(403, "Notes are only for agents on your own cloud machine");
      return this.dependencies.rpc(homeId, "agentNotes", { workspaceId: target.workspaceId, runId: target.runId, request }, signal);
    });
    return replyFrom(request, reply);
  }
}

/** The project check's and the home's own status reach the browser: 503 is a
 * sleeping home, which Notes tells apart, and Notes has no file bindings for
 * ProjectFileRoutes' 403 to renew. A home too old for notes (server.ts
 * homeNeedsUpdate) keeps `needsUpdate`, so the app offers Update now. */
async function ownStatus<T>(work: () => Promise<T>): Promise<T> {
  try { return await work(); }
  catch (error) {
    const { status, needsUpdate } = (error ?? {}) as { status?: unknown; needsUpdate?: unknown };
    if (!(error instanceof CloudProjectFileError) && error instanceof Error && typeof status === "number") {
      throw Object.assign(new CloudProjectFileError(status, error.message), needsUpdate === true ? { needsUpdate: true } : {});
    }
    throw error;
  }
}
