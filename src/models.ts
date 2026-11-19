import { spawn } from "node:child_process";
import fsp from "node:fs/promises";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import type { BackendId } from "./types.js";
import type { AuthProfileCredential } from "./types.js";
import { loadAuthStore, loadConfig } from "./state.js";
import { readJson, rudderHome, writeJson } from "./util.js";

const MODELS_DEV_URL = "https://models.dev/api.json";
const OPENAI_MODELS_URL = "https://api.openai.com/v1/models";
const ANTHROPIC_MODELS_URL = "https://api.anthropic.com/v1/models?limit=100";
const ANTHROPIC_VERSION = "2023-06-01";
const MODELS_DEV_CACHE_MAX_AGE_MS = 24 * 60 * 60 * 1000;
const FETCH_TIMEOUT_MS = 2500;
const MODEL_PICKER_LIMIT = 24;

export type EffortId = "low" | "medium" | "high" | "xhigh" | "max";

export type ModelOption = {
  /** What the picker shows: "Opus 5.5", "GPT-6.1 Sol", "Opus 5.5 · 1M". */
  label: string;
  /** The exact id the agent's CLI is started with. */
  value?: string;
  detail?: string;
  backend?: BackendId;
  /** The generation it is listed under, newest first: "Claude 5.5", "GPT-6.1". */
  group?: string;
  /** The effort levels it can run at. Empty: it has no effort setting. */
  efforts?: EffortId[];
};

type CodexCacheModel = {
  slug?: string;
  display_name?: string;
  description?: string;
  visibility?: string;
  supported_reasoning_levels?: Array<{ effort?: string }>;
};

type CodexCache = {
  models?: CodexCacheModel[];
};

type ModelsDevModel = {
  id?: string;
  name?: string;
  reasoning?: boolean;
  tool_call?: boolean;
  release_date?: string;
  last_updated?: string;
  limit?: {
    context?: number;
    output?: number;
  };
  cost?: {
    input?: number;
    output?: number;
  };
  modalities?: {
    input?: string[];
    output?: string[];
  };
  source?: string;
  created?: number;
  owned_by?: string;
};

type ModelsDevProvider = {
  id?: string;
  name?: string;
  models?: Record<string, ModelsDevModel>;
};

type ModelsDevCache = Record<string, ModelsDevProvider>;

type OpenAIModelsResponse = {
  data?: Array<{
    id?: string;
    created?: number;
    owned_by?: string;
    object?: string;
  }>;
};

type AnthropicModelsResponse = {
  data?: Array<{
    id?: string;
    display_name?: string;
    created_at?: string;
    type?: string;
  }>;
};

let modelCatalogInFlight: Promise<ModelsDevCache> | undefined;

export async function discoverModelOptions(
  backend: BackendId,
  configuredDefault?: string,
): Promise<ModelOption[]> {
  const defaultDetail = configuredDefault || "CLI default";
  const picker = backend === "claude" ? "claude" : "codex";
  // The catalog (models.dev, the provider APIs, local history) and the CLI's
  // own list: Claude Code's /model list or Codex's model cache. The CLI's
  // list names a model, its effort levels and its display name as soon as
  // the CLI learns it.
  const [data, cli] = await Promise.all([
    readModelsDev(),
    picker === "claude" ? readClaudeCodeModels() : readCodexCliModels(),
  ]);
  const catalog = (picker === "claude" ? data.anthropic : data.openai)?.models ?? {};
  return [{ label: "Default", value: undefined, detail: defaultDetail }, ...pickerRows(picker, catalog, cli)];
}

/** Refreshes the models.dev catalog the dashboard's own picker reads. */
export async function refreshModelCatalog(): Promise<void> {
  await readModelsDev();
}

/** The models Codex's cache (the CLI's own list) shows. */
async function readCodexCacheModels(): Promise<CodexCacheModel[]> {
  const file = path.join(process.env.CODEX_HOME || path.join(os.homedir(), ".codex"), "models_cache.json");
  const raw = await fsp.readFile(file, "utf8").catch(() => "");
  if (!raw) {
    return [];
  }
  let parsed: CodexCache;
  try {
    parsed = JSON.parse(raw) as CodexCache;
  } catch {
    // A partially-written cache (Codex writing concurrently) should degrade to
    // "no local models" for this source, not reject the whole discovery.
    return [];
  }
  return (parsed.models ?? [])
    .filter((model) => model.slug && model.slug !== "codex-auto-review" && model.visibility !== "hide");
}

async function discoverCodexModelsLocal(): Promise<ModelOption[]> {
  return (await readCodexCacheModels()).map((model) => ({
    label: model.slug || model.display_name || "model",
    value: model.slug,
    detail: model.display_name || model.description || "local Codex cache",
  }));
}

async function readCodexCliModels(): Promise<CliModel[]> {
  return (await readCodexCacheModels()).map((model) => ({
    id: model.slug!,
    name: model.display_name,
    description: model.description,
    efforts: (model.supported_reasoning_levels ?? []).flatMap((level) => (level.effort ? [level.effort] : [])),
    alias: false,
  }));
}

async function readModelsDev(): Promise<ModelsDevCache> {
  if (modelCatalogInFlight) {
    return modelCatalogInFlight;
  }
  modelCatalogInFlight = readMergedModelCatalog().finally(() => {
    modelCatalogInFlight = undefined;
  });
  return modelCatalogInFlight;
}

async function readMergedModelCatalog(): Promise<ModelsDevCache> {
  const cached = await readModelsDevCache();
  const current = cached !== null && Date.now() - cached.mtimeMs < MODELS_DEV_CACHE_MAX_AGE_MS;
  const base = current ? cached.data : await fetchJsonWithTimeout<ModelsDevCache>(MODELS_DEV_URL, {});

  const merged = cloneModelCatalog(base);
  const added = await mergeLiveAndLocalModels(merged);
  if (!current || added) {
    await writeModelsDevCache(merged);
  }
  return merged;
}

/** The cached catalog, or null when there is none yet. A cache that does not
 * parse is an error that names the file. */
async function readModelsDevCache(): Promise<{ data: ModelsDevCache; mtimeMs: number } | null> {
  const file = modelsDevCachePath();
  const stat = await fsp.stat(file).catch(ignoreMissing);
  const data = stat ? await readJson<ModelsDevCache>(file) : null;
  return stat && data ? { data, mtimeMs: stat.mtimeMs } : null;
}

async function writeModelsDevCache(data: ModelsDevCache): Promise<void> {
  const file = modelsDevCachePath();
  await writeJson(file, data);
}

function modelsDevCachePath(): string {
  // Resolve under RUDDER_HOME (falling back to ~/.rudder) so this TS writer and the
  // native Rust reader (models.rs models_dev_cache_path) agree on the same file when
  // RUDDER_HOME is set — otherwise dynamic model refresh + cached reasoning break.
  return path.join(rudderHome(), "models-dev.json");
}

function isUsableTextModel(id: string, model: ModelsDevModel): boolean {
  if (model.tool_call === false) {
    return false;
  }
  if (isExcludedOpenAiTextModel(id)) {
    return false;
  }
  const output = model.modalities?.output;
  return !output || output.includes("text");
}

function isClaudePickerModel(id: string): boolean {
  // No family-name allowlist: a brand-new model family (fable, and whatever
  // comes after it) must show up in the picker without a Rudder release. Only
  // the legacy 3.x-generation ids are excluded.
  return !id.includes("3-");
}

function isCodexRelevantModel(id: string): boolean {
  if (id.includes("deep-research") || id.includes("chat-latest")) {
    return false;
  }
  return id.includes("codex") || isGptTextModel(id) || isOSeriesModel(id);
}

async function mergeLiveAndLocalModels(data: ModelsDevCache): Promise<boolean> {
  let changed = false;
  const [openai, anthropic, codexLocal, claudeLocal] = await Promise.all([
    fetchOpenAIModelCatalog(),
    fetchAnthropicModelCatalog(),
    readCodexLocalModels(),
    readClaudeLocalModelIds(),
  ]);

  for (const model of openai) {
    changed = mergeProviderModel(data, "openai", model.id, model.meta) || changed;
  }
  for (const model of anthropic) {
    changed = mergeProviderModel(data, "anthropic", model.id, model.meta) || changed;
  }
  for (const model of codexLocal) {
    changed = mergeProviderModel(data, "openai", model.id, model.meta) || changed;
  }
  for (const model of claudeLocal) {
    changed = mergeProviderModel(data, "anthropic", model.id, model.meta) || changed;
  }
  return changed;
}

async function fetchOpenAIModelCatalog(): Promise<Array<{ id: string; meta: ModelsDevModel }>> {
  const authHeaders = await openAIAuthHeaders();
  for (const headers of authHeaders) {
    const response = await fetchJsonWithTimeout<OpenAIModelsResponse>(OPENAI_MODELS_URL, { headers });
    const rows = response.data ?? [];
    if (rows.length === 0) {
      continue;
    }
    return rows
      .filter((model) => typeof model.id === "string" && isCodexRelevantModel(model.id) && !isExcludedOpenAiTextModel(model.id))
      .map((model) => ({
        id: model.id!,
        meta: {
          id: model.id,
          name: model.id,
          tool_call: true,
          reasoning: isLikelyReasoningModel("codex", model.id!),
          release_date: model.created ? new Date(model.created * 1000).toISOString().slice(0, 10) : undefined,
          modalities: { output: ["text"] },
          source: "openai-api",
          created: model.created,
          owned_by: model.owned_by,
        },
      }));
  }
  return [];
}

async function fetchAnthropicModelCatalog(): Promise<Array<{ id: string; meta: ModelsDevModel }>> {
  const authHeaders = await anthropicAuthHeaders();
  for (const headers of authHeaders) {
    const response = await fetchJsonWithTimeout<AnthropicModelsResponse>(ANTHROPIC_MODELS_URL, { headers });
    const rows = response.data ?? [];
    if (rows.length === 0) {
      continue;
    }
    return rows
      .filter((model) => typeof model.id === "string" && model.id.startsWith("claude-") && isClaudePickerModel(model.id))
      .map((model) => ({
        id: model.id!,
        meta: {
          id: model.id,
          name: model.display_name || prettyClaudeModel(model.id!),
          tool_call: true,
          reasoning: isLikelyReasoningModel("claude", model.id!),
          release_date: typeof model.created_at === "string" ? model.created_at.slice(0, 10) : undefined,
          modalities: { output: ["text"] },
          source: "anthropic-api",
        },
      }));
  }
  return [];
}

async function readCodexLocalModels(): Promise<Array<{ id: string; meta: ModelsDevModel }>> {
  return (await discoverCodexModelsLocal())
    .filter((model) => model.value && isCodexRelevantModel(model.value) && !isExcludedOpenAiTextModel(model.value))
    .map((model) => ({
      id: model.value!,
      meta: {
        id: model.value,
        name: model.detail || model.label,
        tool_call: true,
        reasoning: isLikelyReasoningModel("codex", model.value!),
        modalities: { output: ["text"] },
        source: "codex-local-cache",
      },
    }));
}

async function readClaudeLocalModelIds(): Promise<Array<{ id: string; meta: ModelsDevModel }>> {
  const counts = new Map<string, number>();
  await collectClaudeProjectModels(path.join(os.homedir(), ".claude", "projects"), counts);
  return [...counts.keys()]
    .filter((id) => id.startsWith("claude-") && isClaudePickerModel(id))
    .map((id) => ({
      id,
      meta: {
        id,
        name: prettyClaudeModel(id),
        tool_call: true,
        reasoning: isLikelyReasoningModel("claude", id),
        modalities: { output: ["text"] },
        source: "claude-local-history",
      },
    }));
}

async function openAIAuthHeaders(): Promise<Array<Record<string, string>>> {
  const values = await providerCredentialValues("openai");
  return values.map((value) => ({ Authorization: `Bearer ${value}` }));
}

async function anthropicAuthHeaders(): Promise<Array<Record<string, string>>> {
  const credentials = await providerCredentials("anthropic");
  const headers: Array<Record<string, string>> = [];
  for (const credential of credentials) {
    if (credential.kind === "api_key") {
      headers.push({
        "x-api-key": credential.value,
        "anthropic-version": ANTHROPIC_VERSION,
      });
    } else {
      headers.push({
        Authorization: `Bearer ${credential.value}`,
        "anthropic-version": ANTHROPIC_VERSION,
      });
    }
  }
  return headers;
}

async function providerCredentialValues(provider: "openai" | "anthropic"): Promise<string[]> {
  return (await providerCredentials(provider)).map((credential) => credential.value);
}

async function providerCredentials(provider: "openai" | "anthropic"): Promise<Array<{ kind: "api_key" | "token"; value: string }>> {
  const values: Array<{ kind: "api_key" | "token"; value: string }> = [];
  const envKey = provider === "openai" ? process.env.OPENAI_API_KEY?.trim() : process.env.ANTHROPIC_API_KEY?.trim();
  if (envKey) {
    values.push({ kind: "api_key", value: envKey });
  }
  const [store, config] = await Promise.all([loadAuthStore(), loadConfig()]);
  const ids = provider === "openai"
    ? [
        config?.backends?.codex?.profileId,
        "openai-codex:default",
        "openai:env",
        "openai:default",
      ]
    : [
        config?.backends?.claude?.profileId,
        "anthropic:claude-code",
        "anthropic:env",
        "anthropic:default",
      ];
  for (const id of ids) {
    if (!id) {
      continue;
    }
    const credential = store.profiles[id];
    // Subscription access belongs to the Codex protocol, not the public API.
    // Never probe billing/API endpoints with a subscription bearer or a profile
    // belonging to another provider. Codex's local catalog remains available.
    if (credential?.provider !== provider || (provider === "openai" && credential.type !== "api_key")) continue;
    addCredential(values, credential);
  }
  const seen = new Set<string>();
  return values.filter((credential) => {
    const key = `${credential.kind}:${credential.value}`;
    if (seen.has(key)) {
      return false;
    }
    seen.add(key);
    return true;
  });
}

function addCredential(values: Array<{ kind: "api_key" | "token"; value: string }>, credential: AuthProfileCredential | undefined): void {
  if (!credential) {
    return;
  }
  // Rudder's copy of a CLI login stops working once the CLI refreshes it, and
  // an expired token is only ever refused, so it is not offered at all.
  const expired = "expires" in credential && typeof credential.expires === "number" && credential.expires <= Date.now();
  if (credential.type === "api_key" && credential.key) {
    values.push({ kind: "api_key", value: credential.key });
  } else if (credential.type === "token" && credential.token && !expired) {
    values.push({ kind: "token", value: credential.token });
  } else if (credential.type === "oauth" && credential.access && !expired) {
    values.push({ kind: "token", value: credential.access });
  }
}

async function fetchJsonWithTimeout<T>(url: string, init: RequestInit): Promise<T> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);
  try {
    const response = await fetch(url, { ...init, signal: controller.signal }).catch((error: unknown) => {
      if (controller.signal.aborted) {
        throw new Error(`${url} did not answer within ${FETCH_TIMEOUT_MS / 1000} s`);
      }
      const cause = error instanceof Error && error.cause instanceof Error ? error.cause : error;
      throw new Error(`could not reach ${url}: ${cause instanceof Error ? cause.message : String(cause)}`);
    });
    if (!response.ok) {
      const body = (await response.text()).trim().slice(0, 200);
      throw new Error(`${url} returned ${response.status}${body ? `: ${body}` : ""}`);
    }
    return await response.json() as T;
  } finally {
    clearTimeout(timer);
  }
}

function cloneModelCatalog(data: ModelsDevCache): ModelsDevCache {
  return JSON.parse(JSON.stringify(data)) as ModelsDevCache;
}

function mergeProviderModel(data: ModelsDevCache, providerId: "openai" | "anthropic", modelId: string, meta: ModelsDevModel): boolean {
  if (!modelId) {
    return false;
  }
  const provider = data[providerId] ?? { id: providerId, models: {} };
  const models = provider.models ?? {};
  const previous = models[modelId] ?? {};
  models[modelId] = {
    ...previous,
    ...meta,
    id: modelId,
  };
  data[providerId] = {
    ...provider,
    id: provider.id || providerId,
    models,
  };
  return JSON.stringify(previous) !== JSON.stringify(models[modelId]);
}

function isExcludedOpenAiTextModel(id: string): boolean {
  return /\b(embedding|image|audio|tts|whisper|transcribe|translation|moderation|rerank|realtime|dall-e|search)\b/i.test(id);
}

function isGptTextModel(id: string): boolean {
  return /^gpt-\d/.test(id);
}

function isOSeriesModel(id: string): boolean {
  return /^o\d/.test(id);
}

function isLikelyReasoningModel(backend: "claude" | "codex", id: string): boolean {
  if (backend === "claude") {
    if (!id.startsWith("claude-") || id.includes("haiku") || id.includes("3-")) {
      return id.includes("opus") || id.includes("sonnet") || id.includes("fable");
    }
    return true;
  }
  return isGptTextModel(id) || id.includes("codex") || isOSeriesModel(id);
}

/** A catalog model's context window and release date. */
function catalogDetail(model: ModelsDevModel | undefined): string {
  const parts: string[] = [];
  if (model?.limit?.context) {
    parts.push(`${formatNumber(model.limit.context)} context`);
  }
  if (model?.release_date) {
    parts.push(model.release_date);
  }
  return parts.join(" · ");
}

function formatNumber(value: number): string {
  if (value >= 1_000_000) {
    return `${Math.round(value / 100_000) / 10}M`;
  }
  if (value >= 1_000) {
    return `${Math.round(value / 1000)}K`;
  }
  return String(value);
}

// The Claude Code models this machine used lately: the tail of the newest
// session logs. A new model shows up in new sessions, and reading the whole
// history instead cost 11 s and 4 GB on a busy machine on every discovery.
const CLAUDE_HISTORY_FILES = 100;
const CLAUDE_HISTORY_TAIL_BYTES = 64 * 1024;

async function collectClaudeProjectModels(root: string, counts: Map<string, number>): Promise<void> {
  if (!fs.existsSync(root)) {
    return;
  }
  const logs: Array<{ file: string; mtimeMs: number; size: number }> = [];
  await listClaudeSessionLogs(root, logs, 0);
  logs.sort((a, b) => b.mtimeMs - a.mtimeMs);
  for (const log of logs.slice(0, CLAUDE_HISTORY_FILES)) {
    const length = Math.min(log.size, CLAUDE_HISTORY_TAIL_BYTES);
    const buffer = Buffer.alloc(length);
    const handle = await fsp.open(log.file, "r").catch(ignoreMissing);
    if (!handle) {
      continue;
    }
    try {
      await handle.read(buffer, 0, length, log.size - length);
    } finally {
      await handle.close();
    }
    const lines = buffer.toString("utf8").split(/\r?\n/);
    if (length < log.size) {
      lines.shift(); // starts mid-row
    }
    for (const line of lines) {
      if (!line.trim()) {
        continue;
      }
      try {
        collectModelFields(JSON.parse(line), counts);
      } catch {
        // Ignore corrupt or partial JSONL rows from active sessions.
      }
    }
  }
}

async function listClaudeSessionLogs(dir: string, logs: Array<{ file: string; mtimeMs: number; size: number }>, depth: number): Promise<void> {
  if (depth > 2) {
    return;
  }
  const entries = await fsp.readdir(dir, { withFileTypes: true }).catch(ignoreMissing) ?? [];
  await Promise.all(entries.map(async (entry) => {
    const file = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      await listClaudeSessionLogs(file, logs, depth + 1);
    } else if (entry.name.endsWith(".jsonl")) {
      const stat = await fsp.stat(file).catch(ignoreMissing);
      if (stat) {
        logs.push({ file, mtimeMs: stat.mtimeMs, size: stat.size });
      }
    }
  }));
}

// Claude Code deletes old session logs while a scan runs; a vanished file is
// simply not part of the history any more.
function ignoreMissing(error: NodeJS.ErrnoException): undefined {
  if (error.code === "ENOENT") {
    return undefined;
  }
  throw error;
}

function collectModelFields(value: unknown, counts: Map<string, number>, depth = 0): void {
  if (!value || typeof value !== "object" || depth > 5) {
    return;
  }
  if (Array.isArray(value)) {
    for (const item of value) {
      collectModelFields(item, counts, depth + 1);
    }
    return;
  }
  const record = value as Record<string, unknown>;
  if (typeof record.model === "string") {
    counts.set(record.model, (counts.get(record.model) ?? 0) + 1);
  }
  if (record.model && typeof record.model === "object") {
    const model = record.model as Record<string, unknown>;
    for (const key of ["id", "name", "model"]) {
      if (typeof model[key] === "string") {
        counts.set(model[key], (counts.get(model[key]) ?? 0) + 1);
      }
    }
  }
  for (const child of Object.values(record)) {
    collectModelFields(child, counts, depth + 1);
  }
}

function prettyClaudeModel(model: string): string {
  if (model === "opus" || model === "sonnet") {
    return `${capitalize(model)} latest`;
  }
  return model
    .replace(/^claude-/, "")
    .split("-")
    .map((part) => (/^\d+$/.test(part) ? part : capitalize(part)))
    .join(" ");
}

function capitalize(value: string): string {
  return `${value.slice(0, 1).toUpperCase()}${value.slice(1)}`;
}

// The picker's rows ---------------------------------------------------------
//
// Grouped by generation, newest first, with each generation's families
// together (most capable first). Names and effort levels come from the CLI's
// own list when it has the model, and are read from the id otherwise.

type PickerBackend = "claude" | "codex";

/** A model as its CLI lists it. */
type CliModel = {
  id: string;
  name?: string;
  description?: string;
  /** The effort levels the CLI runs it at; none when it has no effort setting. */
  efforts: string[];
  /** A Claude Code alias ("opus") that always means the family's newest model. */
  alias: boolean;
};

type ParsedModelId = {
  /** The generation's heading: "Claude 5.5", "GPT-6.1", "o3". */
  group: string;
  /** GPT and Claude ids, then o-series, then ids of no known shape. */
  series: number;
  version: number[];
  family: string;
  /** Read from the id: "Opus 5.5", "GPT-6.1 Sol". */
  name: string;
  /** A context window suffix: "claude-opus-5-5[1m]" is "1M". */
  context?: string;
};

const ALIAS_GROUP = "Always the newest";
const CONTEXT_SUFFIX = /\[(\d+)([km])\]$/i;

// The levels the dashboard passes (native/src/models.rs effort_options_for):
// a model with an effort setting takes low to xhigh, and Claude also max.
const RUDDER_EFFORTS: Record<PickerBackend, EffortId[]> = {
  claude: ["low", "medium", "high", "xhigh", "max"],
  codex: ["low", "medium", "high", "xhigh"],
};

function pickerRows(backend: PickerBackend, catalog: Record<string, ModelsDevModel>, cli: CliModel[]): ModelOption[] {
  const listed = new Map<string, CliModel>();
  const aliases: CliModel[] = [];
  for (const model of cli) {
    if (model.alias) {
      aliases.push(model);
    } else if (!listed.has(model.id)) {
      listed.set(model.id, model);
    }
  }
  const ids = new Set(Object.entries(catalog).filter(([id, model]) => isPickerModel(backend, id, model)).map(([id]) => id));
  for (const id of listed.keys()) {
    ids.add(id);
  }
  // A dated snapshot repeats its undated twin, so only the twin is listed
  // (the dashboard's picker does the same: models.rs dated_duplicate_base).
  for (const id of ids) {
    const base = /^(.+)-\d{8}$/.exec(id)?.[1];
    if (base && ids.has(base)) {
      const twin = listed.get(id);
      if (twin && !listed.has(base)) {
        listed.set(base, twin);
      }
      ids.delete(id);
    }
  }
  const prices = familyPrices(backend, catalog);
  const rows = [...ids]
    .map((id) => ({ id, parsed: parseModelId(backend, id) }))
    .sort((a, b) => compareModelIds(a.parsed, b.parsed, prices) || a.id.localeCompare(b.id))
    // Older catalog models make way; every model the CLI lists stays.
    .filter((row, index) => listed.has(row.id) || index < MODEL_PICKER_LIMIT)
    .map(({ id, parsed }): ModelOption => ({
      label: modelLabel(backend, id, parsed, listed),
      value: id,
      detail: listed.get(id)?.description || catalogDetail(catalog[id]),
      group: parsed.group,
      efforts: modelEfforts(backend, id, catalog[id], listed.get(id)),
    }));
  const aliasRows = aliases
    .map((alias) => ({ alias, family: alias.id.replace(CONTEXT_SUFFIX, "") }))
    .sort((a, b) => (prices.get(b.family) ?? 0) - (prices.get(a.family) ?? 0) || a.alias.id.localeCompare(b.alias.id))
    .map(({ alias }): ModelOption => ({
      label: withContext(capitalize(alias.id.replace(CONTEXT_SUFFIX, "")), alias.id),
      value: alias.id,
      detail: [alias.name ? `Now ${alias.name}` : "", alias.description ?? ""].filter(Boolean).join(" · "),
      group: ALIAS_GROUP,
      efforts: modelEfforts(backend, alias.id, undefined, alias),
    }));
  return [...rows, ...aliasRows];
}

function isPickerModel(backend: PickerBackend, id: string, model: ModelsDevModel): boolean {
  if (!isUsableTextModel(id, model)) {
    return false;
  }
  return backend === "claude" ? id.startsWith("claude-") && isClaudePickerModel(id) : isCodexRelevantModel(id);
}

/** "claude-opus-5-5" is Opus of Claude 5.5, "gpt-6.1-sol" Sol of GPT-6.1, "o4-mini" Mini of o4. */
function parseModelId(backend: PickerBackend, id: string): ParsedModelId {
  const suffix = CONTEXT_SUFFIX.exec(id);
  const context = suffix ? `${suffix[1]}${suffix[2].toUpperCase()}` : undefined;
  const base = suffix ? id.slice(0, suffix.index) : id;
  if (backend === "claude") {
    const claude = /^claude-([a-z]+)((?:-\d{1,2})+)(?:-\d{8})?$/.exec(base);
    if (claude) {
      const version = claude[2].slice(1).split("-").map(Number);
      const number = version.join(".");
      return { group: `Claude ${number}`, series: 0, version, family: claude[1], name: `${capitalize(claude[1])} ${number}`, context };
    }
  } else {
    const gpt = /^gpt-(\d+(?:\.\d+)?)(?:-([a-z][a-z0-9-]*))?$/.exec(base);
    if (gpt) {
      const variant = gpt[2] ?? "";
      return { group: `GPT-${gpt[1]}`, series: 0, version: gpt[1].split(".").map(Number), family: variant, name: [`GPT-${gpt[1]}`, ...words(variant)].join(" "), context };
    }
    const oSeries = /^o(\d+)(?:-([a-z][a-z0-9-]*))?$/.exec(base);
    if (oSeries) {
      const variant = oSeries[2] ?? "";
      return { group: `o${oSeries[1]}`, series: 1, version: [Number(oSeries[1])], family: `o:${variant}`, name: [`o${oSeries[1]}`, ...words(variant)].join(" "), context };
    }
  }
  return { group: "Other", series: 2, version: [], family: base, name: base, context };
}

function words(variant: string): string[] {
  return variant.split("-").filter(Boolean).map(capitalize);
}

/** Newest generation first; within one, the family with the highest price first. */
function compareModelIds(a: ParsedModelId, b: ParsedModelId, prices: Map<string, number>): number {
  if (a.series !== b.series) {
    return a.series - b.series;
  }
  for (let i = 0; i < Math.max(a.version.length, b.version.length); i += 1) {
    const diff = (b.version[i] ?? 0) - (a.version[i] ?? 0);
    if (diff !== 0) {
      return diff;
    }
  }
  return (prices.get(b.family) ?? 0) - (prices.get(a.family) ?? 0) || Number(!!a.context) - Number(!!b.context);
}

/** Each family's highest output price in the catalog: Fable before Opus before Sonnet before Haiku. */
function familyPrices(backend: PickerBackend, catalog: Record<string, ModelsDevModel>): Map<string, number> {
  const prices = new Map<string, number>();
  for (const [id, model] of Object.entries(catalog)) {
    const price = model.cost?.output;
    if (typeof price === "number") {
      const { family } = parseModelId(backend, id);
      prices.set(family, Math.max(prices.get(family) ?? 0, price));
    }
  }
  return prices;
}

/** The CLI's display name for the model ("GPT-6.2-Sol" reads "GPT-6.2 Sol"), or one read from the id. */
function modelLabel(backend: PickerBackend, id: string, parsed: ParsedModelId, listed: Map<string, CliModel>): string {
  const own = listed.get(id.replace(CONTEXT_SUFFIX, ""))?.name?.trim();
  const name = own ? (backend === "codex" ? own.replace(/^(GPT-\d+(?:\.\d+)?)-/i, "$1 ") : own) : parsed.name;
  return withContext(name, id);
}

/** "Opus 5.5" of "claude-opus-5-5[1m]" reads "Opus 5.5 · 1M". */
function withContext(name: string, id: string): string {
  const suffix = CONTEXT_SUFFIX.exec(id);
  return suffix ? `${name} · ${suffix[1]}${suffix[2].toUpperCase()}` : name;
}

/** The levels both the model's CLI lists and the dashboard passes; the dashboard refuses any other. */
function modelEfforts(backend: PickerBackend, id: string, model: ModelsDevModel | undefined, listed: CliModel | undefined): EffortId[] {
  if (!hasEffortSetting(backend, id, model)) {
    return [];
  }
  return listed ? RUDDER_EFFORTS[backend].filter((level) => listed.efforts.includes(level)) : RUDDER_EFFORTS[backend];
}

/** native/src/models.rs model_supports_reasoning: the family rule, or the catalog's word. */
function hasEffortSetting(backend: PickerBackend, id: string, model: ModelsDevModel | undefined): boolean {
  if (model?.reasoning === true) {
    return true;
  }
  if (backend === "claude") {
    return /opus|sonnet|fable/.test(id) || (id.startsWith("claude-") && !id.includes("haiku") && !id.includes("3-"));
  }
  return isGptTextModel(id) || id.includes("codex") || isOSeriesModel(id);
}

type ClaudeCodeModel = {
  value?: string;
  resolvedModel?: string;
  displayName?: string;
  description?: string;
  supportsEffort?: boolean;
  supportedEffortLevels?: string[];
};

const CLAUDE_CODE_TIMEOUT_MS = 20_000;

/**
 * Claude Code's own /model list for this account, with each alias's current
 * model, display names and the effort levels each model takes. Each alias
 * also lists the model it stands for. Empty when Claude Code is not installed.
 */
async function readClaudeCodeModels(): Promise<CliModel[]> {
  const answer = await claudeCodeModelList();
  const models: CliModel[] = [];
  for (const row of answer ?? []) {
    // Claude Code's "default" is what Rudder's Default row starts.
    if (!row.value || row.value === "default") {
      continue;
    }
    const efforts = row.supportsEffort ? row.supportedEffortLevels ?? [] : [];
    const alias = !row.value.startsWith("claude-");
    models.push({ id: row.value, name: row.displayName, description: row.description, efforts, alias });
    if (alias && row.resolvedModel?.startsWith("claude-")) {
      models.push({ id: row.resolvedModel, name: row.displayName, description: row.description, efforts, alias: false });
    }
  }
  return models;
}

/** Claude Code's headless `initialize` answer: its model list, or null when Claude Code is not installed. */
function claudeCodeModelList(): Promise<ClaudeCodeModel[] | null> {
  const bin = process.env.RUDDER_CLAUDE_BIN?.trim() || "claude";
  const args = [
    "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
    // It answers and exits: no hooks or MCP servers start, and no prompt is sent.
    "--settings", JSON.stringify({ disableAllHooks: true }),
    "--strict-mcp-config", "--mcp-config", JSON.stringify({ mcpServers: {} }),
  ];
  const requestId = "rudder-models";
  return new Promise((resolve, reject) => {
    const child = spawn(bin, args, { cwd: os.homedir(), stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    let settled = false;
    const finish = (error: Error | null, models?: ClaudeCodeModel[] | null) => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      // It exits once its stdin closes; nothing here waits for that.
      child.stdin.end();
      child.stdout.destroy();
      child.stderr.destroy();
      child.unref();
      if (error) {
        child.kill("SIGKILL");
        reject(error);
      } else {
        resolve(models ?? null);
      }
    };
    const timer = setTimeout(() => finish(new Error(`${bin} did not answer initialize within ${CLAUDE_CODE_TIMEOUT_MS / 1000}s`)), CLAUDE_CODE_TIMEOUT_MS);
    child.on("error", (error: NodeJS.ErrnoException) => {
      finish(error.code === "ENOENT" ? null : new Error(`could not start ${bin}: ${error.message}`), null);
    });
    child.stdin.on("error", (error) => finish(new Error(`could not ask ${bin} for its models: ${error.message}`)));
    child.stderr.on("data", (data) => { stderr += data; });
    child.stdout.on("data", (data) => {
      stdout += data;
      let newline;
      while ((newline = stdout.indexOf("\n")) >= 0) {
        const line = stdout.slice(0, newline).trim();
        stdout = stdout.slice(newline + 1);
        if (!line.startsWith("{")) {
          continue;
        }
        let message: { type?: string; response?: { request_id?: string; subtype?: string; error?: string; response?: { models?: unknown } } };
        try {
          message = JSON.parse(line);
        } catch {
          finish(new Error(`${bin} wrote a line that is not JSON: ${line.slice(0, 120)}`));
          return;
        }
        if (message.type !== "control_response" || message.response?.request_id !== requestId) {
          continue;
        }
        if (message.response.subtype !== "success") {
          finish(new Error(`${bin} refused initialize: ${message.response.error || "no reason given"}`));
        } else if (!Array.isArray(message.response.response?.models)) {
          finish(new Error(`${bin} answered initialize without its model list`));
        } else {
          finish(null, message.response.response.models as ClaudeCodeModel[]);
        }
        return;
      }
    });
    child.on("close", (code) => finish(new Error(`${bin} exited (${code}) before listing its models${stderr.trim() ? `: ${stderr.trim().split("\n")[0]}` : ""}`)));
    child.stdin.write(`${JSON.stringify({ type: "control_request", request_id: requestId, request: { subtype: "initialize" } })}\n`);
  });
}
