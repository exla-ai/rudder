// Turns a goal into 3 to 10 small, independently shippable tasks for Linear.
//
// OpenAI first: the Responses API with RUDDER_TASKGEN_MODEL (default
// gpt-6-luna, OPENAI_API_KEY), no reasoning, and a strict JSON schema. When
// OPENAI_API_KEY is unset or that call fails, Claude Haiku 5.5 through the
// Anthropic SDK with the same schema as a structured output
// (ANTHROPIC_API_KEY, plus the anthropic-workspace-id header when
// ANTHROPIC_WORKSPACE_ID is set). Whatever comes back is validated again here
// before any issue is created. HTTP goes through an injected fetch so tests
// never reach either API. Keys never reach a log or an error message.

import Anthropic from "@anthropic-ai/sdk";

export const TASKGEN_DEFAULT_MODEL = "gpt-6-luna";
export const TASKGEN_FALLBACK_MODEL = "claude-haiku-5-5";
const OPENAI_RESPONSES_URL = "https://api.openai.com/v1/responses";
const TIMEOUT_MS = 60_000;

export const MIN_TASKS = 3;
export const MAX_TASKS = 10;
export const GOAL_LIMIT = 4000;
const TITLE_LIMIT = 200;
const DESCRIPTION_LIMIT = 4000;
const DONE_WHEN_LIMIT = 1000;

export type GeneratedTask = { title: string; description: string; doneWhen: string };
export type TaskgenProvider = "openai" | "anthropic";

export class TaskgenError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

/** Structured-output schema, shared by both providers (strict mode needs every key required). */
export const TASKS_SCHEMA = {
  type: "object",
  properties: {
    tasks: {
      type: "array",
      items: {
        type: "object",
        properties: {
          title: { type: "string" },
          description: { type: "string" },
          done_when: { type: "string" },
        },
        required: ["title", "description", "done_when"],
        additionalProperties: false,
      },
    },
  },
  required: ["tasks"],
  additionalProperties: false,
} as const;

export const TASKGEN_SYSTEM = [
  "You plan software work for coding agents. Each task you write becomes one Linear issue, and one coding agent picks it up and works on it alone in the repository.",
  `Break the goal into ${MIN_TASKS} to ${MAX_TASKS} small tasks. Use as few as the goal needs.`,
  "Every task must ship on its own: one agent can finish it in a single session, it does not wait for another task in the list, and the product still works after it merges.",
  "title: an imperative summary under 80 characters, such as \"Add retry to the upload client\".",
  "description: what to change and why, with the context an agent needs to start without asking. Plain sentences, no markdown headings.",
  "done_when: one concrete condition someone can check, such as a test that passes, a command that succeeds, or what a page shows.",
].join("\n");

export function taskgenUserPrompt(goal: string, project?: { name: string; repo?: string | null } | null): string {
  const where = project ? `Project: ${project.name}${project.repo ? ` (${project.repo})` : ""}\n\n` : "";
  return `${where}<goal>\n${goal}\n</goal>`;
}

export function taskgenConfigured(env: NodeJS.ProcessEnv): boolean {
  return Boolean((env.OPENAI_API_KEY ?? "").trim() || (env.ANTHROPIC_API_KEY ?? "").trim());
}

/** Linear issue description: the task's description, then its done-when line. */
export function linearIssueDescription(task: GeneratedTask): string {
  return `${task.description}\n\nDone when: ${task.doneWhen}`;
}

function field(raw: Record<string, unknown>, key: string, limit: number, index: number): string {
  const value = raw[key];
  if (typeof value !== "string" || !value.trim()) {
    throw new TaskgenError(502, `task ${index + 1} has no ${key}`);
  }
  const text = value.trim();
  if (text.length > limit) {
    throw new TaskgenError(502, `task ${index + 1} ${key} is longer than ${limit} characters`);
  }
  return text;
}

/** Strict: exactly {tasks: [...]}, 3 to 10 tasks, three non-empty strings each, no extra keys, no repeated titles. */
export function parseGeneratedTasks(text: string): GeneratedTask[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw new TaskgenError(502, "the model did not return JSON");
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new TaskgenError(502, "the model did not return a task list");
  }
  const root = parsed as Record<string, unknown>;
  if (Object.keys(root).some((key) => key !== "tasks") || !Array.isArray(root.tasks)) {
    throw new TaskgenError(502, "the model did not return a task list");
  }
  const list = root.tasks as unknown[];
  if (list.length < MIN_TASKS || list.length > MAX_TASKS) {
    throw new TaskgenError(502, `the model returned ${list.length} tasks, not ${MIN_TASKS} to ${MAX_TASKS}`);
  }
  const seen = new Set<string>();
  return list.map((item, index) => {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      throw new TaskgenError(502, `task ${index + 1} is not an object`);
    }
    const raw = item as Record<string, unknown>;
    if (Object.keys(raw).some((key) => key !== "title" && key !== "description" && key !== "done_when")) {
      throw new TaskgenError(502, `task ${index + 1} has unexpected fields`);
    }
    const task = {
      title: field(raw, "title", TITLE_LIMIT, index),
      description: field(raw, "description", DESCRIPTION_LIMIT, index),
      doneWhen: field(raw, "done_when", DONE_WHEN_LIMIT, index),
    };
    const key = task.title.toLowerCase();
    if (seen.has(key)) {
      throw new TaskgenError(502, `the model repeated the task "${task.title}"`);
    }
    seen.add(key);
    return task;
  });
}

function scrub(message: string, secrets: string[]): string {
  let out = message;
  for (const secret of secrets) {
    if (secret) {
      out = out.split(secret).join("[key]");
    }
  }
  return out.replace(/\b(sk-[A-Za-z0-9_*-]{6,})/g, "[key]").slice(0, 300);
}

async function viaOpenAI(params: { apiKey: string; model: string; fetch: typeof fetch; prompt: string }): Promise<string> {
  let res: Response;
  try {
    res = await params.fetch(OPENAI_RESPONSES_URL, {
      method: "POST",
      headers: { Authorization: `Bearer ${params.apiKey}`, "content-type": "application/json" },
      body: JSON.stringify({
        model: params.model,
        instructions: TASKGEN_SYSTEM,
        input: params.prompt,
        reasoning: { effort: "none" },
        text: { format: { type: "json_schema", name: "linear_tasks", strict: true, schema: TASKS_SCHEMA } },
      }),
      signal: AbortSignal.timeout(TIMEOUT_MS),
    });
  } catch (error) {
    throw new TaskgenError(502, `OpenAI could not be reached: ${scrub(error instanceof Error ? error.message : String(error), [params.apiKey])}`);
  }
  const data = (await res.json().catch(() => ({}))) as {
    error?: { message?: string } | null;
    status?: string;
    incomplete_details?: { reason?: string } | null;
    output?: Array<{ type?: string; content?: Array<{ type?: string; text?: string; refusal?: string }> }>;
  };
  if (!res.ok) {
    if (res.status === 401 || res.status === 403) {
      throw new TaskgenError(502, "OpenAI rejected the API key");
    }
    throw new TaskgenError(502, `OpenAI could not plan the tasks: ${scrub(data.error?.message ?? `HTTP ${res.status}`, [params.apiKey])}`);
  }
  const parts = (data.output ?? []).filter((item) => item.type === "message").flatMap((item) => item.content ?? []);
  if (parts.some((part) => part.type === "refusal")) {
    throw new TaskgenError(422, "OpenAI declined to plan this goal. Try rewording it.");
  }
  if (data.status === "incomplete") {
    throw new TaskgenError(502, `OpenAI stopped before finishing the tasks (${data.incomplete_details?.reason ?? "incomplete"}). Try a smaller goal.`);
  }
  return parts.map((part) => (part.type === "output_text" ? part.text ?? "" : "")).join("");
}

async function viaAnthropic(params: { apiKey: string; workspaceId: string; fetch: typeof fetch; prompt: string }): Promise<string> {
  const client = new Anthropic({
    apiKey: params.apiKey,
    fetch: params.fetch,
    timeout: TIMEOUT_MS,
    maxRetries: 2,
    // A user key works only inside its workspace, named by this header.
    ...(params.workspaceId ? { defaultHeaders: { "anthropic-workspace-id": params.workspaceId } } : {}),
  });
  let message: Anthropic.Message;
  try {
    message = await client.messages.create({
      model: TASKGEN_FALLBACK_MODEL,
      max_tokens: 16000,
      system: TASKGEN_SYSTEM,
      messages: [{ role: "user", content: params.prompt }],
      output_config: { effort: "medium", format: { type: "json_schema", schema: TASKS_SCHEMA } },
    });
  } catch (error) {
    if (error instanceof Anthropic.AuthenticationError || error instanceof Anthropic.PermissionDeniedError) {
      throw new TaskgenError(502, "Anthropic rejected the API key");
    }
    const message = error instanceof Error ? error.message : String(error);
    throw new TaskgenError(502, `Claude could not plan the tasks: ${scrub(message, [params.apiKey])}`);
  }
  if (message.stop_reason === "refusal") {
    throw new TaskgenError(422, "Claude declined to plan this goal. Try rewording it.");
  }
  if (message.stop_reason === "max_tokens") {
    throw new TaskgenError(502, "Claude ran out of room writing the tasks. Try a smaller goal.");
  }
  return message.content.map((block) => (block.type === "text" ? block.text : "")).join("");
}

export async function generateTasks(params: {
  env: NodeJS.ProcessEnv;
  fetch: typeof fetch;
  goal: string;
  project?: { name: string; repo?: string | null } | null;
  log?: (message: string) => void;
}): Promise<{ tasks: GeneratedTask[]; provider: TaskgenProvider; model: string }> {
  const openaiKey = (params.env.OPENAI_API_KEY ?? "").trim();
  const anthropicKey = (params.env.ANTHROPIC_API_KEY ?? "").trim();
  if (!openaiKey && !anthropicKey) {
    throw new TaskgenError(503, "Task generation is not set up on this server (OPENAI_API_KEY is missing)");
  }
  const prompt = taskgenUserPrompt(params.goal, params.project);
  if (openaiKey) {
    const model = (params.env.RUDDER_TASKGEN_MODEL ?? "").trim() || TASKGEN_DEFAULT_MODEL;
    try {
      const text = await viaOpenAI({ apiKey: openaiKey, model, fetch: params.fetch, prompt });
      return { tasks: parseGeneratedTasks(text), provider: "openai", model };
    } catch (error) {
      if (!anthropicKey) {
        throw error;
      }
      // Messages here are already scrubbed of keys.
      params.log?.(`linear taskgen: OpenAI failed (${error instanceof Error ? error.message : String(error)}), using Claude`);
    }
  }
  const text = await viaAnthropic({
    apiKey: anthropicKey,
    workspaceId: (params.env.ANTHROPIC_WORKSPACE_ID ?? "").trim(),
    fetch: params.fetch,
    prompt,
  });
  return { tasks: parseGeneratedTasks(text), provider: "anthropic", model: TASKGEN_FALLBACK_MODEL };
}
