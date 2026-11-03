/** Non-secret backend session receipts. These are audit data, never launch,
 * credential, verification or integration authority. The trusted owner records
 * one only after receiving the actual backend session-start response. */
import { inspectManagedTaskProfile, type ManagedTaskProfileMetadata } from "../managed-task-profile.js";
import { normalizeEffortForBackend } from "../effort.js";
import type { EffortLevel } from "../types.js";
import { updateJson } from "../util.js";

export type ModelCapability = {
  model: string; resolved_model?: string; efforts: EffortLevel[]; default_effort?: EffortLevel;
};
export type ModelProfileSelectionV1 = {
  format_version: 1; backend: "codex" | "claude"; profile_id: string;
  provider: ManagedTaskProfileMetadata["provider"]; credential_kind: ManagedTaskProfileMetadata["credentialKind"];
  requested_model: string; model: string; requested_effort: EffortLevel | null;
  effort: EffortLevel | null; backend_version: string;
};
/** Same execution identity as engine ExecutionIdentityV2. No new scheduler identity. */
export type ModelExecutionIdentity = {
  project_id: string; request_id: string; run_id: string; run_epoch: number;
  execution_id: string; operation_id: string; view_id: string;
  owner: { owner: string; epoch: number }; request_digest: string; supervisor_pid: number;
};
export type ModelSessionReceiptV1 = {
  format_version: 1; selection: ModelProfileSelectionV1; execution: ModelExecutionIdentity;
  session_id: string; observed_model: string; observed_effort: EffortLevel | null;
};
const efforts = new Set(["low", "medium", "high", "xhigh", "max"]);
const validEffort = (value: unknown): value is EffortLevel => typeof value === "string" && efforts.has(value);
function text(value: unknown, max = 256): asserts value is string {
  if (typeof value !== "string" || !value.trim() || value.trim() !== value
    || Buffer.byteLength(value) > max || /[\p{Cc}\uD800-\uDFFF]/u.test(value)) throw new Error("Invalid model session identity.");
}
function keys(value: object, allowed: string[]): void {
  if (Object.keys(value).length !== allowed.length || Object.keys(value).some(key => !allowed.includes(key)))
    throw new Error("Unexpected model session receipt fields.");
}

/** Parse model/list from the installed Codex app-server, not a generic catalog.
 * The caller owns authentication, transport, timeout and process reap. */
export function codexProtocolCapabilities(result: unknown): ModelCapability[] {
  if (!result || typeof result !== "object" || !Array.isArray((result as { data?: unknown }).data))
    throw new Error("Invalid Codex model/list response.");
  const rows = (result as { data: Record<string, unknown>[] }).data;
  return rows.filter(row => row && !row.hidden).map(row => {
    text(row.model);
    if (!Array.isArray(row.supportedReasoningEfforts)) throw new Error("Missing Codex effort capabilities.");
    const supported = row.supportedReasoningEfforts.flatMap(value => {
      const effort = value && typeof value === "object" ? (value as Record<string, unknown>).reasoningEffort : undefined;
      text(effort);
      // Codex also advertises none/minimal and may add new levels. Rudder
      // exposes only its supported dial; never map an unknown level to high.
      return validEffort(effort) ? [effort] : [];
    });
    const defaultEffort = row.defaultReasoningEffort;
    if (defaultEffort !== undefined && typeof defaultEffort !== "string"
      || validEffort(defaultEffort) && !supported.includes(defaultEffort))
      throw new Error("Invalid Codex default effort.");
    return { model: row.model, efforts: supported, default_effort: validEffort(defaultEffort) ? defaultEffort : undefined };
  });
}

/** Parse supportedModels() from the installed Claude Agent SDK Query. Alias
 * resolution is retained so resume cannot silently follow an alias change. */
export function claudeProtocolCapabilities(result: unknown): ModelCapability[] {
  if (!Array.isArray(result)) throw new Error("Invalid Claude supportedModels response.");
  return result.map(row => {
    if (!row || typeof row !== "object") throw new Error("Invalid Claude model capability.");
    text(row.value); if (row.resolvedModel !== undefined) text(row.resolvedModel);
    const supported: unknown = row.supportedEffortLevels ?? [];
    if (!Array.isArray(supported) || !supported.every(validEffort)
      || row.supportsEffort === false && supported.length) throw new Error("Invalid Claude effort capabilities.");
    return { model: row.value, resolved_model: row.resolvedModel, efforts: supported as EffortLevel[] };
  });
}

/** Resolve exactly one selected profile. No provider request, auth import,
 * refresh, alternate-profile fallback or key handoff occurs here. */
export async function prepareSelectedModelProfile(input: {
  profileId: string; backend: "codex" | "claude"; model: string; effort?: EffortLevel;
  backendVersion: string;
}, capabilities: readonly ModelCapability[]): Promise<ModelProfileSelectionV1> {
  text(input.model); text(input.backendVersion); text(input.profileId);
  const capability = capabilities.find(row => row.model === input.model);
  if (!capability) throw new Error("Selected model is unavailable in the backend protocol.");
  const effort = normalizeEffortForBackend(input.backend, input.effort)
    ?? (input.backend === "codex" ? "high" : undefined);
  if (effort && !capability.efforts.includes(effort)) throw new Error("Selected effort is unsupported by this model.");
  const inspected = await inspectManagedTaskProfile({ profileId: input.profileId, backend: input.backend });
  if (!inspected.ok) throw new Error(inspected.error.message);
  const result: ModelProfileSelectionV1 = {
    format_version: 1, backend: input.backend, profile_id: input.profileId,
    provider: inspected.metadata.provider, credential_kind: inspected.metadata.credentialKind,
    requested_model: input.model, model: capability.resolved_model ?? input.model,
    requested_effort: input.effort ?? null, effort: effort ?? null, backend_version: input.backendVersion,
  };
  validateModelProfile(result); return result;
}

export function validateModelProfile(value: ModelProfileSelectionV1): void {
  if (!value || value.format_version !== 1 || !["codex", "claude"].includes(value.backend)) throw new Error("Invalid model profile version or backend.");
  keys(value, ["format_version", "backend", "profile_id", "provider", "credential_kind", "requested_model", "model", "requested_effort", "effort", "backend_version"]);
  for (const field of [value.profile_id, value.requested_model, value.model, value.backend_version]) text(field);
  if (!(["api_key", "oauth", "token"] as unknown[]).includes(value.credential_kind)
    || (value.backend === "claude" ? value.provider !== "anthropic"
      : value.provider !== "openai" && !(value.provider === "openai-codex" && value.credential_kind === "oauth")))
    throw new Error("Model profile provider differs from backend.");
  if (value.requested_effort !== null && !validEffort(value.requested_effort)
    || value.effort !== null && !validEffort(value.effort)) throw new Error("Invalid model effort.");
  const effective = normalizeEffortForBackend(value.backend, value.requested_effort ?? undefined)
    ?? (value.backend === "codex" ? "high" : undefined);
  if ((effective ?? null) !== value.effort) throw new Error("Model effort differs from selected launch policy.");
}

export function bindModelSession(selection: ModelProfileSelectionV1, execution: ModelExecutionIdentity,
  observed: { session_id: string; model: string; effort?: EffortLevel }): ModelSessionReceiptV1 {
  validateModelProfile(selection); text(observed.session_id); text(observed.model);
  keys(execution, ["project_id", "request_id", "run_id", "run_epoch", "execution_id", "operation_id", "view_id", "owner", "request_digest", "supervisor_pid"]);
  keys(execution.owner, ["owner", "epoch"]);
  for (const field of [execution.project_id, execution.run_id, execution.owner.owner]) text(field);
  for (const id of [execution.request_id, execution.execution_id, execution.operation_id, execution.view_id]) {
    if (typeof id !== "string" || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(id))
      throw new Error("Invalid execution UUID.");
  }
  for (const number of [execution.run_epoch, execution.owner.epoch, execution.supervisor_pid]) {
    if (!Number.isSafeInteger(number) || number <= 0) throw new Error("Invalid execution epoch or supervisor.");
  }
  if (!/^[0-9a-f]{64}$/.test(execution.request_digest)) throw new Error("Invalid execution request digest.");
  if (observed.model !== selection.model || observed.effort !== undefined && observed.effort !== selection.effort)
    throw new Error("Backend session differs from the selected model or effort.");
  // Own a snapshot: later mutation of launch objects cannot rewrite the receipt.
  return structuredClone({ format_version: 1, selection, execution, session_id: observed.session_id,
    observed_model: observed.model, observed_effort: observed.effort ?? null });
}

export function assertResumeModelSession(receipt: ModelSessionReceiptV1, selected: ModelProfileSelectionV1,
  execution: ModelExecutionIdentity, sessionId: string): void {
  validateModelProfile(selected);
  if (receipt.format_version !== 1) throw new Error("Unsupported model session receipt.");
  keys(receipt, ["format_version", "selection", "execution", "session_id", "observed_model", "observed_effort"]);
  const checked = bindModelSession(receipt.selection, receipt.execution, { session_id: receipt.session_id,
    model: receipt.observed_model, effort: receipt.observed_effort ?? undefined });
  const same = <T>(left: T, right: T) => JSON.stringify(sorted(left)) === JSON.stringify(sorted(right));
  if (!same(checked.selection, selected) || !same(checked.execution, execution) || receipt.session_id !== sessionId)
    throw new Error("Resume model, private profile, session or execution owner changed.");
}
function sorted(value: unknown): unknown {
  if (!value || typeof value !== "object" || Array.isArray(value)) return value;
  return Object.fromEntries(Object.entries(value).sort(([a], [b]) => a.localeCompare(b)).map(([k, v]) => [k, sorted(v)]));
}
/** Atomically persist once. An uncertain session-start must be reconciled with
 * the backend; it must never overwrite the old receipt or launch a replacement. */
export async function saveModelSessionReceipt(file: string, receipt: ModelSessionReceiptV1): Promise<void> {
  assertResumeModelSession(receipt, receipt.selection, receipt.execution, receipt.session_id);
  await updateJson<ModelSessionReceiptV1>(file, current => {
    if (current) {
      assertResumeModelSession(current, receipt.selection, receipt.execution, receipt.session_id);
      if (JSON.stringify(sorted(current)) !== JSON.stringify(sorted(receipt))) throw new Error("Session observation changed.");
      return undefined;
    }
    return JSON.parse(JSON.stringify(receipt));
  });
}
