import type { IncomingMessage, ServerResponse } from "node:http";

/** Whether one agent's turns update the canvas (cloud/worker/vibeprint-track.mjs). */
export type VibeprintTracking = { on: boolean; pending?: boolean; lastAt?: string; error?: string };

/** A project's vibeprint canvas as the app sees it (cloud/worker/vibeprint-host.mjs). */
export type VibeprintView = (
  | { state: "off" }
  | { state: "starting"; note?: string }
  | { state: "running"; url: string; token: string }
  | { state: "failed"; error: string }
) & { tracking?: VibeprintTracking };

type Dependencies = {
  /** Account and project check; the home and the repo id its RPCs name. */
  workspace(accountId: string, workspaceId: string): { homeId: string; repoId: string };
  rpc(homeId: string, params: Record<string, unknown>, signal: AbortSignal): Promise<unknown>;
};

class VibeprintRouteError extends Error {
  constructor(readonly status: number, message: string) { super(message); }
}

const ID = /^[A-Za-z0-9_-]{1,256}$/;
const RUN = /^[A-Za-z0-9._-]{1,128}$/;
const TOKEN = /^[\w-]{16,128}$/;
const MESSAGE_LIMIT = 16_000;

function relayUrl(value: unknown): value is string {
  if (typeof value !== "string" || value.length > 512) return false;
  try {
    const url = new URL(value);
    return url.protocol === "https:" && url.pathname === "/" && !url.search && !url.hash && !url.username && !url.password;
  } catch { return false; }
}

const text = (v: unknown): v is string => typeof v === "string" && v.length > 0 && v.length <= 500;

function checkTracking(raw: unknown): VibeprintTracking {
  const value = raw as Record<string, unknown> | null;
  if (typeof value?.on !== "boolean" || (value.pending !== undefined && typeof value.pending !== "boolean")
    || (value.lastAt !== undefined && (!text(value.lastAt) || Number.isNaN(Date.parse(value.lastAt))))
    || (value.error !== undefined && !text(value.error))) {
    throw new VibeprintRouteError(502, "The home sent an invalid tracking status");
  }
  return {
    on: value.on,
    ...(value.pending ? { pending: true } : {}),
    ...(value.lastAt ? { lastAt: value.lastAt as string } : {}),
    ...(value.error ? { error: value.error as string } : {}),
  };
}

/** Only well-formed views reach the browser; anything else is a bad reply from the home. */
export function checkView(raw: unknown): VibeprintView {
  const value = raw as Record<string, unknown> | null;
  const tracking = value?.tracking === undefined ? {} : { tracking: checkTracking(value.tracking) };
  if (value?.state === "off") return { state: "off", ...tracking };
  if (value?.state === "starting") return { state: "starting", ...(text(value.note) ? { note: value.note } : {}), ...tracking };
  if (value?.state === "running" && relayUrl(value.url) && typeof value.token === "string" && TOKEN.test(value.token)) {
    return { state: "running", url: value.url, token: value.token, ...tracking };
  }
  if (value?.state === "failed" && text(value.error)) return { state: "failed", error: value.error, ...tracking };
  throw new VibeprintRouteError(502, "The home sent an invalid canvas status");
}

async function jsonBody(req: IncomingMessage): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of req) {
    size += (chunk as Buffer).length;
    if (size > 4096) throw new VibeprintRouteError(413, "Request too large");
    chunks.push(chunk as Buffer);
  }
  try {
    const value = JSON.parse(Buffer.concat(chunks).toString("utf8")) as unknown;
    if (value && typeof value === "object" && !Array.isArray(value)) return value as Record<string, unknown>;
  } catch { /* reported below */ }
  throw new VibeprintRouteError(400, "Send JSON");
}

/**
 * GET  /api/app/vibeprint?workspace=<project>[&run=<agent>]  the canvas status (starts one left on),
 *                                                           with that agent's tracking
 * POST /api/app/vibeprint {workspace, op, run?}              op: start | stop | brief (with since) |
 *                                                           tracking (run, on, check)
 *
 * A running canvas comes back with its relay link and access token, for the
 * app's frame only; the brief is the canvas as text the user may send to an agent.
 */
export class VibeprintRoutes {
  #active = 0;
  constructor(private readonly dependencies: Dependencies) {}

  async handle(req: IncomingMessage, res: ServerResponse, url: URL, accountId: string): Promise<boolean> {
    if (url.pathname !== "/api/app/vibeprint") return false;
    const send = (status: number, value: unknown) => {
      res.writeHead(status, { "content-type": "application/json; charset=utf-8", "cache-control": "no-store", "x-content-type-options": "nosniff" });
      res.end(JSON.stringify(value));
    };
    try {
      if (this.#active >= 8) throw new VibeprintRouteError(429, "Too many canvas requests");
      this.#active++;
      try {
        let workspace: string;
        let params: Record<string, unknown>;
        if (req.method === "GET") {
          if ([...url.searchParams.keys()].some((key) => key !== "workspace" && key !== "run")
            || url.searchParams.getAll("workspace").length !== 1 || url.searchParams.getAll("run").length > 1) {
            throw new VibeprintRouteError(400, "Select a project");
          }
          workspace = url.searchParams.get("workspace") ?? "";
          const run = url.searchParams.get("run");
          if (run !== null && !RUN.test(run)) throw new VibeprintRouteError(400, "Select an agent");
          params = run === null ? { op: "status" } : { op: "status", run };
        } else if (req.method === "POST") {
          const body = await jsonBody(req);
          workspace = typeof body.workspace === "string" ? body.workspace : "";
          const op = body.op;
          if (op !== "start" && op !== "stop" && op !== "brief" && op !== "tracking") throw new VibeprintRouteError(400, "Unknown canvas request");
          const run = body.run;
          if (run !== undefined && (typeof run !== "string" || !RUN.test(run))) throw new VibeprintRouteError(400, "Select an agent");
          const since = body.since;
          if (since !== undefined && (typeof since !== "string" || since.length > 40 || Number.isNaN(Date.parse(since)))) {
            throw new VibeprintRouteError(400, "Invalid time");
          }
          if (op === "tracking") {
            if (run === undefined || typeof body.on !== "boolean" || (body.check !== undefined && typeof body.check !== "boolean")) {
              throw new VibeprintRouteError(400, "Tracking needs an agent and on");
            }
            params = { op, run, on: body.on, check: body.check === true };
          } else {
            params = { op, ...(run !== undefined ? { run } : {}), ...(op === "brief" && typeof since === "string" ? { since } : {}) };
          }
        } else throw new VibeprintRouteError(405, "Use GET or POST");
        if (!ID.test(workspace)) throw new VibeprintRouteError(400, "Select a project");
        const { homeId, repoId } = this.dependencies.workspace(accountId, workspace);
        let raw: unknown;
        try {
          raw = await this.dependencies.rpc(homeId, { workspaceId: repoId, ...params }, AbortSignal.timeout(10_000));
        } catch (error) {
          throw new VibeprintRouteError(409, error instanceof Error && error.message ? error.message.slice(0, 300) : "The home could not reach the canvas");
        }
        if (params.op !== "brief") return send(200, checkView(raw)), true;
        const brief = raw as Record<string, unknown> | null;
        if (typeof brief?.text === "string" && brief.text.length > MESSAGE_LIMIT) throw new VibeprintRouteError(413, "The canvas is too large to send as one message");
        if (typeof brief?.text !== "string" || typeof brief.at !== "string"
          || Number.isNaN(Date.parse(brief.at)) || !Number.isInteger(brief.components)) {
          throw new VibeprintRouteError(502, "The home sent an invalid canvas brief");
        }
        send(200, { text: brief.text, at: brief.at, components: brief.components });
      } finally { this.#active--; }
    } catch (error) {
      const status = error instanceof VibeprintRouteError ? error.status
        : error && typeof error === "object" && "status" in error && typeof (error as { status: unknown }).status === "number" ? (error as { status: number }).status : 500;
      send(status, { error: error instanceof Error ? error.message : "Canvas unavailable" });
    }
    return true;
  }
}
