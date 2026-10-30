/** The managed Claude profile uses the public SDK's typed stream-json protocol.
 * Runtime imports of the SDK are deliberately unnecessary: the installed CLI
 * owns the actual session, and the engine owns this process attempt.
 */
import { createHash, randomUUID } from "node:crypto";
import { performance } from "node:perf_hooks";
import type { SDKControlRequest, SDKControlResponse, SDKControlInterruptResponse, SDKUserMessage, SDKMessageOrigin } from "@anthropic-ai/claude-agent-sdk";

export const SDK_PROFILE = "claude-sdk";
export const SDK_FRAME_LIMIT = 1024 * 1024;
export const SDK_TEXT_LIMIT = 64 * 1024;
export const SDK_REQUIRED_CAPABILITIES = ["interrupt_receipt_v1", "interrupt_cancel_queued_v1"] as const;

type Json = Record<string, unknown>;
export type SdkPeerMessage = { sender_run_id: string; message_id: string; text: string };
export type SdkOperation = { kind: "deliver" | "steer" | "resume"; message: SdkPeerMessage } | { kind: "interrupt" };
export type SdkCommand = { run_id: string; attempt_id: string; command_id: string; expected_turn_id: string | null; operation: SdkOperation; deadline_unix_ms?: number };
export type SdkWireResult = { turnId?: string; replacedTurnId?: string; turn?: { id: string; status: "inProgress" }; rudderUncertain?: string };
export type SdkLifecycle = { method: "turn/started" | "turn/completed"; params: { threadId: string; turn: { id: string; status: "inProgress" | "completed" | "failed" | "interrupted" | "idle" } } }
  | { method: "rudder/profile"; params: { threadId: string | null; qualified: boolean; uncertain: boolean } };
export type SdkSnapshot = { sessionId: string | null; activeTurnId: string | null; lastInterruptedTurnId: string | null; qualified: boolean; uncertain: boolean; capabilities: string[]; cliVersion: string | null; steerMode: "interrupt_then_replace" };

export class SdkControlError extends Error {
  constructor(readonly kind: "rejected" | "unsupported" | "uncertain", message: string) { super(message); this.name = "SdkControlError"; }
}

export function object(value: unknown): value is Json { return !!value && typeof value === "object" && !Array.isArray(value); }
export function id(value: unknown): value is string { return typeof value === "string" && value.length > 0 && value.length <= 256 && !/[\x00-\x1f\x7f]/.test(value); }
export function text(value: unknown): value is string { return typeof value === "string" && value.length > 0 && Buffer.byteLength(value) <= SDK_TEXT_LIMIT; }
function exactKeys(value: Json, allowed: string[]): boolean { return Object.keys(value).every((key) => allowed.includes(key)); }
export function parseSdkCommand(value: unknown): SdkCommand {
  if (!object(value) || !exactKeys(value, ["run_id", "attempt_id", "command_id", "expected_turn_id", "operation", "deadline_unix_ms"])
      || !id(value.run_id) || !id(value.attempt_id) || !id(value.command_id)
      || !(value.expected_turn_id === null || id(value.expected_turn_id)) || !object(value.operation)
      || !(value.deadline_unix_ms === undefined || Number.isSafeInteger(value.deadline_unix_ms) && Number(value.deadline_unix_ms) > 0)) {
    throw new SdkControlError("rejected", "Invalid managed Claude command envelope");
  }
  const op = value.operation;
  if (op.kind === "interrupt" && exactKeys(op, ["kind"])) return value as SdkCommand;
  if (!["deliver", "steer", "resume"].includes(String(op.kind)) || !exactKeys(op, ["kind", "message"]) || !object(op.message)
      || !exactKeys(op.message, ["sender_run_id", "message_id", "text"]) || !id(op.message.sender_run_id) || !id(op.message.message_id) || !text(op.message.text)) {
    throw new SdkControlError("rejected", "Invalid attributed peer input");
  }
  return value as SdkCommand;
}

function strings(value: unknown): value is string[] { return Array.isArray(value) && value.length <= 1024 && value.every(id); }
function nonHumanOriginVersion(version: string | null): boolean {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(version ?? "");
  if (!match) return false;
  const [major, minor, patch] = match.slice(1).map(Number);
  return major > 2 || major === 2 && (minor > 1 || minor === 1 && patch >= 223);
}
function uuid(): NonNullable<SDKUserMessage["uuid"]> { return randomUUID(); }
type Input = SDKUserMessage | SDKControlRequest | SDKControlResponse;
type Deferred<T> = { promise: Promise<T>; resolve: (value: T) => void; reject: (error: Error) => void; timer: NodeJS.Timeout };
function deferred<T>(timeout: number, message: string): Deferred<T> {
  let resolve!: (value: T) => void, reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  // A transport can fail synchronously before a caller reaches its await.
  // Keep the original promise rejecting for callers without an unhandled timer.
  void promise.catch(() => {});
  const timer = setTimeout(() => reject(new SdkControlError("uncertain", message)), timeout);
  return { promise, resolve(value) { clearTimeout(timer); resolve(value); }, reject(error) { clearTimeout(timer); reject(error); }, timer };
}

type ActiveTurn = { id: string; inputs: Set<string>; started: boolean; origin: "human" | "peer" };
type InputWait = { turnId: string; wait: Deferred<void> };
type InterruptWait = { requestId: string; turnId: string; replacing: boolean; timedOut: boolean; receipt: SDKControlInterruptResponse | null; result: { status: "completed" | "failed" | "interrupted" | "idle" } | null; wait: Deferred<void> };

/** One backend protocol actor, with no run/workspace scheduling authority. */
export class ClaudeSdkProtocol {
  private sessionId: string | null = null;
  private cliVersion: string | null = null;
  private capabilities = new Set<string>();
  private active: ActiveTurn | null = null;
  private lastInterrupted: string | null = null;
  private lastTurn: { id: string; status: string } | null = null;
  private uncertain = false;
  private hardUncertain = false;
  private closed = false;
  private initialization: { id: string; wait: Deferred<void> } | null = null;
  private initialized = false;
  private negotiated = false;
  private readonly sessionHint: boolean;
  private inputs = new Map<string, InputWait>();
  private interruption: InterruptWait | null = null;
  private busy = false;
  private commands = new Map<string, { fingerprint: string; promise: Promise<SdkWireResult> }>();
  private permissions = new Map<string, Json>();
  private humanQueue: string[] = [];

  constructor(readonly runId: string, readonly attemptId: string, readonly cwd: string,
      private readonly send: (frame: Input) => void,
      private readonly event: (frame: SdkLifecycle) => void,
      private readonly notice: (message: string) => void,
      private readonly timeoutMs = 30_000, selectedSession: string | null = null) {
    if (!id(runId) || !id(attemptId) || timeoutMs < 10 || timeoutMs > 120_000) throw new Error("Invalid managed Claude protocol options");
    if (selectedSession !== null && !id(selectedSession)) throw new Error("Invalid managed Claude session selection");
    this.sessionId = selectedSession; this.sessionHint = selectedSession !== null;
  }

  snapshot(): SdkSnapshot {
    return { sessionId: this.sessionId, activeTurnId: this.active?.id ?? null, lastInterruptedTurnId: this.lastInterrupted,
      qualified: !!this.sessionId && nonHumanOriginVersion(this.cliVersion) && SDK_REQUIRED_CAPABILITIES.every((capability) => this.capabilities.has(capability)), uncertain: this.uncertain,
      capabilities: [...this.capabilities], cliVersion: this.cliVersion, steerMode: "interrupt_then_replace" };
  }

  thread(): Json | null {
    if (!this.sessionId || !(this.snapshot().qualified || this.canBootstrap())) return null;
    const last = this.active ? { id: this.active.id, status: "inProgress", items: [] } : this.lastTurn ? { ...this.lastTurn, items: [] } : null;
    return { id: this.sessionId, sessionId: this.sessionId, cwd: this.cwd, source: "cli", ephemeral: false, turns: last ? [last] : [], status: { type: this.active ? "active" : "idle" },
      rudder_profile_qualified: this.snapshot().qualified, rudder_binding_observed: this.negotiated, rudder_capabilities: [...this.capabilities],
      rudder_uncertain: this.uncertain, rudder_cli_version: this.cliVersion };
  }

  private canBootstrap(): boolean { return this.initialized && this.sessionHint && !this.negotiated && nonHumanOriginVersion(this.cliVersion); }

  async initialize(): Promise<void> {
    if (this.initialization) throw new Error("Claude stream was already initialized");
    const wait = deferred<void>(this.timeoutMs, "Claude SDK initialization timed out");
    this.initialization = { id: randomUUID(), wait };
    this.write({ type: "control_request", request_id: this.initialization.id, request: { subtype: "initialize" } });
    await wait.promise;
  }

  private write(frame: Input): void {
    if (this.closed) throw new SdkControlError("rejected", "Claude attempt has closed");
    try { this.send(frame); } catch {
      this.failTransport("Claude stdin write may have failed; do not replay");
      throw new SdkControlError("uncertain", "Claude stdin write may have failed; do not replay");
    }
  }

  /** Only the direct keyboard path calls this. Peer RPC payloads cannot select
   * human/coordinator origin or provide permission responses. */
  async humanInput(body: string): Promise<void> {
    if (!text(body)) throw new SdkControlError("rejected", "Input must contain 1..=65536 bytes");
    if (this.closed || this.uncertain) throw new SdkControlError("uncertain", "Claude control state requires reconciliation");
    if (this.active || this.busy) {
      if (this.humanQueue.length >= 16) throw new SdkControlError("rejected", "Human input queue is full");
      this.humanQueue.push(body); this.notice("Input held until the current turn finishes."); return;
    }
    await this.submitInput(body, { kind: "human" });
  }

  private async submitInput(body: string, origin: SDKMessageOrigin, budgetMs = this.timeoutMs): Promise<string> {
    if (this.active) throw new SdkControlError("rejected", "Claude already has an active turn");
    if (budgetMs <= 0) throw new SdkControlError("uncertain", "Claude control expired before input submission; no new turn was submitted");
    const inputId = uuid();
    const wait = deferred<void>(budgetMs, "Claude input acceptance is unknown; do not replay");
    this.active = { id: inputId, inputs: new Set([inputId]), started: false, origin: origin.kind === "peer" ? "peer" : "human" };
    this.lastInterrupted = null;
    this.inputs.set(inputId, { turnId: inputId, wait });
    const frame: SDKUserMessage = { type: "user", uuid: inputId, message: { role: "user", content: body }, parent_tool_use_id: null, origin,
      ...(origin.kind === "peer" ? { isSynthetic: true, priority: "next" as const } : {}), ...(this.sessionId ? { session_id: this.sessionId } : {}) };
    try {
      this.write(frame);
      await wait.promise;
      return inputId;
    } catch (error) {
      this.uncertain = true;
      throw error;
    }
  }

  execute(raw: unknown, threadId: string): Promise<SdkWireResult> {
    let command: SdkCommand;
    try { command = parseSdkCommand(raw); } catch (error) { return Promise.reject(error); }
    const fingerprint = createHash("sha256").update(JSON.stringify({ threadId, command })).digest("hex");
    const prior = this.commands.get(command.command_id);
    if (prior) {
      if (prior.fingerprint !== fingerprint) return Promise.reject(new SdkControlError("rejected", "Command id reused with different content"));
      return prior.promise;
    }
    if (this.commands.size >= 4096) return Promise.reject(new SdkControlError("rejected", "Attempt replay table is full"));
    const promise = this.perform(command, threadId);
    this.commands.set(command.command_id, { fingerprint, promise });
    return promise;
  }

  private async perform(command: SdkCommand, threadId: string): Promise<SdkWireResult> {
    if (command.run_id !== this.runId || command.attempt_id !== this.attemptId || threadId !== this.sessionId) throw new SdkControlError("rejected", "Stale Claude attempt or session");
    const firstIdleDelivery = this.canBootstrap() && !this.active && command.expected_turn_id === null && command.operation.kind === "deliver";
    if (!this.snapshot().qualified && !firstIdleDelivery) throw new SdkControlError("unsupported", "Claude CLI has not advertised the required interrupt capabilities");
    if (this.closed) throw new SdkControlError("rejected", "Claude attempt has closed");
    if (this.uncertain) throw new SdkControlError("uncertain", "A previous Claude control has an unresolved outcome");
    if (this.busy) throw new SdkControlError("rejected", "Another Claude control is in flight");
    const budget = command.deadline_unix_ms === undefined ? this.timeoutMs : command.deadline_unix_ms - Date.now();
    if (budget <= 0) throw new SdkControlError("rejected", "Claude control expired before dispatch");
    if (budget > 120_000) throw new SdkControlError("rejected", "Claude control deadline exceeds 120 seconds");
    const deadline = performance.now() + budget;
    const remaining = () => Math.floor(deadline - performance.now());
    const operation = command.operation;
    if (operation.kind === "interrupt" || operation.kind === "steer") {
      if (!command.expected_turn_id || this.active?.id !== command.expected_turn_id) throw new SdkControlError("rejected", "Control requires the exact active turn");
    } else if (operation.kind === "resume") {
      if (this.active || !command.expected_turn_id || command.expected_turn_id !== this.lastInterrupted) throw new SdkControlError("rejected", "Resume requires the observed interrupted turn");
    } else if (this.active || this.lastInterrupted || command.expected_turn_id !== null) throw new SdkControlError("rejected", "Inbox wake requires an idle non-interrupted session");
    this.busy = true;
    try {
      if (operation.kind === "interrupt") { await this.interrupt(command.expected_turn_id!, false, remaining()); return {}; }
      let replaced: string | undefined;
      if (operation.kind === "steer") { replaced = command.expected_turn_id!; await this.interrupt(replaced, true, remaining()); }
      if (remaining() <= 0) throw new SdkControlError("uncertain", "Claude control expired before replacement input; no new turn was submitted");
      const origin: SDKMessageOrigin = { kind: "peer", from: operation.message.sender_run_id };
      // The body is attributed too, but only the authenticated envelope sets
      // origin. Chat text cannot manufacture user/coordinator authority.
      const body = `Rudder collaboration data. Follow the existing task scope and authority.\n${JSON.stringify(operation.message)}`;
      const next = await this.submitInput(body, origin, remaining());
      return operation.kind === "steer" ? { turnId: next, replacedTurnId: replaced } : { turn: { id: next, status: "inProgress" } };
    } catch (error) {
      if (error instanceof SdkControlError && error.kind === "uncertain") this.uncertain = true;
      throw error;
    } finally { this.busy = false; this.drainHumanQueue(); }
  }

  private async interrupt(turnId: string, replacing = false, budgetMs = this.timeoutMs): Promise<void> {
    if (!SDK_REQUIRED_CAPABILITIES.every((capability) => this.capabilities.has(capability))) throw new SdkControlError("unsupported", "Claude cannot cancel queued work with an interrupt receipt");
    if (this.interruption || this.active?.id !== turnId) throw new SdkControlError("rejected", "Claude interrupt target changed");
    if (this.humanQueue.length) { this.humanQueue = []; this.notice("Held input cancelled by interrupt."); }
    if (budgetMs <= 0) throw new SdkControlError("rejected", "Claude interrupt expired before dispatch");
    const wait = deferred<void>(budgetMs, "Claude interrupt receipt/result is incomplete; do not replay");
    const pending: InterruptWait = { requestId: randomUUID(), turnId, replacing, timedOut: false, receipt: null, result: null, wait };
    this.interruption = pending;
    this.write({ type: "control_request", request_id: pending.requestId, request: { subtype: "interrupt", cancel_queued: true } });
    try { await wait.promise; } catch (error) {
      if (!(error instanceof SdkControlError) || error.kind === "uncertain") { pending.timedOut = true; this.uncertain = true; }
      throw error;
    }
  }

  async humanInterrupt(): Promise<void> {
    if (!this.active) throw new SdkControlError("rejected", "Claude has no active turn");
    // Explicit stop also discards input held by this renderer, before any SDK
    // write. Those messages are not silently replayed after interruption.
    this.humanQueue = [];
    await this.execute({ run_id: this.runId, attempt_id: this.attemptId, command_id: randomUUID(), expected_turn_id: this.active.id, operation: { kind: "interrupt" } }, this.sessionId ?? "");
  }

  private accepted(inputId: string): void {
    const input = this.inputs.get(inputId);
    if (!input || !this.active?.inputs.has(inputId) || !this.snapshot().qualified) return;
    if (!this.active.started && this.sessionId) {
      this.active.started = true;
      this.event({ method: "turn/started", params: { threadId: this.sessionId, turn: { id: this.active.id, status: "inProgress" } } });
    }
    this.inputs.delete(inputId);
    input.wait.resolve();
  }

  private correlate(frame: Json): string[] {
    const values = Array.isArray(frame.user_message_uuids) ? [...frame.user_message_uuids] : [];
    if (typeof frame.user_message_uuid === "string") values.push(frame.user_message_uuid);
    return [...new Set(values.filter((value): value is string => id(value)))];
  }

  ingest(raw: unknown): void {
    if (!object(raw) || typeof raw.type !== "string" || this.closed) return;
    if (raw.type === "control_response") { this.controlResponse(raw); return; }
    if (raw.type === "control_request") { this.permissionRequest(raw); return; }
    if (raw.type === "control_cancel_request" && id(raw.request_id)) { this.permissions.delete(raw.request_id); return; }
    if (raw.type === "system" && raw.subtype === "init") {
      if (!id(raw.session_id) || !strings(raw.capabilities) || typeof raw.claude_code_version !== "string") {
        this.failTransport("Claude initialization does not expose supported capability evidence"); return;
      }
      if (this.sessionId && this.sessionId !== raw.session_id) { this.failTransport("Claude session identity changed"); return; }
      this.sessionId = raw.session_id; this.cliVersion = raw.claude_code_version; this.capabilities = new Set(raw.capabilities);
      this.negotiated = true;
      if (!this.snapshot().qualified) this.failTransport("Claude CLI lacks interrupt_receipt_v1 and interrupt_cancel_queued_v1");
      else this.event({ method: "rudder/profile", params: { threadId: this.sessionId, qualified: true, uncertain: this.uncertain } });
      return;
    }
    if (!this.sessionId || raw.session_id !== this.sessionId || raw.parent_tool_use_id != null) return;
    if (raw.type === "user" && raw.isReplay === true && id(raw.uuid)) this.accepted(raw.uuid);
    const correlation = this.correlate(raw);
    if (raw.type === "assistant" || raw.type === "stream_event" || raw.type === "result") for (const input of correlation) this.accepted(input);
    if (raw.type !== "result" || !this.active || !correlation.some((input) => this.active!.inputs.has(input))) return;
    if (typeof raw.is_error !== "boolean" || !["success", "error_during_execution", "error_max_turns", "error_max_budget_usd", "error_max_structured_output_retries"].includes(String(raw.subtype))) {
      this.failTransport("Claude result has an unsupported lifecycle schema"); return;
    }
    if (this.active.origin === "peer" && (!object(raw.origin) || !["peer", "unclassified"].includes(String(raw.origin.kind)))) {
      this.failTransport("Claude did not preserve a non-human source for the peer turn"); return;
    }
    const turnId = this.active.id;
    const terminal = raw.terminal_reason;
    let status: "completed" | "failed" | "interrupted" | "idle";
    if (terminal === "aborted_streaming" || terminal === "aborted_tools") status = "interrupted";
    else if (raw.is_error === true || String(raw.subtype).startsWith("error")) status = "failed";
    else if (["stop_hook_prevented", "hook_stopped", "tool_deferred", "background_requested", "tool_deferred_unavailable"].includes(String(terminal))) status = "idle";
    else if (["blocking_limit", "rapid_refill_breaker", "prompt_too_long", "image_error", "model_error", "api_error", "malformed_tool_use_exhausted", "max_turns", "budget_exhausted", "structured_output_retry_exhausted", "turn_setup_failed"].includes(String(terminal))) status = "failed";
    else if (terminal === "completed") status = "completed";
    else if (terminal == null) status = raw.num_turns === 0 ? "idle" : "completed"; // supported legacy SDK success results
    else { this.failTransport("Claude reported an unknown terminal reason; completion is unproven"); return; }
    if (this.interruption?.turnId === turnId) {
      this.interruption.result = { status };
      this.finishInterrupt();
      return;
    }
    this.finishTurn(turnId, status);
    this.drainHumanQueue();
  }

  private controlResponse(frame: Json): void {
    if (!object(frame.response) || !id(frame.response.request_id)) return;
    const response = frame.response;
    const initialization = this.initialization;
    if (initialization && initialization.id === response.request_id) {
      if (response.subtype === "success" && object(response.response)) {
        this.initialized = true;
        if (typeof response.response.claude_code_version === "string") this.cliVersion = response.response.claude_code_version;
        initialization.wait.resolve();
      }
      else initialization.wait.reject(new SdkControlError("rejected", "Claude initialization was rejected"));
      return;
    }
    const pending = this.interruption;
    if (!pending || response.request_id !== pending.requestId) return;
    if (response.subtype === "error") {
      this.interruption = null;
      pending.wait.reject(new SdkControlError("rejected", "Claude rejected the interrupt request"));
      if (pending.result) this.finishTurn(pending.turnId, pending.result.status);
      return;
    }
    const receipt = response.response;
    if (response.subtype !== "success" || !object(receipt) || !strings(receipt.still_queued) || !(receipt.cancelled === undefined || strings(receipt.cancelled))) {
      this.failTransport("Claude interrupt receipt does not prove queue cancellation"); return;
    }
    if (receipt.still_queued.length > 0) { this.failTransport("Claude retained queued work after cancel_queued interrupt"); return; }
    pending.receipt = receipt as SDKControlInterruptResponse;
    this.finishInterrupt();
  }

  private finishInterrupt(): void {
    const pending = this.interruption;
    if (!pending?.receipt || !pending.result) return;
    this.interruption = null;
    this.finishTurn(pending.turnId, pending.result.status);
    // A late exact receipt + aborted result resolves a simple interrupt's
    // uncertainty. A timed-out replacement is never sent automatically.
    if (pending.timedOut && !pending.replacing && !this.hardUncertain && pending.result.status === "interrupted") {
      this.uncertain = false;
      this.event({ method: "rudder/profile", params: { threadId: this.sessionId, qualified: this.snapshot().qualified, uncertain: false } });
    }
    if (pending.result.status === "interrupted") pending.wait.resolve();
    else pending.wait.reject(new SdkControlError("rejected", `Intended turn ended as ${pending.result.status}, not interrupted`));
  }

  private finishTurn(turnId: string, status: "completed" | "failed" | "interrupted" | "idle"): void {
    if (this.active?.id !== turnId || !this.sessionId) return;
    this.active = null;
    this.permissions.clear();
    this.lastInterrupted = status === "interrupted" ? turnId : null;
    this.lastTurn = { id: turnId, status };
    this.event({ method: "turn/completed", params: { threadId: this.sessionId, turn: { id: turnId, status } } });
  }

  private drainHumanQueue(): void {
    if (this.active || this.busy || this.closed || this.uncertain || this.lastInterrupted || !this.humanQueue.length) return;
    const next = this.humanQueue.shift()!;
    void this.humanInput(next).catch((error: Error) => this.notice(error.message));
  }

  private permissionRequest(frame: Json): void {
    if (!id(frame.request_id) || !object(frame.request)) return;
    if (frame.request.subtype !== "can_use_tool" || this.permissions.size >= 16) {
      this.write({ type: "control_response", response: { subtype: "error", request_id: frame.request_id, error: "This managed profile cannot answer this control request" } }); return;
    }
    this.permissions.set(frame.request_id, frame.request);
    this.notice(`Permission needed: ${String(frame.request.tool_name).slice(0, 128)} ${JSON.stringify(frame.request.input ?? {}).slice(0, 2048)}\n/approve ${frame.request_id} or /deny ${frame.request_id}`);
  }

  answerPermission(requestId: string, allow: boolean): void {
    const pending = this.permissions.get(requestId);
    if (!pending) throw new SdkControlError("rejected", "Permission request is no longer pending");
    this.permissions.delete(requestId);
    this.write({ type: "control_response", response: { subtype: "success", request_id: requestId,
      response: allow ? { behavior: "allow", updatedInput: pending.input } : { behavior: "deny", message: "Denied by the user" } } });
  }

  failTransport(reason: string): void {
    this.uncertain = true; this.hardUncertain = true;
    this.event({ method: "rudder/profile", params: { threadId: this.sessionId, qualified: false, uncertain: true } });
    this.initialization?.wait.reject(new SdkControlError("uncertain", reason));
    for (const input of this.inputs.values()) input.wait.reject(new SdkControlError("uncertain", reason));
    this.inputs.clear();
    this.interruption?.wait.reject(new SdkControlError("uncertain", reason));
    this.notice(reason);
  }

  close(): void {
    this.closed = true; this.humanQueue = []; this.permissions.clear();
    if (this.inputs.size || this.interruption) this.failTransport("Claude attempt closed; unresolved controls must not be replayed");
    else this.initialization?.wait.reject(new SdkControlError("rejected", "Claude session closed"));
  }
}
