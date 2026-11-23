import { execFile, spawn } from "node:child_process";
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

export type EffortId = "low" | "medium" | "high" | "xhigh" | "max";

export type ModelOption = {
  /** What the pickers show, the name and then the version: "Opus 5.5", "Sol 6.1", "Opus 5.5 · 1M". */
  label: string;
  /** The exact id the agent's CLI is started with. */
  value: string;
  detail: string;
  /** The effort levels it can run at. Empty: it has no effort setting. */
  efforts: EffortId[];
  /** A CLI alias ("opus") that moves to each new release of its family. */
  alias?: true;
};

/** One harness's section of the pickers. */
export type HarnessModels = {
  /** The backend id `/model <id> <model>` takes. */
  id: BackendId;
  /** "Claude", "Codex". */
  label: string;
  /** What it signs in with: "Claude Code", "ChatGPT". */
  account: string;
  /** Sorted: the highest version first, the harness's tier order within one; aliases last. */
  models: ModelOption[];
  /**
   * The model it runs for an agent that names none: Claude Code's default,
   * the `model` in Codex's config.toml. Null when it does not say.
   */
  default: string | null;
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
  /** The effort levels Claude Code lists for the model (empty: no effort
   * setting). The dashboard (native/src/models.rs effort_options_for) and the
   * picker accept exactly these. */
  efforts?: EffortId[];
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

/** The merged catalog, and Claude Code's own list that went into it. */
type ModelCatalog = { data: ModelsDevCache; claudeCode: ClaudeCodeList };

let modelCatalogInFlight: Promise<ModelCatalog> | undefined;

/**
 * Every harness's models, one section per HARNESSES row, in its order. A
 * section lists exactly what the harness's CLI lists for this account, with
 * the CLI's names and effort levels; the merged catalog (models.dev, the
 * provider APIs, local history, Claude Code's list) adds detail lines and
 * prices. Reading it also refreshes the catalog the dashboard reads.
 */
export async function discoverHarnesses(): Promise<HarnessModels[]> {
  const catalog = await readModelsDev();
  return Promise.all(HARNESSES.map(async (harness) => ({
    id: harness.id,
    label: harness.label,
    account: harness.account,
    models: pickerRows(harness, (harness.catalog && catalog.data[harness.catalog]?.models) || {}, await harness.listed(catalog)),
    default: await harness.defaultModel(catalog),
  })));
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

/**
 * The model Codex runs when it is given none: the top-level `model` in its
 * config.toml, or null when the file sets none.
 */
async function readCodexConfiguredModel(): Promise<string | null> {
  const file = path.join(process.env.CODEX_HOME || path.join(os.homedir(), ".codex"), "config.toml");
  const raw = await fsp.readFile(file, "utf8").catch(ignoreMissing);
  if (!raw) {
    return null;
  }
  // Top-level keys come before the first [table].
  for (const line of raw.split("\n")) {
    if (/^\s*\[/.test(line)) {
      break;
    }
    const match = /^\s*model\s*=\s*"([^"]+)"/.exec(line);
    if (match) {
      return match[1];
    }
  }
  return null;
}

async function readCodexCliModels(): Promise<CliModel[]> {
  return (await readCodexCacheModels()).map((model) => ({
    id: model.slug!,
    name: model.display_name,
    description: model.description,
    efforts: (model.supported_reasoning_levels ?? []).flatMap((level) => (level.effort ? [level.effort] : [])),
    alias: false,
    resolved: model.slug!,
  }));
}

const OPENCODE_TIMEOUT_MS = 20_000;

/** The OpenCode CLI; RUDDER_OPENCODE_BIN points tests at a stub, as in the dashboard. */
function opencodeBin(): string {
  return process.env.RUDDER_OPENCODE_BIN?.trim() || "opencode";
}

/**
 * `opencode <args>` run from the home directory: its stdout, or null when
 * OpenCode is not installed. Any other failure is an error with its reason.
 */
async function opencodeOutput(args: string[]): Promise<string | null> {
  const bin = opencodeBin();
  return await new Promise((resolve, reject) => {
    execFile(bin, args, { cwd: os.homedir(), timeout: OPENCODE_TIMEOUT_MS, maxBuffer: 64 * 1024 * 1024 }, (error, stdout, stderr) => {
      if ((error as NodeJS.ErrnoException | null)?.code === "ENOENT") {
        resolve(null);
      } else if (error) {
        const reason = String(stderr).trim().split("\n")[0] || error.message;
        reject(new Error(`${bin} ${args.join(" ")} failed: ${reason}`));
      } else {
        resolve(String(stdout));
      }
    });
  });
}

/** OpenCode's model list for this machine's sign-ins, with names and variants. */
async function readOpencodeModels(): Promise<CliModel[]> {
  const out = await opencodeOutput(["models", "--verbose"]);
  return out === null ? [] : parseOpencodeModels(out);
}

/**
 * `opencode models --verbose`: each model's `provider/model` id on a line of
 * its own, then its JSON (name, limit.context, release_date and `variants`,
 * the reasoning levels it runs at, keyed by name).
 */
export function parseOpencodeModels(out: string): CliModel[] {
  const lines = out.split("\n");
  const models: CliModel[] = [];
  for (let index = 0; index < lines.length; index += 1) {
    const id = lines[index].trim();
    if (!/^[^\s{}"]+\/\S+$/.test(id) || lines[index + 1] !== "{") {
      continue;
    }
    const end = lines.indexOf("}", index + 1);
    if (end < 0) {
      throw new Error(`opencode models --verbose cut off in ${id}`);
    }
    const model = JSON.parse(lines.slice(index + 1, end + 1).join("\n")) as {
      name?: string; limit?: { context?: number }; release_date?: string; variants?: Record<string, unknown>;
    };
    models.push({
      id,
      name: model.name,
      description: catalogDetail({ limit: model.limit, release_date: model.release_date }),
      efforts: Object.keys(model.variants ?? {}),
      alias: false,
      resolved: id,
    });
    index = end;
  }
  return models;
}

/**
 * The model OpenCode runs when it is given none: the `model` its resolved
 * config sets (`opencode debug config`), or null when none does.
 */
async function readOpencodeConfiguredModel(): Promise<string | null> {
  const out = await opencodeOutput(["debug", "config"]);
  if (out === null) {
    return null;
  }
  const model = (JSON.parse(out) as { model?: string | { providerID?: string; model?: string } }).model;
  if (typeof model === "string") {
    return model || null;
  }
  return model?.providerID && model.model ? `${model.providerID}/${model.model}` : null;
}

const AGY_TIMEOUT_MS = 20_000;

/** The Antigravity CLI; RUDDER_AGY_BIN points tests at a stub, as in the dashboard. */
export function agyBin(): string {
  return process.env.RUDDER_AGY_BIN?.trim() || "agy";
}

/**
 * `agy --output-format json models`, whose stdout holds the list alone (agy
 * prints its errors and spinner on stderr). The flag is agy's own, so it
 * comes before `models`, which refuses it after. None when agy is not
 * installed or not signed in: it then says "Please sign in to view available
 * models". Any other failure is an error with its reason.
 */
async function readAntigravityModels(): Promise<CliModel[]> {
  const bin = agyBin();
  const args = ["--output-format", "json", "models"];
  return await new Promise((resolve, reject) => {
    const child = execFile(bin, args, { cwd: os.homedir(), timeout: AGY_TIMEOUT_MS, maxBuffer: 16 * 1024 * 1024 }, (error, stdout, stderr) => {
      if ((error as NodeJS.ErrnoException | null)?.code === "ENOENT" || /Please sign in to view available models/.test(`${stdout}\n${stderr}`)) {
        resolve([]);
      } else if (error) {
        const reason = String(stderr).trim().split("\n")[0] || error.message;
        reject(new Error(`${bin} ${args.join(" ")} failed: ${reason}`));
      } else {
        try {
          resolve(parseAntigravityModels(String(stdout)));
        } catch (parseError) {
          reject(parseError);
        }
      }
    });
    // agy waits on an open stdin pipe it inherits.
    child.stdin?.end();
  });
}

/**
 * agy's JSON model list: the result envelope its print mode writes, with
 * the list under `command.data.models` as `{"id": "...", "label": "..."}`
 * (tests/fixtures/agy-1.5.0-models.json). An id is the slug `--model` takes,
 * one per effort ("gemini-3.8-flash-high"), so the section offers no
 * efforts and no default, which the list does not name.
 */
export function parseAntigravityModels(out: string): CliModel[] {
  let payload: unknown;
  try {
    payload = JSON.parse(out);
  } catch (error) {
    throw new Error(`agy --output-format json models printed no JSON: ${(error as Error).message}`);
  }
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
    throw new Error("agy --output-format json models printed no result envelope");
  }
  const { status, response, command } = payload as {
    status?: unknown;
    response?: unknown;
    command?: { name?: unknown; data?: { models?: unknown } };
  };
  if (status !== "SUCCESS") {
    const reason = typeof response === "string" && response.trim() ? `: ${response.trim().split("\n")[0]}` : "";
    throw new Error(`agy --output-format json models ended ${JSON.stringify(status)}${reason}`);
  }
  const list = command?.name === "models" ? command.data?.models : undefined;
  if (!Array.isArray(list)) {
    throw new Error(`agy --output-format json models printed no command.data.models (command ${JSON.stringify(command?.name)})`);
  }
  return list.map((entry: unknown) => {
    const { id, label } = (entry ?? {}) as { id?: unknown; label?: unknown };
    if (typeof id !== "string" || !id || (label !== undefined && typeof label !== "string")) {
      throw new Error(`agy --output-format json models listed a model with no id: ${JSON.stringify(entry)}`);
    }
    return { id, name: label || undefined, efforts: [], alias: false, resolved: id };
  });
}

async function readModelsDev(): Promise<ModelCatalog> {
  if (modelCatalogInFlight) {
    return modelCatalogInFlight;
  }
  modelCatalogInFlight = readMergedModelCatalog().finally(() => {
    modelCatalogInFlight = undefined;
  });
  return modelCatalogInFlight;
}

async function readMergedModelCatalog(): Promise<ModelCatalog> {
  const cached = await readModelsDevCache();
  const current = cached !== null && Date.now() - cached.mtimeMs < MODELS_DEV_CACHE_MAX_AGE_MS;
  const base = current ? cached.data : await fetchJsonWithTimeout<ModelsDevCache>(MODELS_DEV_URL, {});

  const merged = cloneModelCatalog(base);
  const { changed, claudeCode } = await mergeLiveAndLocalModels(merged);
  if (!current || changed) {
    await writeModelsDevCache(merged);
  }
  return { data: merged, claudeCode };
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

async function mergeLiveAndLocalModels(data: ModelsDevCache): Promise<{ changed: boolean; claudeCode: ClaudeCodeList }> {
  let changed = false;
  const [openai, anthropic, codexLocal, claudeLocal, claudeCode] = await Promise.all([
    fetchOpenAIModelCatalog(),
    fetchAnthropicModelCatalog(),
    readCodexLocalModels(),
    readClaudeLocalModelIds(),
    readClaudeCodeModels(),
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
  for (const model of claudeCodeCatalogModels(data, claudeCode.models)) {
    changed = mergeProviderModel(data, "anthropic", model.id, model.meta) || changed;
  }
  return { changed, claudeCode };
}

/**
 * Every model and alias Claude Code lists, with exactly its effort levels, so
 * the dashboard accepts it and offers the same levels as the picker. A model
 * the catalog already has keeps its other fields. A dated id also gives its
 * levels to the undated twin the pickers list in its place.
 */
function claudeCodeCatalogModels(data: ModelsDevCache, listed: CliModel[]): Array<{ id: string; meta: ModelsDevModel }> {
  const known = data.anthropic?.models ?? {};
  const efforts = new Map<string, EffortId[]>();
  const names = new Map<string, string | undefined>();
  for (const model of listed) {
    efforts.set(model.id, CLAUDE.efforts.filter((level) => model.efforts.includes(level)));
    names.set(model.id, model.name);
  }
  for (const model of listed) {
    const base = /^(.+)-\d{8}$/.exec(model.id)?.[1];
    if (base && known[base] && !efforts.has(base)) {
      efforts.set(base, efforts.get(model.id)!);
    }
  }
  return [...efforts].map(([id, levels]) => ({
    id,
    meta: known[id]
      ? { efforts: levels }
      : { id, name: names.get(id) || claudeModelName(id), tool_call: true, efforts: levels, modalities: { output: ["text"] }, source: "claude-code" },
  }));
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
          name: model.display_name || claudeModelName(model.id!),
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
  return (await readCodexCacheModels())
    .filter((model) => isCodexRelevantModel(model.slug!) && !isExcludedOpenAiTextModel(model.slug!))
    .map((model) => ({
      id: model.slug!,
      meta: {
        id: model.slug,
        name: model.display_name || model.description || model.slug,
        tool_call: true,
        reasoning: isLikelyReasoningModel("codex", model.slug!),
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
        name: claudeModelName(id),
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
    if (!id.startsWith("claude-") || id.includes("3-")) {
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

/** The name a Claude id reads as, the picker's way: "claude-fable-5-2" is "Fable 5.2". */
function claudeModelName(id: string): string {
  return withContext(parseClaudeId(id).name, id);
}

function capitalize(value: string): string {
  return `${value.slice(0, 1).toUpperCase()}${value.slice(1)}`;
}

// The picker's rows ---------------------------------------------------------
//
// One section per harness, listing exactly the models the harness's own CLI
// lists for this account (the catalog adds a detail line and prices, never a
// row). Rows read the name and then the version ("Opus 5.5", "Sol 6.1") and
// sort by the version read from the id, highest first. Models of one version
// go by the harness's tier order: Claude Code's own family order (Opus, Fable,
// Sonnet, Haiku), Codex's families by price (Astra, Sol, Terra, Luna). A CLI
// alias ("opus") closes its section, named and sorted like the model it
// stands for now.

/** A model as its CLI lists it. */
type CliModel = {
  id: string;
  name?: string;
  description?: string;
  /** The effort levels the CLI runs it at; none when it has no effort setting. */
  efforts: string[];
  /** A Claude Code alias ("opus") that always means the family's newest model. */
  alias: boolean;
  /** The model it runs: its own id, or the one an alias stands for now. */
  resolved: string;
};

type ParsedModelId = {
  /** Ids of the harness's main shape, then a second shape (o-series), then ids of no known shape. */
  series: number;
  version: number[];
  family: string;
  /** Read from the id: "Opus 5.5", "Sol 6.1". */
  name: string;
  /** A context window suffix: "claude-opus-5-5[1m]" is "1M". */
  context?: string;
};

/**
 * A harness: an agent CLI Rudder runs, and how the pickers find, name and
 * sort its models. The pickers show one section per HARNESSES row, in that
 * order, so a new harness is a new row.
 */
type Harness = {
  /** The backend id `/model <id> <model>` and the dashboard take. */
  id: BackendId;
  label: string;
  /** What it signs in with, for the picker's account line. */
  account: string;
  /**
   * The catalog provider (models.dev, `models-dev.json`) for detail lines and
   * prices. Null when the CLI's own list carries them (OpenCode's spans every
   * provider).
   */
  catalog: "anthropic" | "openai" | null;
  /** The CLI's own list of models: the rows of its section. */
  listed: (catalog: ModelCatalog) => Promise<CliModel[]>;
  /** The model it runs when given none (HarnessModels.default). */
  defaultModel: (catalog: ModelCatalog) => Promise<string | null>;
  /** The version, family and name an id reads as. */
  parse: (id: string) => ParsedModelId;
  /** Its name for a model, from the CLI's display name. */
  name: (display: string) => string;
  /** How its families rank within one version. */
  tiers: Tiers;
  /** The levels the dashboard passes it (native/src/models.rs effort_options_for). */
  efforts: EffortId[];
};

/** Each family's rank, the highest first. */
type Tiers = (parse: Harness["parse"], cli: CliModel[], catalog: Record<string, ModelsDevModel>) => Map<string, number>;

const CLAUDE: Harness = {
  id: "claude",
  label: "Claude",
  account: "Claude Code",
  catalog: "anthropic",
  listed: async (catalog) => catalog.claudeCode.models,
  defaultModel: async (catalog) => catalog.claudeCode.default,
  parse: parseClaudeId,
  // Claude Code's names already read "Opus 5.5".
  name: (display) => display,
  tiers: tiersByListOrder,
  efforts: ["low", "medium", "high", "xhigh", "max"],
};

const CODEX: Harness = {
  id: "codex",
  label: "Codex",
  account: "ChatGPT",
  catalog: "openai",
  listed: () => readCodexCliModels(),
  defaultModel: () => readCodexConfiguredModel(),
  parse: parseGptId,
  name: gptName,
  // Codex lists its recommended model first, not its flagship.
  tiers: tiersByPrice,
  efforts: ["low", "medium", "high", "xhigh"],
};

const OPENCODE: Harness = {
  id: "opencode",
  label: "OpenCode",
  // OpenCode runs its own free models with no sign-in, and the models of any
  // provider it is signed in to.
  account: "OpenCode",
  catalog: null,
  listed: () => readOpencodeModels(),
  defaultModel: () => readOpencodeConfiguredModel(),
  parse: parseOpencodeId,
  // OpenCode's names already read "Big Pickle", "Claude Sonnet 4.5".
  name: (display) => display,
  // Providers in the order OpenCode lists them.
  tiers: tiersByListOrder,
  // A model's variants of these names (`opencode models --verbose`); the
  // dashboard runs one as the agent's `variant` (native launch.rs).
  efforts: ["low", "medium", "high", "xhigh", "max"],
};

const ANTIGRAVITY: Harness = {
  id: "antigravity",
  label: "Antigravity",
  account: "Google",
  catalog: null,
  listed: readAntigravityModels,
  // agy's list does not say which model it runs when given none.
  defaultModel: async () => null,
  parse: parseAntigravityId,
  // agy's labels already read "Gemini 3.8 Flash (High)".
  name: (display) => display,
  // Models in the order agy lists them.
  tiers: tiersByListOrder,
  // None: agy lists a slug per effort and runs no --effort (native launch.rs).
  efforts: [],
};

const HARNESSES: Harness[] = [CLAUDE, CODEX, OPENCODE, ANTIGRAVITY];

const CONTEXT_SUFFIX = /\[(\d+)([km])\]$/i;

function pickerRows(harness: Harness, catalog: Record<string, ModelsDevModel>, cli: CliModel[]): ModelOption[] {
  const models = new Map<string, CliModel>();
  const aliases: CliModel[] = [];
  for (const model of cli) {
    if (model.alias) {
      aliases.push(model);
    } else if (!models.has(model.id)) {
      models.set(model.id, model);
    }
  }
  const tiers = harness.tiers(harness.parse, cli, catalog);
  // The levels both the CLI lists and the dashboard passes; the dashboard refuses any other.
  const efforts = (model: CliModel) => harness.efforts.filter((level) => model.efforts.includes(level));
  const rows = [...models.values()]
    .map((model) => ({ model, parsed: harness.parse(model.id) }))
    .sort((a, b) => compareModelIds(a.parsed, b.parsed, tiers) || a.model.id.localeCompare(b.model.id))
    .map(({ model, parsed }): ModelOption => ({
      label: modelLabel(harness, model.id, parsed, models),
      value: model.id,
      detail: model.description || catalogDetail(catalog[model.id]),
      efforts: efforts(model),
    }));
  // "opus" reads "Opus 5.5" and sorts where Opus 5.5 does.
  const aliasRows = aliases
    .map((alias) => ({ alias, parsed: harness.parse(alias.resolved) }))
    .sort((a, b) => compareModelIds(a.parsed, b.parsed, tiers) || a.alias.id.localeCompare(b.alias.id))
    .map(({ alias, parsed }): ModelOption => ({
      label: modelLabel(harness, alias.resolved, parsed, models),
      value: alias.id,
      detail: alias.description ?? "",
      efforts: efforts(alias),
      alias: true,
    }));
  return [...rows, ...aliasRows];
}

/** Families in the order the CLI first lists them: Claude Code lists Opus, Fable, Sonnet, Haiku. */
function tiersByListOrder(parse: Harness["parse"], cli: CliModel[]): Map<string, number> {
  const families = [...new Set(cli.map((model) => parse(model.resolved).family))];
  return new Map(families.map((family, index) => [family, families.length - index]));
}

/** Each family's highest output price in the catalog: Astra before Sol before Terra before Luna. */
function tiersByPrice(parse: Harness["parse"], _cli: CliModel[], catalog: Record<string, ModelsDevModel>): Map<string, number> {
  const prices = new Map<string, number>();
  for (const [id, model] of Object.entries(catalog)) {
    const price = model.cost?.output;
    if (typeof price === "number") {
      const { family } = parse(id);
      prices.set(family, Math.max(prices.get(family) ?? 0, price));
    }
  }
  return prices;
}

function splitContext(id: string): { base: string; context?: string } {
  const suffix = CONTEXT_SUFFIX.exec(id);
  return suffix ? { base: id.slice(0, suffix.index), context: `${suffix[1]}${suffix[2].toUpperCase()}` } : { base: id };
}

/** An id of no known shape reads as its words, without a provider prefix: "anthropic/my-model" is "My Model". */
function unknownId(base: string, context?: string): ParsedModelId {
  return { series: 2, version: [], family: base, name: words(base.slice(base.lastIndexOf("/") + 1)), context };
}

/**
 * "opencode/big-pickle" reads "Big Pickle". OpenCode's ids carry no version
 * Rudder can read across providers, so each provider is a family, ranked in
 * list order, and its models go by id.
 */
function parseOpencodeId(id: string): ParsedModelId {
  const { base, context } = splitContext(id);
  const slash = base.indexOf("/");
  return { ...unknownId(base, context), series: 0, family: slash > 0 ? base.slice(0, slash) : base };
}

/**
 * An Antigravity model ID reads as words when agy gives it no label. agy's
 * list spans model makers, so each model is its own family, ranked in list order.
 */
function parseAntigravityId(id: string): ParsedModelId {
  const { base, context } = splitContext(id);
  return { series: 0, version: [], family: base, name: words(base), context };
}

/** "claude-opus-5-5" is Opus 5.5, "claude-haiku-4-5-20251001" Haiku 4.5. */
function parseClaudeId(id: string): ParsedModelId {
  const { base, context } = splitContext(id);
  const claude = /^claude-([a-z]+)((?:-\d{1,2})+)(?:-\d{8})?$/.exec(base);
  if (!claude) {
    return unknownId(base, context);
  }
  const version = claude[2].slice(1).split("-").map(Number);
  return { series: 0, version, family: claude[1], name: `${capitalize(claude[1])} ${version.join(".")}`, context };
}

/** "gpt-6.1-sol" is Sol 6.1, "gpt-5.3-codex-spark" Codex Spark 5.3; "gpt-5.6" has no family word. */
function parseGptId(id: string): ParsedModelId {
  const { base, context } = splitContext(id);
  const gpt = /^gpt-(\d+(?:\.\d+)?)(?:-([a-z][a-z0-9-]*))?$/.exec(base);
  if (gpt) {
    const family = gpt[2] ?? "";
    return { series: 0, version: gpt[1].split(".").map(Number), family, name: family ? `${words(family)} ${gpt[1]}` : `GPT-${gpt[1]}`, context };
  }
  const oSeries = /^o(\d+)(?:-([a-z][a-z0-9-]*))?$/.exec(base);
  if (oSeries) {
    const family = oSeries[2] ?? "";
    return { series: 1, version: [Number(oSeries[1])], family: `o:${family}`, name: family ? `${words(family)} o${oSeries[1]}` : `o${oSeries[1]}`, context };
  }
  return unknownId(base, context);
}

/** Codex's display name, the name and then the version: "GPT-6.2-Sol" and "GPT-6.2 Sol" read "Sol 6.2". */
function gptName(display: string): string {
  const named = /^GPT-(\d+(?:\.\d+)?)[- ](.+)$/i.exec(display);
  return named ? `${capitalize(named[2].replace(/-/g, " "))} ${named[1]}` : display;
}

function words(family: string): string {
  return family.split(/[-_]/).filter(Boolean).map(capitalize).join(" ");
}

/** The highest version first; within one, the family of the highest tier first. */
function compareModelIds(a: ParsedModelId, b: ParsedModelId, tiers: Map<string, number>): number {
  if (a.series !== b.series) {
    return a.series - b.series;
  }
  for (let i = 0; i < Math.max(a.version.length, b.version.length); i += 1) {
    const diff = (b.version[i] ?? 0) - (a.version[i] ?? 0);
    if (diff !== 0) {
      return diff;
    }
  }
  return (tiers.get(b.family) ?? 0) - (tiers.get(a.family) ?? 0) || Number(!!a.context) - Number(!!b.context);
}

/** The harness's name for the CLI's display name, or the name read from the id. */
function modelLabel(harness: Harness, id: string, parsed: ParsedModelId, listed: Map<string, CliModel>): string {
  const own = listed.get(id.replace(CONTEXT_SUFFIX, ""))?.name?.trim();
  return withContext(own ? harness.name(own) : parsed.name, id);
}

/** "Opus 5.5" of "claude-opus-5-5[1m]" reads "Opus 5.5 · 1M". */
function withContext(name: string, id: string): string {
  const suffix = CONTEXT_SUFFIX.exec(id);
  return suffix ? `${name} · ${suffix[1]}${suffix[2].toUpperCase()}` : name;
}

/**
 * The name the app shows for any model an agent runs on. The home names every
 * model it sends with this (cloud/worker/home.mjs), so the app never names
 * one itself. A model its harness lists reads as its picker row does (an
 * alias like the model it stands for: "opus" is "Opus 5.5"); any other id
 * reads as the rows' rules read it ("claude-opus-4-1" is "Opus 4.1", "gpt-6.1-sol"
 * Sol 6.1, "my-model" My Model). No id is the model the harness runs when
 * given none (HarnessModels.default), and null when it does not say which.
 * `harnesses` is the last discoverHarnesses() answer, empty before the first.
 */
export function modelName(harnesses: HarnessModels[], backend: string, id: string | null): string | null {
  const listed = harnesses.find((harness) => harness.id === backend);
  if (!id) {
    return listed?.default ? modelName(harnesses, backend, listed.default) : null;
  }
  const row = listed?.models.find((model) => model.value === id);
  if (row) {
    return row.label;
  }
  const harness = HARNESSES.find((entry) => entry.id === backend);
  return withContext(harness ? harness.parse(id).name : unknownId(splitContext(id).base).name, id);
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

/** Claude Code's own /model list, and the model its "default" row runs. */
type ClaudeCodeList = { models: CliModel[]; default: string | null };

/**
 * Claude Code's own /model list for this account, with each alias's current
 * model, display names and the effort levels each model takes. Each alias
 * also lists the model it stands for. Empty when Claude Code is not installed.
 * Its "default" row is not a model of its own: it names the model an agent
 * started without one runs.
 */
async function readClaudeCodeModels(): Promise<ClaudeCodeList> {
  const answer = await claudeCodeModelList();
  const models: CliModel[] = [];
  let runsByDefault: string | null = null;
  for (const row of answer ?? []) {
    if (row.value === "default") {
      runsByDefault = row.resolvedModel?.startsWith("claude-") ? row.resolvedModel : null;
      continue;
    }
    if (!row.value) {
      continue;
    }
    const efforts = row.supportsEffort ? row.supportedEffortLevels ?? [] : [];
    if (row.value.startsWith("claude-")) {
      models.push({ id: row.value, name: row.displayName, description: row.description, efforts, alias: false, resolved: row.value });
      continue;
    }
    // An alias is named and placed like the model it stands for.
    if (!row.resolvedModel?.startsWith("claude-")) {
      throw new Error(`Claude Code listed the model alias "${row.value}" without the Claude model it stands for`);
    }
    models.push({ id: row.value, name: row.displayName, description: row.description, efforts, alias: true, resolved: row.resolvedModel });
    models.push({ id: row.resolvedModel, name: row.displayName, description: row.description, efforts, alias: false, resolved: row.resolvedModel });
  }
  return { models, default: runsByDefault };
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
