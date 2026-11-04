import { createHash } from "node:crypto";
import { operationDefinition } from "./registry.js";

export type ExecutionScope = {
  project_id: string;
  location: "local" | "cloud";
  checkout: "main" | "isolated";
  /** Bound by the owner, never inferred from the caller's current selection. */
  owner_epoch: number;
  agents: Array<{ run_id: string; run_epoch: number }>;
  selection: "project" | "agent" | "group" | "change" | "conversation" | "account";
  home_id?: string;
  /** Existing delegated mailbox/control scope. Distinct from project identity. */
  communication_scope?: string;
};

type TaskArguments = { task: string; intent: "automatic" | "execute" | "discuss" | "plan_only" };
type MessageArguments = { message: string; ttl_ms?: number };
type ControlArguments = { reason: string; instruction?: string; expected_thread_id: string; expected_turn_id: string };
type ConversationArguments = { session_id: string; next_step: string; mode: "continue" | "copy" | "move" };
type ChangeArguments = { change_ids: string[]; authorization: "review" | "check" | "integrate" };
export type OperationArguments = {
  "surface.open.v1": Record<string, never>;
  "agent.list.v1": Record<string, never>;
  "agent.status.v1": Record<string, never>;
  "agent.logs.v1": Record<string, never>;
  "agent.watch.v1": Record<string, never>;
  "agent.attach.v1": Record<string, never>;
  "agent.report.v1": Record<string, never>;
  "agent.find.v1": Record<string, never>;
  "agent.receipt.v1": { command_id: string };
  "agent.topics.v1": { topics: string[] };
  "task.start.v1": TaskArguments;
  "agent.send.v1": MessageArguments;
  "agent.ask.v1": MessageArguments & { request_id: string };
  "agent.reply.v1": MessageArguments & { reply_to: string };
  "agent.inbox.v1": { limit?: number };
  "agent.ack.v1": { message_id: string };
  "agent.await.v1": { timeout_ms: number; command_id?: string };
  "agent.steer.v1": ControlArguments;
  "agent.interrupt.v1": ControlArguments;
  "agent.resume.v1": ControlArguments;
  "agent.checkpoint.v1": { name: string };
  "agent.continue.v1": ConversationArguments;
  "conversation.copy.v1": ConversationArguments;
  "conversation.move.v1": ConversationArguments;
  "agent.stop.v1": { reason: string };
  "agent.delete.v1": { force: boolean };
  "change.review.v1": ChangeArguments;
  "change.check.v1": ChangeArguments;
  "change.integrate.v1": ChangeArguments;
  "change.sync.v1": Record<string, never>;
  "change.cleanup.v1": { force: boolean };
  "change.undo.v1": { op_id?: string };
  "project.cloud.v1": { argv: string[] };
  "project.board.v1": { argv: string[] };
  "project.serve.v1": { argv: string[] };
  "project.restart.v1": Record<string, never>;
  "project.gc.v1": { dry_run: boolean };
  "auth.login.v1": Record<string, never>;
  "diagnostic.check.v1": Record<string, never>;
  "diagnostic.setup.v1": Record<string, never>;
  "diagnostic.mouse.v1": { argv: string[] };
  "diagnostic.feedback.v1": { argv: string[] };
  "diagnostic.context.v1": { argv: string[] };
  "diagnostic.improve.v1": { argv: string[] };
  "project.settings.v1": { setting: "model" | "effort" | "speed" | "notifications" | "sound" | "color" | "telemetry"; value: string };
  "project.files.v1": { workspace_id?: string; path?: string };
};
export type OperationId = keyof OperationArguments;
export type OperationRequest<K extends OperationId = OperationId> = K extends OperationId ? {
  format_version: 1;
  operation: K;
  idempotency_key: string;
  scope: ExecutionScope;
  arguments: OperationArguments[K];
} : never;

function requireCondition(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message);
}
function record(value: unknown): Record<string, unknown> {
  requireCondition(value !== null && typeof value === "object" && !Array.isArray(value), "Expected an object");
  return value as Record<string, unknown>;
}
function keys(value: Record<string, unknown>, allowed: readonly string[]): void {
  requireCondition(Object.keys(value).every((key) => allowed.includes(key)), "Unknown operation field");
}
function text(value: unknown, max = 65536): asserts value is string {
  requireCondition(typeof value === "string" && value.trim().length > 0 && Buffer.byteLength(value) <= max && !value.includes("\0"), "Invalid bounded text");
}
function epoch(value: unknown): void { requireCondition(Number.isSafeInteger(value) && Number(value) > 0, "Invalid owner or run epoch"); }

export function validateOperationRequest(input: unknown): OperationRequest {
  const request = record(input);
  keys(request, ["format_version", "operation", "idempotency_key", "scope", "arguments"]);
  requireCondition(request.format_version === 1, "Unsupported operation version");
  text(request.operation, 128);
  operationDefinition(request.operation);
  text(request.idempotency_key, 128);
  requireCondition(/^[A-Za-z0-9_.:-]+$/.test(request.idempotency_key), "Invalid idempotency key");
  const scope = record(request.scope);
  keys(scope, ["project_id", "location", "checkout", "owner_epoch", "agents", "selection", "home_id", "communication_scope"]);
  text(scope.project_id, 256);
  requireCondition(["local", "cloud"].includes(String(scope.location)) && ["main", "isolated"].includes(String(scope.checkout)), "Explicit location and checkout required");
  epoch(scope.owner_epoch);
  requireCondition(["project", "agent", "group", "change", "conversation", "account"].includes(String(scope.selection)), "Explicit selection required");
  if (scope.location === "cloud") text(scope.home_id, 256);
  else requireCondition(scope.home_id === undefined, "Local execution cannot select a Cloud home");
  if (scope.communication_scope !== undefined) text(scope.communication_scope, 256);
  requireCondition(Array.isArray(scope.agents) && scope.agents.length <= 64, "Bounded selected agents required");
  const ids = new Set<string>();
  for (const item of scope.agents) {
    const agent = record(item); keys(agent, ["run_id", "run_epoch"]); text(agent.run_id, 256); epoch(agent.run_epoch);
    requireCondition(!ids.has(agent.run_id), "Duplicate selected agent"); ids.add(agent.run_id);
  }
  requireCondition(scope.selection !== "agent" || ids.size === 1, "Agent action requires exactly one selected agent");
  requireCondition(scope.selection !== "group" || ids.size > 0, "Group action requires explicit selected agents");
  const args = record(request.arguments);
  const name = request.operation;
  if (["agent.send.v1", "agent.ask.v1", "agent.reply.v1"].includes(name)) {
    keys(args, ["message", "ttl_ms", ...(name === "agent.ask.v1" ? ["request_id"] : name === "agent.reply.v1" ? ["reply_to"] : [])]);
    text(args.message);
    if (args.ttl_ms !== undefined) requireCondition(Number.isSafeInteger(args.ttl_ms) && Number(args.ttl_ms) > 0 && Number(args.ttl_ms) <= 86400000, "Invalid message TTL");
    if (name === "agent.ask.v1") text(args.request_id, 256);
    if (name === "agent.reply.v1") text(args.reply_to, 256);
    requireCondition(["agent", "group"].includes(String(scope.selection)), "Messaging requires an addressed agent or explicit group");
    if (name === "agent.reply.v1") requireCondition(scope.selection === "agent", "Reply targets its original conversation");
    else text(scope.communication_scope, 256);
  } else if (["agent.steer.v1", "agent.interrupt.v1", "agent.resume.v1"].includes(name)) {
    keys(args, ["reason", "instruction", "expected_thread_id", "expected_turn_id"]);
    text(args.reason); text(args.expected_thread_id, 256); text(args.expected_turn_id, 256);
    if (args.instruction !== undefined) text(args.instruction);
    requireCondition(scope.selection === "agent", "Live control requires one exact agent attempt");
    text(scope.communication_scope, 256);
  } else if (name === "task.start.v1") {
    keys(args, ["task", "intent"]); text(args.task);
    requireCondition(["automatic", "execute", "discuss", "plan_only"].includes(String(args.intent)), "Explicit task intent required");
    requireCondition(scope.selection === "project" && ids.size === 0, "Task start requires project scope");
  } else if (["agent.continue.v1", "conversation.copy.v1", "conversation.move.v1"].includes(name)) {
    keys(args, ["session_id", "next_step", "mode"]); text(args.session_id, 256); text(args.next_step);
    const expected = name === "agent.continue.v1" ? "continue" : name === "conversation.copy.v1" ? "copy" : "move";
    requireCondition(args.mode === expected && scope.selection === "conversation", "Conversation mode must be explicit");
  } else if (["change.review.v1", "change.check.v1", "change.integrate.v1"].includes(name)) {
    keys(args, ["change_ids", "authorization"]);
    requireCondition(Array.isArray(args.change_ids) && args.change_ids.length > 0 && args.change_ids.length <= 64, "Selected changes required");
    for (const id of args.change_ids) text(id, 256);
    requireCondition(args.authorization === name.split(".")[1] && scope.selection === "change", "Change authorization differs");
  } else if (name === "project.settings.v1") {
    keys(args, ["setting", "value"]); text(args.value, 256);
    requireCondition(["model", "effort", "speed", "notifications", "sound", "color", "telemetry"].includes(String(args.setting)) && scope.selection === "project", "Unknown project setting");
  } else if (name === "project.files.v1") {
    keys(args, ["workspace_id", "path"]);
    if (args.workspace_id !== undefined) text(args.workspace_id, 256);
    if (args.path !== undefined) text(args.path, 4096);
    requireCondition(scope.selection === "project", "Project files require project scope");
  } else {
    const passthrough = ["project.cloud.v1", "project.board.v1", "project.serve.v1", "diagnostic.mouse.v1", "diagnostic.feedback.v1", "diagnostic.context.v1", "diagnostic.improve.v1"];
    const empty = ["surface.open.v1", "agent.list.v1", "agent.status.v1", "agent.logs.v1", "agent.watch.v1", "agent.attach.v1", "agent.report.v1", "agent.find.v1", "change.sync.v1", "project.restart.v1", "auth.login.v1", "diagnostic.check.v1", "diagnostic.setup.v1"];
    if (passthrough.includes(name)) {
      keys(args, ["argv"]);
      requireCondition(Array.isArray(args.argv) && args.argv.length <= 128, "Bounded adapter arguments required");
      for (const arg of args.argv) text(arg, 65536);
    } else if (empty.includes(name)) keys(args, []);
    else if (name === "project.gc.v1" || name === "change.cleanup.v1") {
      const flag = name === "project.gc.v1" ? "dry_run" : "force";
      keys(args, [flag]); requireCondition(typeof args[flag] === "boolean", "Explicit effect flag required");
    } else if (name === "change.undo.v1") { keys(args, ["op_id"]); if (args.op_id !== undefined) text(args.op_id, 256); }
    else if (name === "agent.receipt.v1") { keys(args, ["command_id"]); text(args.command_id, 256); }
    else if (name === "agent.topics.v1") {
      keys(args, ["topics"]); requireCondition(Array.isArray(args.topics) && args.topics.length <= 256, "Bounded topics required");
      for (const topic of args.topics) text(topic, 256);
    } else {
    const fields: Record<string, string[]> = {
      "agent.inbox.v1": ["limit"], "agent.ack.v1": ["message_id"], "agent.await.v1": ["timeout_ms", "command_id"],
      "agent.checkpoint.v1": ["name"], "agent.stop.v1": ["reason"], "agent.delete.v1": ["force"],
    };
    requireCondition(fields[name], "Operation requires its existing adapter schema"); keys(args, fields[name]!);
    requireCondition(scope.selection === "agent", "Agent action requires one exact agent attempt");
    if (name === "agent.ack.v1") text(args.message_id, 256);
    if (name === "agent.checkpoint.v1") text(args.name, 256);
    if (name === "agent.stop.v1") text(args.reason);
    if (name === "agent.delete.v1") requireCondition(typeof args.force === "boolean", "Explicit force flag required");
    if (args.limit !== undefined) requireCondition(Number.isSafeInteger(args.limit) && Number(args.limit) > 0 && Number(args.limit) <= 64, "Invalid inbox limit");
    if (name === "agent.await.v1") {
      requireCondition(Number.isSafeInteger(args.timeout_ms) && Number(args.timeout_ms) >= 0 && Number(args.timeout_ms) <= 30000, "Wait must be bounded to 30000 ms");
      if (args.command_id !== undefined) text(args.command_id, 256);
    }
    }
    const definition = operationDefinition(name);
    requireCondition(scope.selection === definition.scope || definition.scope === "execution", "Selection differs from operation scope");
  }
  requireCondition(Buffer.byteLength(JSON.stringify(input)) <= 128 * 1024, "Operation request exceeds bound");
  return input as OperationRequest;
}

function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value && typeof value === "object") return `{${Object.entries(value).filter(([, v]) => v !== undefined).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`).join(",")}}`;
  return JSON.stringify(value);
}
export function operationDigest(request: OperationRequest): string {
  validateOperationRequest(request);
  return createHash("sha256").update(canonical(request)).digest("hex");
}

export type OperationReceipt = {
  format_version: 1;
  operation: OperationId;
  idempotency_key: string;
  request_sha256: string;
  kind: string;
  state: "accepted" | "running" | "completed" | "rejected" | "effect_unknown";
  result: unknown;
};
export function validateOperationReceipt(request: OperationRequest, input: unknown): OperationReceipt {
  const receipt = record(input);
  keys(receipt, ["format_version", "operation", "idempotency_key", "request_sha256", "kind", "state", "result"]);
  requireCondition(receipt.format_version === 1 && receipt.operation === request.operation && receipt.idempotency_key === request.idempotency_key
    && receipt.request_sha256 === operationDigest(request) && receipt.kind === operationDefinition(request.operation).receipt, "Receipt differs from original operation and scope");
  requireCondition(["accepted", "running", "completed", "rejected", "effect_unknown"].includes(String(receipt.state)) && receipt.result !== undefined, "Invalid operation receipt");
  return input as OperationReceipt;
}

export type OperationAuthority = { capabilities: readonly string[]; scope: ExecutionScope };
export type TransactionalOperationHandler = (request: OperationRequest) => Promise<OperationReceipt>;

/** The handler owns durable idempotency, epoch checks and reconciliation. This
 * boundary never retries, converts an uncertain effect to success, or consults
 * a model to choose the meaning of stop/resume/review/settings. */
export async function dispatchOperation(input: unknown, authority: OperationAuthority, handler: TransactionalOperationHandler): Promise<OperationReceipt> {
  const request = structuredClone(validateOperationRequest(input));
  const definition = operationDefinition(request.operation);
  requireCondition(authority.capabilities.includes(definition.capability), "Operation capability was not delegated");
  requireCondition(canonical(authority.scope) === canonical(request.scope), "Operation differs from authenticated execution scope");
  const original = structuredClone(request);
  return validateOperationReceipt(original, await handler(request));
}

/** Validation adds no permissions. The receiving engine must authenticate its
 * caller and use its existing transactional command receipt and epoch checks.
 * Ordinary send maps ONLY to agents_send, never resume or broadcast recovery. */
export function agentToolCalls(request: OperationRequest): Array<{ tool: string; arguments: Record<string, unknown> }> {
  validateOperationRequest(request);
  const definition = operationDefinition(request.operation);
  requireCondition("tool" in definition && definition.tool, "Operation has no agent tool adapter");
  const tool = definition.tool;
  const args = request.arguments as Record<string, unknown>;
  return request.scope.agents.map((agent) => {
    const key = request.scope.selection === "group" ? `${request.idempotency_key}:${agent.run_id}` : request.idempotency_key;
    requireCondition(Buffer.byteLength(key) <= 128, "Per-agent idempotency key exceeds bound");
    const scope = request.scope.communication_scope;
    if (tool === "agents_send" || tool === "agents_ask") return { tool, arguments: { ...args, idempotency_key: key, recipient_run: agent.run_id, recipient_epoch: agent.run_epoch, scope } };
    if (["agents_steer", "agents_interrupt", "agents_resume"].includes(tool)) return { tool, arguments: { ...args, idempotency_key: key, target_run: agent.run_id, target_epoch: agent.run_epoch, scope } };
    if (tool === "agents_reply") return { tool, arguments: { ...args, idempotency_key: key } };
    return { tool, arguments: { ...args } };
  });
}
