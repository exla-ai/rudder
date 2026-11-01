/** Explicit managed-only HTTP adapter. PostgreSQL registry and authenticated
 * socket owners supply authority; request paths, aliases and WS state do not.
 * No legacy SQLite repo lookup, wake/restart, rekey, automatic retry or default
 * backend/model/API profile enters this path. */
import type { IncomingMessage, ServerResponse } from "node:http";
import type { ManagedHomeRegistry, HostOperation } from "./managed-home-provisioning/index.js";
import { originalTime } from "./managed-home-provisioning/fly.js";

const PREFIX = "/api/rudder/home/managed/";
const LIMIT = 32 * 1024;
const TEXT_LIMIT = 24 * 1024;
type EffectName = "open-prepared-project" | "start-task" | "build-current-source-sdk" | "receipt";
export interface ManagedMachineApiDependencies {
  readonly registry: ManagedHomeRegistry;
  readJsonBody(request: IncomingMessage, limit: number): Promise<unknown>;
  sendJson(response: ServerResponse, status: number, body: unknown): void;
  /** Actual accepted websocket object; this lookup is not readiness proof. */
  currentHomeSocket(homeId: string): object | undefined;
  /** MUST synchronously compare SAME socket against currentHomeSocket then use
   * existing homeRpcSessions.request(homeId,socket,...). Never recapture a
   * replacement socket inside a generic homeRpc(homeId) after async checks. */
  rpcOnExactSocket(homeId: string, socket: object, method: string,
    params: Record<string, unknown>, remainingMs: number): Promise<unknown>;
}
function rejected(message: string): never {
  throw Object.assign(new Error(message), { status: 400 });
}
function record(value: unknown): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) rejected("managed request must be an object");
  return value as Record<string, unknown>;
}
function text(value: unknown, name: string, max: number): string {
  if (typeof value !== "string" || value.length === 0 || Buffer.byteLength(value) > max || /[\x00-\x1f\x7f]/.test(value)) rejected(`${name} is required`);
  return value as string;
}
function key(value: unknown, name: string): string {
  const result = text(value, name, 128);
  if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/.test(result)) rejected(`${name} is invalid`);
  return result;
}
export type ManagedMachineInput = Readonly<{ workspace: string; commandKey: string;
  selection?: Readonly<{ backend: "codex" | "claude"; model: string; profileId: string }>;
  task?: string; lookupKey?: string; sdkCase?:"accepted"|"proc-deny"|"job-deny";repeatIdentical?:boolean }>;
/** DATA validation only. This does not issue a home/project/task capability. */
export function parseManagedMachineInput(name: EffectName, value: unknown): ManagedMachineInput {
  const body = record(value);
  const fields = name === "receipt" ? ["workspace", "command_key", "lookup_command_key"]
    : name==="build-current-source-sdk"?["workspace","command_key","case","repeat_identical"]:["workspace", "command_key", "backend", "model", "profile_id", ...(name === "start-task" ? ["text"] : [])];
  if (Object.keys(body).some(field => !fields.includes(field))) rejected("unknown managed request field");
  const workspace = key(body.workspace, "workspace");
  const commandKey = key(body.command_key, "command_key");
  if (name === "receipt") return Object.freeze({ workspace, commandKey, lookupKey: key(body.lookup_command_key, "lookup_command_key") });
  if(name==="build-current-source-sdk"){if(typeof body.case!=="string"||!["accepted","proc-deny","job-deny"].includes(body.case)||typeof body.repeat_identical!=="boolean")rejected("explicit SDK case and repeat_identical are required");return Object.freeze({workspace,commandKey,sdkCase:body.case as "accepted"|"proc-deny"|"job-deny",repeatIdentical:body.repeat_identical});}
  if (body.backend !== "codex" && body.backend !== "claude") rejected("explicit backend is required");
  const model = text(body.model, "model", 256);
  const profileId = text(body.profile_id, "profile_id", 256);
  if (!model.trim() || !profileId.trim()) rejected("explicit model and API profile are required");
  let task: string | undefined;
  if (name === "start-task") {
    if (typeof body.text !== "string" || !body.text.trim() || Buffer.byteLength(body.text) > TEXT_LIMIT || body.text.includes("\0")) rejected("task text is required and must be at most 24 KiB");
    task = body.text;
  }
  return Object.freeze({ workspace, commandKey, selection: Object.freeze({ backend: body.backend, model, profileId }), ...(task === undefined ? {} : { task }) });
}
function operation(name: EffectName, input: ManagedMachineInput, alias: string): HostOperation {
  if (name === "receipt") return { op: "ReceiptV1", lookup_command_key: input.lookupKey! };
  if(name==="build-current-source-sdk")return {op:"BuildCurrentSourceSdkV1",project_alias:alias,case:input.sdkCase!,repeat_identical:input.repeatIdentical!};
  const selected = input.selection!;
  // Current qualified registry/image profile is Codex-only. Claude remains an
  // explicit unsupported selection until its exact artifact/profile qualifies.
  if (selected.backend !== "codex") rejected("selected managed backend is not qualified");
  const selection = { project_alias: alias, backend: "codex" as const, model: selected.model, profile_id: selected.profileId };
  return name === "start-task" ? { op: "StartTaskV1", ...selection, text: input.task! }
    : { op: "OpenPreparedProjectV1", ...selection };
}
/** Called only AFTER the existing requireBearer(req). It returns false for all
 * legacy methods/routes, preserving their current behavior. */
export async function handleManagedMachineApi(request: IncomingMessage, response: ServerResponse,
  url: URL, authenticatedAccountId: string, deps: ManagedMachineApiDependencies): Promise<boolean> {
  if (!url.pathname.startsWith(PREFIX)) return false;
  const suffix = url.pathname.slice(PREFIX.length);
  if (request.method !== "POST" || !["open-prepared-project", "start-task", "build-current-source-sdk", "receipt"].includes(suffix)) {
    deps.sendJson(response, 404, { error: "managed operation not found" }); return true;
  }
  const name = suffix as EffectName;
  const input = parseManagedMachineInput(name, await deps.readJsonBody(request, LIMIT));
  // Fresh PG-owned homes have no legacy SQLite rows. This issuer verifies exact
  // account/home/project and requires actual assignment/provisioning receipts.
  const identity = await deps.registry.requireManagedHomeRepo(authenticatedAccountId, input.workspace);
  const route = await deps.registry.resolveManagedHomeRoute(authenticatedAccountId, identity.home.id, identity.repo.id);
  if (route.accountId !== authenticatedAccountId || route.homeId !== identity.home.id || route.projectAlias !== identity.repo.id) {
    throw Object.assign(new Error("managed home binding changed"), { status: 409 });
  }
  await route.assertRoutableCurrent();
  const command = operation(name, input, route.projectAlias);
  // Registry commits acceptance/outbox then Started BEFORE calling this trusted
  // transport. Same key/hash retries return its original result or uncertainty;
  // they never create a second native preparation budget or a replacement send.
  const result = await deps.registry.dispatchManagedHomeCommand(route, input.commandKey, command,
    async (method, params, remainingMs) => {
      // Async route/provider observations consume this SAME remainder; a later
      // request must not regain time spent authenticating the actual socket.
      const original = originalTime(remainingMs);
      const socket = deps.currentHomeSocket(route.homeId);
      if (!socket) throw Object.assign(new Error("managed home is not connected"), { status: 503 });
      await deps.registry.assertManagedHomeSocketMatchesRoute(socket, route);
      if (deps.currentHomeSocket(route.homeId) !== socket) throw Object.assign(new Error("managed home socket changed; original effect is uncertain"), { status: 503 });
      return await deps.rpcOnExactSocket(route.homeId, socket, method, params, original.remaining());
    });
  deps.sendJson(response, 200, result);
  return true;
}
