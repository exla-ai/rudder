// Talking to Rudder (the phone's voice screen and CarPlay).
//
// The phone holds the audio: it speaks to OpenAI's realtime model directly
// over a WebSocket with a short-lived client secret minted here, so no audio
// passes through the control plane. Everything the model may DO is a tool
// call the phone relays to /api/app/voice/tool, which runs it here against
// the account's home machine with the same RPCs the app's buttons use. So
// the model only knows and changes what the account can, and the session's
// instructions and tools live in one place (this file), not in the app.
//
// Pure apart from the VoiceContext it is handed, so tests drive it with fakes.

export type VoiceAsk = {
  id: string;
  tool?: string;
  kind?: string;
  input?: unknown;
  suggestions?: unknown[];
};

export type VoiceAgent = {
  id: string;
  title?: string | null;
  task?: string | null;
  status?: string | null;
  phase?: string | null;
  backend?: string | null;
  model?: string | null;
  createdAt?: number | null;
  updatedAt?: number | null;
  lastMessage?: string | null;
  /** A task typed a moment ago whose run record does not exist yet. */
  pending?: boolean;
  summary?: string | null;
  asks?: VoiceAsk[];
};

/** A project's /plan task graph (cloud/worker/plan.mjs). */
export type VoicePlan = {
  nodes?: Array<{ id: string; title?: string | null; status?: string | null }>;
  awaitingApproval?: boolean;
  plans?: Array<{ id: string; awaitingApproval?: boolean; request?: string | null }>;
};

export type VoiceProject = {
  workspaceId: string;
  name: string;
  repo?: string | null;
  status?: string | null;
  dashboard?: string | null;
  failureReason?: string | null;
  agents: VoiceAgent[];
  plan?: VoicePlan | null;
};

export type VoiceContext = {
  /** The account's projects as the app sees them right now. */
  projects(): VoiceProject[];
  /** One RPC to the home machine (woken first when asleep). */
  rpc(method: string, params: Record<string, unknown>, timeoutMs?: number): Promise<unknown>;
  /** Clone a GitHub repo onto the home machine and open it. */
  openRepo(repo: string): Promise<unknown>;
  /** What this conversation did, so "tell it to..." right after a start_task reaches the new agent. */
  memory?: VoiceMemory;
  log?: (line: string) => void;
  /** How long to wait for a just-started agent's run to appear (tests shorten it). */
  startWaitMs?: number;
  /** Told about every tool call (product analytics); ok is false when it threw. */
  onTool?: (name: string, ok: boolean) => void;
};

export type VoiceMemory = {
  /** The task this conversation started last. */
  started?: { workspaceId: string; text: string; at: number; runId?: string };
};

/** A started task's follow-ups go to its agent for this long. */
const STARTED_FOCUS_MS = 10 * 60_000;

export type VoiceToolResult = { output: string; end?: boolean };

// "call": a phone call to Rudder's number (cloud/src/voice-call.ts), usually from a car.
export type VoiceSurface = "carplay" | "phone" | "call";

type ToolDef = {
  type: "function";
  name: string;
  description: string;
  parameters: Record<string, unknown>;
};

const projectParam = {
  type: "string",
  description: "Project name or repo (for example \"libra\" or \"exla-ai/libra\"). Leave out when there is only one project.",
};
const agentParam = {
  type: "string",
  description: "Which agent: words from its title or task, or its id. \"just started\" means the agent your last start_task created. Leave out to mean the one that needs the user (or the only one).",
};
const userWords = "The user's own words, as close to verbatim as you can: drop filler like um and fix obvious speech-to-text slips, but never reinterpret (\"test site\" stays \"test site\"), expand, or add steps of your own.";

export const VOICE_TOOLS: ToolDef[] = [
  {
    type: "function",
    name: "get_overview",
    description: "Fresh status of every project and agent: what each is doing, which need the user and why, and the cloud machine. Call this before answering anything about current state.",
    parameters: { type: "object", properties: {}, additionalProperties: false },
  },
  {
    type: "function",
    name: "get_agent",
    description: "One agent in detail: its task, status, the prompt it is waiting on (with options), and its latest messages and tool calls. Use it to explain what an agent did or is stuck on.",
    parameters: {
      type: "object",
      properties: { project: projectParam, agent: agentParam },
      additionalProperties: false,
    },
  },
  {
    type: "function",
    name: "send_message",
    description: "Send a message to a running or finished agent, exactly as if the user typed it to that agent. Use for instructions, follow-ups and corrections to existing work. A follow-up about a task you just started goes to agent \"just started\" (or leave agent out).",
    parameters: {
      type: "object",
      properties: {
        project: projectParam,
        agent: agentParam,
        text: { type: "string", description: `The message. ${userWords}` },
      },
      required: ["text"],
      additionalProperties: false,
    },
  },
  {
    type: "function",
    name: "start_task",
    description: "Start new work in a project as a new agent. Use it for new work, not for a follow-up to an existing agent.",
    parameters: {
      type: "object",
      properties: {
        project: projectParam,
        text: { type: "string", description: `The task, with everything the user said about it. ${userWords}` },
      },
      required: ["text"],
      additionalProperties: false,
    },
  },
  {
    type: "function",
    name: "answer_prompt",
    description: "Answer what an agent is waiting on: a permission request, its questions, or a plan to approve.",
    parameters: {
      type: "object",
      properties: {
        project: projectParam,
        agent: agentParam,
        decision: {
          type: "string",
          enum: ["allow", "allow_always", "deny", "approve_plan", "revise_plan", "answer_questions"],
          description: "allow / allow_always / deny for a permission; approve_plan / revise_plan for a plan; answer_questions for questions.",
        },
        message: { type: "string", description: "For deny or revise_plan: what to do instead." },
        prompt_id: { type: "string", description: "The prompt_id of the exact prompt, from an update or get_agent. Leave out when the agent has only one." },
        answers: {
          type: "array",
          items: { type: "string" },
          description: "For answer_questions: one answer per question, in order. Use the option's exact label when the user picks one; otherwise their words.",
        },
      },
      required: ["decision"],
      additionalProperties: false,
    },
  },
  {
    type: "function",
    name: "agent_action",
    description: "stop: stop the agent. interrupt: interrupt its current turn (like Esc) but keep it. merge: merge or publish its finished work. delete: remove the agent (only after the user said yes to deleting that exact agent). One agent per call.",
    parameters: {
      type: "object",
      properties: {
        project: projectParam,
        agent: agentParam,
        action: { type: "string", enum: ["stop", "interrupt", "merge", "delete"] },
        confirmed: { type: "boolean", description: "For delete: true only once the user has said yes to deleting this exact agent, named back to them." },
      },
      required: ["action"],
      additionalProperties: false,
    },
  },
  {
    type: "function",
    name: "launch_plan",
    description: "Approve a Rudder plan that is waiting for approval, so its agents start (the same as Approve in the app). A Rudder plan is the task graph a /plan request produced. Not for one agent's own plan prompt: that is answer_prompt.",
    parameters: {
      type: "object",
      properties: {
        project: projectParam,
        plan: { type: "string", description: "Words from the plan's request. Leave out when only one plan is waiting." },
      },
      additionalProperties: false,
    },
  },
  {
    type: "function",
    name: "find_repos",
    description: "Search the user's GitHub repos that can be opened as a project on the cloud machine.",
    parameters: {
      type: "object",
      properties: { query: { type: "string", description: "Words from the repo name or owner." } },
      additionalProperties: false,
    },
  },
  {
    type: "function",
    name: "open_project",
    description: "Open a GitHub repo as a project on the cloud machine (it is cloned there).",
    parameters: {
      type: "object",
      properties: { repo: { type: "string", description: "owner/name, from find_repos." } },
      required: ["repo"],
      additionalProperties: false,
    },
  },
  {
    type: "function",
    name: "end_conversation",
    description: "End the voice session when the user is done (says bye, thanks that's all, stop listening).",
    parameters: { type: "object", properties: {}, additionalProperties: false },
  },
];

// ---------------------------------------------------------------------------
// What the model is told.
// ---------------------------------------------------------------------------

export function voiceInstructions(projects: VoiceProject[], surface: VoiceSurface, focus?: string): string {
  const lines = [
    "You are Rudder, a voice assistant for a developer's coding agents. Rudder runs Claude Code and Codex agents on the user's cloud machine, one dashboard per GitHub project. You can see every project and agent, explain what they are doing, and act for the user: message an agent, start a task, answer an agent's permission request, questions or plan, launch a Rudder plan that is waiting for approval, interrupt, stop, merge or delete an agent, and open a new project.",
    surface === "carplay"
      ? "The user is DRIVING and talking to you through CarPlay. Keep every reply short (one to three spoken sentences), never read out code, file paths, long ids or URLs, and never ask them to look at a screen."
      : surface === "call"
        ? "The user PHONED you, most likely while DRIVING. Keep every reply short (one to three spoken sentences), never read out code, file paths, long ids or URLs, and never ask them to look at a screen. Ending the conversation hangs up the call."
        : "The user is talking to you on their phone. Keep replies short and spoken; never read out code, file paths, long ids or URLs.",
    "Speak naturally, like a sharp colleague giving a status update. Summarize; lead with what needs the user. Say agent names by their short title, never their id.",
    "Always call get_overview or get_agent before describing current state; the snapshot below may be stale. Do what the user asks right away without asking for confirmation, then say in a few words what you did and to which agent and project, so the user knows exactly where it went. The one exception is delete: name the agent and project and get a yes first. If a request is ambiguous between agents or projects, ask one short question.",
    "When the user dictates a message or task, pass on their own words: do not reinterpret, expand or add steps. A follow-up right after you started a task is for that new agent (agent \"just started\"), not an older one.",
    "Only say something happened when a tool result says so. If a tool says it did not work, or that it is queued or still starting, tell the user that plainly.",
    "The user speaks English. Speech that is not English is background noise: ignore it.",
    "When the user is done, say a brief goodbye and call end_conversation.",
    "",
    "Snapshot when this conversation started:",
    overviewText(projects),
  ];
  if (focus) {
    lines.push("", `The user opened Rudder on: ${clip(focus, 300)}. Start with that.`);
  }
  return lines.join("\n");
}

/** The realtime session, minted with the client secret so the phone sends no config of its own. */
export function voiceSessionConfig(options: {
  model: string;
  voice: string;
  instructions: string;
  transcribeModel?: string | null;
  minimal?: boolean;
  /** Transcription language (ISO-639-1); keeps road noise from being captioned as Korean. */
  language?: string | null;
  /** A phone call: SIP negotiates the audio format, so none is named. */
  sip?: boolean;
}): Record<string, unknown> {
  const input: Record<string, unknown> = {
    turn_detection: { type: "semantic_vad" },
  };
  const output: Record<string, unknown> = { voice: options.voice };
  if (!options.sip) {
    input.format = { type: "audio/pcm", rate: 24000 };
    output.format = { type: "audio/pcm", rate: 24000 };
  }
  if (!options.minimal) {
    // A car is a far-field microphone; transcripts feed the phone's captions.
    input.noise_reduction = { type: "far_field" };
    if (options.transcribeModel) {
      input.transcription = options.language
        ? { model: options.transcribeModel, language: options.language }
        : { model: options.transcribeModel };
    }
  }
  return {
    type: "realtime",
    model: options.model,
    instructions: options.instructions,
    output_modalities: ["audio"],
    audio: {
      input,
      output,
    },
    tools: VOICE_TOOLS,
    tool_choice: "auto",
  };
}

/** A one-line greeting the phone asks the model to open with. */
export function voiceGreeting(projects: VoiceProject[], focus?: string): string {
  if (focus) {
    return `Tell the user in one sentence what ${clip(focus, 200)} needs, then ask what to do.`;
  }
  const waiting = projects.flatMap((project) => project.agents.filter(needsUser));
  if (waiting.length === 0) {
    return "Say hi in a few words and ask what they want to do. Do not list anything yet.";
  }
  return `Say hi and tell them in one sentence that ${waiting.length === 1 ? "one agent needs" : `${waiting.length} agents need`} them, naming ${waiting.length === 1 ? "it" : "the first"} briefly, then ask what to do.`;
}

// ---------------------------------------------------------------------------
// Running a tool call.
// ---------------------------------------------------------------------------

export async function runVoiceTool(ctx: VoiceContext, name: string, rawArgs: unknown): Promise<VoiceToolResult> {
  let ok = true;
  try {
    return await runTool(ctx, name, rawArgs, () => { ok = false; });
  } finally {
    try {
      ctx.onTool?.(name, ok);
    } catch {
      // Analytics never breaks a conversation.
    }
  }
}

async function runTool(ctx: VoiceContext, name: string, rawArgs: unknown, failed: () => void): Promise<VoiceToolResult> {
  let args: Record<string, unknown>;
  try {
    args = parseArgs(rawArgs);
  } catch {
    return { output: "Those tool arguments were not valid JSON. Try again." };
  }
  try {
    switch (name) {
      case "get_overview":
        return { output: overviewText(ctx.projects()) };
      case "get_agent":
        return await getAgent(ctx, args);
      case "send_message":
        return await sendMessage(ctx, args);
      case "start_task":
        return await startTask(ctx, args);
      case "answer_prompt":
        return await answerPrompt(ctx, args);
      case "agent_action":
        return await agentAction(ctx, args);
      case "launch_plan":
        return await launchPlan(ctx, args);
      case "find_repos":
        return await findRepos(ctx, args);
      case "open_project": {
        const repo = str(args.repo).replace(/^https:\/\/github\.com\//, "").replace(/\.git$/, "");
        await ctx.openRepo(repo);
        return { output: `Opening ${repo} on the cloud machine. It is being cloned; its agents can take tasks in a minute.` };
      }
      case "end_conversation":
        return { output: "Ending the conversation.", end: true };
      default:
        return { output: `There is no tool called ${name}.` };
    }
  } catch (error) {
    if (error instanceof Resolution) {
      return { output: error.message };
    }
    failed();
    const reason = error instanceof Error ? error.message : String(error);
    return { output: `That did not work: ${clip(reason, 300)}` };
  }
}

async function getAgent(ctx: VoiceContext, args: Record<string, unknown>): Promise<VoiceToolResult> {
  const project = resolveProject(ctx.projects(), str(args.project));
  const agent = resolveAgent(project, str(args.agent));
  const lines = [`${agentLine(agent)} in ${project.name}.`];
  if (agent.task && agent.task !== agent.title) {
    lines.push(`Task: ${clip(agent.task, 600)}`);
  }
  for (const ask of agent.asks ?? []) {
    lines.push(`Waiting on: ${describeAsk(ask)}`);
  }
  if (agent.summary) {
    lines.push(`Its report: ${clip(agent.summary, 800)}`);
  }
  try {
    const transcript = (await ctx.rpc("transcript", { workspaceId: project.workspaceId, runId: agent.id, limit: 40 }, 20_000)) as {
      messages?: Array<{ role?: string; text?: string; tool?: { name?: string; input?: unknown } }>;
    };
    const recent = (transcript?.messages ?? []).slice(-14).map(transcriptLine).filter(Boolean);
    if (recent.length) {
      lines.push("Latest activity, oldest first:", ...recent);
    }
  } catch (error) {
    if (agent.lastMessage) {
      lines.push(`Its last message: ${clip(agent.lastMessage, 800)}`);
    }
    lines.push(`(The full transcript could not be read: ${clip(error instanceof Error ? error.message : String(error), 120)})`);
  }
  return { output: lines.join("\n") };
}

async function sendMessage(ctx: VoiceContext, args: Record<string, unknown>): Promise<VoiceToolResult> {
  const text = str(args.text).trim().slice(0, 16_000);
  if (!text) {
    return { output: "What should the message say?" };
  }
  const started = startedTarget(ctx, args);
  if (started) {
    const found = await waitForStarted(ctx, started);
    if (!found) {
      // Its run record is still being made: deliver the moment it exists.
      void deliverWhenStarted(ctx, started, text);
      return { output: `The new agent in ${projectName(ctx, started.workspaceId)} is still starting up; your message will reach it the moment it starts. Tell the user that, in a few words.` };
    }
    return delivered(ctx, found.project, found.agent, text);
  }
  const project = resolveProject(ctx.projects(), str(args.project));
  return delivered(ctx, project, resolveAgent(project, str(args.agent)), text);
}

async function delivered(ctx: VoiceContext, project: VoiceProject, agent: VoiceAgent, text: string): Promise<VoiceToolResult> {
  const result = (await ctx.rpc("send", { workspaceId: project.workspaceId, runId: agent.id, text })) as { status?: string } | null;
  const name = `"${shortTitle(agent)}" (${STATUS_WORDS[agent.status ?? ""] ?? agent.status ?? "unknown"}) in ${project.name}`;
  ctx.log?.(`sent to ${agent.id} in ${project.name}: ${result?.status ?? "?"}`);
  if (result?.status === "queued") {
    return { output: `Queued for ${name}: "${clip(text, 200)}". Its dashboard has not taken it yet; it will on its next check. Tell the user it is queued for that agent.` };
  }
  return { output: `Delivered to ${name}: "${clip(text, 200)}". Tell the user in a few words which agent got it.` };
}

async function startTask(ctx: VoiceContext, args: Record<string, unknown>): Promise<VoiceToolResult> {
  const text = str(args.text).trim().slice(0, 16_000);
  if (!text) {
    return { output: "What is the task?" };
  }
  const project = resolveProject(ctx.projects(), str(args.project));
  const at = Date.now();
  const result = (await ctx.rpc("task", { workspaceId: project.workspaceId, text })) as { status?: string } | null;
  if (ctx.memory) {
    ctx.memory.started = { workspaceId: project.workspaceId, text, at };
  }
  const queued = result?.status === "queued" ? " Its dashboard has not picked it up yet; it will on its next check." : "";
  return { output: `Started a new agent in ${project.name} for: "${clip(text, 200)}".${queued} Follow-ups for it go to agent "just started".` };
}

const JUST_STARTED = /^(the )?(one|agent|task)?\s*(you |i |we )?(just )?(started|created|new|newest one|new one|new agent|new task)( one| agent| task)?$|^just started$/;

/** The task this conversation just started, when the call is about it. */
function startedTarget(ctx: VoiceContext, args: Record<string, unknown>): NonNullable<VoiceMemory["started"]> | null {
  const started = ctx.memory?.started;
  if (!started || Date.now() - started.at > STARTED_FOCUS_MS) {
    return null;
  }
  const query = str(args.agent).trim().toLowerCase();
  const named = query !== "" && JUST_STARTED.test(query);
  if (!named && query !== "") {
    return null;
  }
  const project = str(args.project).trim();
  if (project) {
    try {
      if (resolveProject(ctx.projects(), project).workspaceId !== started.workspaceId) {
        return null;
      }
    } catch {
      return null;
    }
  }
  return started;
}

function findStarted(ctx: VoiceContext, started: NonNullable<VoiceMemory["started"]>): { project: VoiceProject; agent: VoiceAgent } | null {
  const project = ctx.projects().find((candidate) => candidate.workspaceId === started.workspaceId);
  if (!project) {
    return null;
  }
  const real = project.agents.filter((agent) => !agent.pending);
  if (started.runId) {
    const agent = real.find((candidate) => candidate.id === started.runId);
    return agent ? { project, agent } : null;
  }
  const words = normalized(started.text).slice(0, 80);
  const byTask = real.filter((agent) => words && normalized(agent.task ?? "").includes(words));
  const newer = real.filter((agent) => (agent.createdAt ?? 0) >= started.at - 2_000);
  const agent = byTask.length === 1 ? byTask[0] : newer.length === 1 ? newer[0] : byTask.find((candidate) => newer.includes(candidate));
  if (!agent) {
    return null;
  }
  started.runId = agent.id;
  return { project, agent };
}

async function waitForStarted(ctx: VoiceContext, started: NonNullable<VoiceMemory["started"]>): Promise<{ project: VoiceProject; agent: VoiceAgent } | null> {
  const deadline = Date.now() + (ctx.startWaitMs ?? 15_000);
  for (;;) {
    const found = findStarted(ctx, started);
    if (found || Date.now() >= deadline) {
      return found;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
}

async function deliverWhenStarted(ctx: VoiceContext, started: NonNullable<VoiceMemory["started"]>, text: string): Promise<void> {
  const deadline = Date.now() + 3 * 60_000;
  while (Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 2_000));
    const found = findStarted(ctx, started);
    if (found) {
      try {
        await ctx.rpc("send", { workspaceId: found.project.workspaceId, runId: found.agent.id, text });
        ctx.log?.(`held message delivered to ${found.agent.id} in ${found.project.name}`);
      } catch (error) {
        ctx.log?.(`held message for ${found.agent.id} failed: ${error instanceof Error ? error.message : String(error)}`);
      }
      return;
    }
  }
  ctx.log?.(`held message dropped: the new agent in ${started.workspaceId} never appeared`);
}

function projectName(ctx: VoiceContext, workspaceId: string): string {
  return ctx.projects().find((project) => project.workspaceId === workspaceId)?.name ?? "that project";
}

async function answerPrompt(ctx: VoiceContext, args: Record<string, unknown>): Promise<VoiceToolResult> {
  const projects = ctx.projects();
  const decision = str(args.decision);
  const promptId = str(args.prompt_id).trim();
  // A prompt id names the exact prompt (the one just announced); "allow it"
  // with no names means the one prompt that is waiting, wherever it is.
  const byId = promptId ? findPrompt(projects, promptId) : null;
  if (promptId && !byId) {
    return { output: "That prompt is no longer waiting (it was answered or withdrawn)." };
  }
  const target = byId ?? (str(args.project) || str(args.agent)
    ? (() => {
      const project = resolveProject(projects, str(args.project));
      return { project, agent: resolveAgent(project, str(args.agent), (agent) => (agent.asks ?? []).length > 0) };
    })()
    : onlyWaiting(projects));
  const { project, agent } = target;
  const asks = agent.asks ?? [];
  const ask = byId?.ask ?? (asks.length === 1 ? asks[0] : undefined);
  if (!ask) {
    return { output: asks.length
      ? `${shortTitle(agent)} has ${asks.length} prompts waiting: ${asks.map((a) => `${describeAsk(a)}`).join(" ")} Call again with the prompt_id of the one the user means.`
      : `${shortTitle(agent)} is not waiting on anything right now.` };
  }
  const kind = ask.kind ?? askKind(ask.tool);
  const message = str(args.message).trim();
  let answer: Record<string, unknown>;
  if (kind === "plan") {
    if (decision !== "approve_plan" && decision !== "revise_plan" && decision !== "allow" && decision !== "deny") {
      return { output: "That prompt is a plan: approve it or say what to change." };
    }
    answer = decision === "approve_plan" || decision === "allow" ? { approve: true } : { message: message || "Revise the plan." };
  } else if (kind === "question") {
    const questions = askQuestions(ask);
    const given = Array.isArray(args.answers) ? args.answers.map((value) => str(value).trim()) : [];
    if (decision !== "answer_questions" || given.length < questions.length || given.some((value) => !value)) {
      return { output: `It is asking ${questions.length === 1 ? "a question" : `${questions.length} questions`}: ${questions.map((q, i) => `${i + 1}. ${describeQuestion(q)}`).join(" ")} Answer each one.` };
    }
    answer = { answers: Object.fromEntries(questions.map((question, index) => [question.question, matchOption(question, given[index])])) };
  } else {
    if (decision === "allow" || decision === "approve_plan") {
      answer = { behavior: "allow" };
    } else if (decision === "allow_always") {
      answer = { behavior: "allow", always: true };
    } else if (decision === "deny" || decision === "revise_plan") {
      answer = { behavior: "deny", message: message || "The user denied this by voice." };
    } else {
      return { output: "That prompt is a permission request: allow, allow always, or deny." };
    }
  }
  await ctx.rpc("answer", { workspaceId: project.workspaceId, runId: agent.id, askId: ask.id, answer });
  const remaining = asks.length - 1;
  return { output: `Answered ${shortTitle(agent)}.${remaining > 0 ? ` It has ${remaining} more waiting.` : ""}` };
}

async function agentAction(ctx: VoiceContext, args: Record<string, unknown>): Promise<VoiceToolResult> {
  const action = str(args.action);
  const project = resolveProject(ctx.projects(), str(args.project));
  const agent = resolveAgent(project, str(args.agent));
  if (action === "interrupt") {
    await ctx.rpc("keys", { workspaceId: project.workspaceId, runId: agent.id, keys: "esc" });
    return { output: `Interrupted ${shortTitle(agent)}.` };
  }
  if (action !== "stop" && action !== "merge" && action !== "delete") {
    return { output: "The action must be stop, interrupt, merge or delete." };
  }
  if (action === "delete" && args.confirmed !== true) {
    return { output: `Not deleted yet. Ask the user: delete "${shortTitle(agent)}" in ${project.name}? Call again with confirmed true only if they say yes.` };
  }
  await ctx.rpc("action", { workspaceId: project.workspaceId, runId: agent.id, action });
  if (action === "merge") {
    // The dashboard merges in the background (on the cloud machine: a pull
    // request); say it started, not that it is done. Its result is an update.
    return { output: `Merge started for ${shortTitle(agent)} in ${project.name}. On the cloud machine this opens a pull request; it is not finished yet, so say it is merging.` };
  }
  return { output: `${action === "stop" ? "Stopped" : "Deleted"} ${shortTitle(agent)} in ${project.name}.` };
}

function waitingPlans(project: VoiceProject): Array<{ id: string; request: string }> {
  return (project.plan?.plans ?? [])
    .filter((entry) => entry.awaitingApproval && typeof entry.id === "string")
    .map((entry) => ({ id: entry.id, request: oneLine(entry.request || "") }));
}

function planName(request: string): string {
  return request ? `"${clip(request, 80)}"` : "the plan";
}

function planWords(request: string): string {
  return request ? clip(request, 80) : "untitled plan";
}

async function launchPlan(ctx: VoiceContext, args: Record<string, unknown>): Promise<VoiceToolResult> {
  const projects = ctx.projects();
  const named = str(args.project);
  const candidates = named ? [resolveProject(projects, named)] : projects.filter((project) => waitingPlans(project).length || project.plan?.awaitingApproval);
  if (!candidates.length) {
    return { output: "No plan is waiting for approval." };
  }
  if (candidates.length > 1) {
    return { output: `Plans are waiting in ${listNames(candidates.map((project) => project.name))}. Ask the user which project.` };
  }
  const project = candidates[0];
  let waiting = waitingPlans(project);
  if (!waiting.length && !project.plan?.awaitingApproval) {
    return { output: `No plan is waiting for approval in ${project.name}.` };
  }
  const query = tokens(str(args.plan));
  if (waiting.length > 1 && query.length) {
    const best = bestMatches(waiting, query, (entry) => tokens(entry.request));
    if (best.length) {
      waiting = best;
    }
  }
  if (waiting.length > 1) {
    return { output: `${waiting.length} plans are waiting in ${project.name}: ${listNames(waiting.map((entry) => planWords(entry.request)), "or")}. Ask the user which one.` };
  }
  const target = waiting[0];
  const result = (await ctx.rpc("action", { workspaceId: project.workspaceId, planId: target?.id ?? "", action: "approve" })) as { status?: string } | null;
  const name = planName(target?.request ?? "");
  return {
    output: result?.status === "queued"
      ? `Approval for ${name} in ${project.name} is queued; it launches when the dashboard picks it up.`
      : `Approved ${name} in ${project.name}. Its agents are starting.`,
  };
}

async function findRepos(ctx: VoiceContext, args: Record<string, unknown>): Promise<VoiceToolResult> {
  const result = (await ctx.rpc("repos", {}, 70_000)) as { repos?: Array<{ repo?: string; description?: string | null; open?: boolean }> };
  const words = tokens(str(args.query));
  const repos = (result?.repos ?? []).filter((repo) => typeof repo.repo === "string");
  const scored = repos
    .map((repo) => ({ repo, score: words.length ? overlap(words, tokens(`${repo.repo} ${repo.description ?? ""}`)) : 1 }))
    .filter((entry) => entry.score > 0)
    .sort((a, b) => b.score - a.score)
    .slice(0, 8);
  if (!scored.length) {
    return { output: words.length ? "No repo matches that." : "No repos found." };
  }
  return {
    output: scored
      .map(({ repo }) => `${repo.repo}${repo.open ? " (already a project)" : ""}${repo.description ? `: ${clip(repo.description, 100)}` : ""}`)
      .join("\n"),
  };
}

// ---------------------------------------------------------------------------
// News while a conversation is open: an agent finished, failed or needs you.
// The phone polls with what it last saw and speaks what changed, so the user
// hears it without asking. Keyed by "<workspace>/<run>", valued by a short
// fingerprint of the facts worth announcing.
// ---------------------------------------------------------------------------

export type VoiceSeen = Record<string, string>;

/** At most this many items per announcement; the rest wait for the next poll. */
const UPDATES_PER_POLL = 4;

// What was last seen of an agent: "<status>|<ask ids>|<last announced>". The
// last part keeps an agent flapping between working and waiting (each new
// turn) from being announced again until it says something new.
function seenEntry(agent: VoiceAgent, announced: string): string {
  return `${agent.status ?? ""}|${(agent.asks ?? []).map((ask) => ask.id).join(",")}|${announced}`;
}

function messageMark(agent: VoiceAgent): string {
  const text = agent.lastMessage || agent.summary || "";
  let hash = 0;
  for (let i = 0; i < text.length; i++) {
    hash = (hash * 31 + text.charCodeAt(i)) | 0;
  }
  return `${agent.status ?? ""}:${(hash >>> 0).toString(36)}`;
}

export function voiceUpdates(projects: VoiceProject[], seen: VoiceSeen | null): { updates: string[]; seen: VoiceSeen } {
  const next: VoiceSeen = {};
  const updates: string[] = [];
  for (const project of projects) {
    for (const entry of waitingPlans(project)) {
      // A plan waiting for approval is news once; the key keeps it said.
      const key = `${project.workspaceId}/plan:${entry.id}`;
      if (seen && seen[key] === undefined && updates.length < UPDATES_PER_POLL) {
        updates.push(`A plan in ${project.name} is ready and waiting for approval: ${planName(entry.request)}. Ask the user whether to launch it.`);
      } else if (seen && seen[key] === undefined) {
        continue;
      }
      next[key] = "waiting";
    }
    for (const agent of project.agents) {
      const key = `${project.workspaceId}/${agent.id}`;
      const before = seen?.[key];
      const [oldStatus = "", oldAsks = "", announced = ""] = (before ?? "").split("|");
      if (!seen) {
        // The first poll starts the watch: what is already there is old news.
        next[key] = seenEntry(agent, messageMark(agent));
        continue;
      }
      const news = agentNews(project, agent, before === undefined ? null : { status: oldStatus, asks: oldAsks, announced });
      if (news && updates.length >= UPDATES_PER_POLL) {
        // Not said yet: keep the old entry so the next poll says it.
        if (before !== undefined) {
          next[key] = before;
        }
        continue;
      }
      if (news) {
        updates.push(news);
      }
      next[key] = seenEntry(agent, news ? messageMark(agent) : announced);
    }
  }
  return { updates, seen: next };
}

function agentNews(project: VoiceProject, agent: VoiceAgent, before: { status: string; asks: string; announced: string } | null): string | null {
  const name = `"${shortTitle(agent)}" in ${project.name}`;
  const known = new Set((before?.asks ?? "").split(",").filter(Boolean));
  const fresh = (agent.asks ?? []).filter((ask) => !known.has(ask.id));
  if (fresh.length) {
    return `${name} needs the user: ${describeAsk(fresh[fresh.length - 1])}`;
  }
  const status = agent.status ?? "";
  if (before && status === before.status) {
    return null;
  }
  const said = agent.lastMessage || agent.summary;
  const tail = said ? ` Its last message: ${clip(oneLine(said), 300)}` : "";
  // Something new that already got somewhere (finished between two polls) is
  // still news; something new that is merely working is announced as started.
  switch (status) {
    case "working":
    case "starting":
      return before ? null : `${name} started.`;
    case "review":
      return `${name} finished and is ready to review and merge.${tail}`;
    case "done":
      return `${name} finished.${tail}`;
    case "merged":
      return before ? `${name} was merged.` : null;
    case "failed":
      return `${name} failed.${tail}`;
    case "waiting":
      // Each turn that ends on a question flips working to waiting; only a
      // new message is worth interrupting the user for.
      return before && before.announced === messageMark(agent) ? null : `${name} is waiting for the user.${tail}`;
    default:
      return null;
  }
}

/** The message the phone adds to the conversation so the model says the news. */
export function voiceUpdateMessage(updates: string[]): string {
  return [
    "Live update from Rudder (not from the user). Tell the user right away, in one or two short spoken sentences, then stop and listen:",
    ...updates.slice(0, 6).map((update) => `- ${update}`),
  ].join("\n");
}

// ---------------------------------------------------------------------------
// Words to projects and agents.
// ---------------------------------------------------------------------------

/** A name the model could not pin down. Its message goes back to the model to ask about. */
class Resolution extends Error {}

export function resolveProject(projects: VoiceProject[], query: string): VoiceProject {
  if (!projects.length) {
    throw new Resolution("There are no projects on the cloud machine yet. Use find_repos and open_project to add one.");
  }
  const q = query.trim().toLowerCase();
  if (!q) {
    if (projects.length === 1) {
      return projects[0];
    }
    const waiting = projects.filter((project) => project.agents.some(needsUser));
    if (waiting.length === 1) {
      return waiting[0];
    }
    throw new Resolution(`Which project? ${listNames(projects.map((project) => project.name))}.`);
  }
  const exact = projects.find((project) =>
    project.workspaceId === query || project.name.toLowerCase() === q || (project.repo ?? "").toLowerCase() === q);
  if (exact) {
    return exact;
  }
  const best = bestMatches(projects, tokens(q), (project) => tokens(`${project.name} ${project.repo ?? ""}`));
  if (best.length === 1) {
    return best[0];
  }
  throw new Resolution(best.length
    ? `Did they mean ${listNames(best.map((project) => project.name), "or")}?`
    : `No project called "${query}". The projects are ${listNames(projects.map((project) => project.name))}.`);
}

export function resolveAgent(project: VoiceProject, query: string, prefer: (agent: VoiceAgent) => boolean = needsUser): VoiceAgent {
  const agents = project.agents;
  if (!agents.length) {
    throw new Resolution(`${project.name} has no agents yet. Use start_task to give it work.`);
  }
  const q = query.trim();
  if (!q) {
    if (agents.length === 1) {
      return agents[0];
    }
    const preferred = agents.filter(prefer);
    if (preferred.length === 1) {
      return preferred[0];
    }
    const pool = preferred.length ? preferred : agents;
    throw new Resolution(`Which agent in ${project.name}? ${listNames(pool.slice(0, 6).map(shortTitle))}.`);
  }
  const byId = agents.find((agent) => agent.id === q);
  if (byId) {
    return byId;
  }
  const lower = q.toLowerCase();
  if (/^(the )?(latest|newest|last|most recent)( one| agent)?$/.test(lower)) {
    return [...agents].sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0))[0];
  }
  const exact = agents.filter((agent) => normalized(agent.title ?? "") === normalized(lower));
  if (exact.length === 1) {
    return exact[0];
  }
  const best = bestMatches(agents, tokens(lower), (agent) => tokens(`${agent.title ?? ""} ${agent.task ?? ""}`));
  if (best.length === 1) {
    return best[0];
  }
  // Equally good matches are a question for the user, never a guess: the
  // action could be a delete.
  throw new Resolution(best.length
    ? `Which one: ${listNames(best.slice(0, 5).map(shortTitle), "or")}?`
    : `No agent in ${project.name} matches "${query}". Its agents: ${listNames(agents.slice(0, 6).map(shortTitle))}.`);
}

function findPrompt(projects: VoiceProject[], id: string): { project: VoiceProject; agent: VoiceAgent; ask: VoiceAsk } | null {
  for (const project of projects) {
    for (const agent of project.agents) {
      const ask = (agent.asks ?? []).find((candidate) => candidate.id === id);
      if (ask) {
        return { project, agent, ask };
      }
    }
  }
  return null;
}

function onlyWaiting(projects: VoiceProject[]): { project: VoiceProject; agent: VoiceAgent } {
  const waiting = projects.flatMap((project) =>
    project.agents.filter((agent) => (agent.asks ?? []).length > 0).map((agent) => ({ project, agent })));
  if (waiting.length === 1) {
    return waiting[0];
  }
  if (!waiting.length) {
    throw new Resolution("No agent is waiting on a prompt right now.");
  }
  throw new Resolution(`${waiting.length} agents are waiting: ${listNames(waiting.map(({ project, agent }) => `${shortTitle(agent)} in ${project.name}`))}. Which one?`);
}

function normalized(text: string): string {
  return text.toLowerCase().replace(/[^a-z0-9]+/g, " ").trim();
}

function bestMatches<T>(items: T[], query: string[], words: (item: T) => string[]): T[] {
  if (!query.length) {
    return [];
  }
  let best: T[] = [];
  let top = 0;
  for (const item of items) {
    const score = overlap(query, words(item));
    if (score > top) {
      top = score;
      best = [item];
    } else if (score === top && score > 0) {
      best.push(item);
    }
  }
  // Half the spoken words must land, so "the login one" can find "Fix login redirect".
  return top >= Math.max(1, Math.ceil(query.length / 2)) ? best : [];
}

const STOP_WORDS = new Set(["the", "a", "an", "one", "agent", "project", "repo", "that", "this", "on", "in", "for", "to", "of", "and", "is", "it", "about", "with", "my"]);

function tokens(text: string): string[] {
  return text
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter((word) => word.length > 1 && !STOP_WORDS.has(word))
    .map(stem);
}

function stem(word: string): string {
  return word.length > 4 ? word.replace(/(ing|ed|es|s)$/, "") : word;
}

function overlap(query: string[], words: string[]): number {
  const have = new Set(words);
  let score = 0;
  for (const word of new Set(query)) {
    if (have.has(word) || words.some((candidate) => candidate.length > 3 && word.length > 3 && (candidate.startsWith(word) || word.startsWith(candidate)))) {
      score += 1;
    }
  }
  return score;
}

// ---------------------------------------------------------------------------
// State as text the model reads.
// ---------------------------------------------------------------------------

const ACTIVE = new Set(["working", "waiting", "review", "paused", "failed", "starting"]);

export function needsUser(agent: VoiceAgent): boolean {
  return (agent.asks ?? []).length > 0 || agent.status === "waiting" || agent.status === "review" || agent.status === "failed";
}

export function overviewText(projects: VoiceProject[]): string {
  if (!projects.length) {
    return "No projects on the cloud machine yet.";
  }
  const out: string[] = [];
  for (const project of projects) {
    const header = [`Project ${project.name}${project.repo && project.repo !== project.name ? ` (${project.repo})` : ""}`];
    if (project.failureReason) {
      header.push(`problem: ${clip(project.failureReason, 160)}`);
    } else if (project.dashboard && project.dashboard !== "running") {
      header.push(`dashboard ${project.dashboard}`);
    }
    out.push(`${header.join(", ")}:`);
    const plan = planLine(project);
    if (plan) {
      out.push(`  ${plan}`);
    }
    const agents = [...project.agents].sort((a, b) => rank(a) - rank(b) || (b.updatedAt ?? 0) - (a.updatedAt ?? 0));
    const shown = agents.filter((agent) => ACTIVE.has(agent.status ?? "") || needsUser(agent)).slice(0, 12);
    const quiet = agents.length - shown.length;
    if (!agents.length) {
      out.push("  no agents.");
    }
    for (const agent of shown) {
      out.push(`  - ${agentLine(agent)}`);
      for (const ask of agent.asks ?? []) {
        out.push(`    waiting on: ${describeAsk(ask)}`);
      }
      const last = agent.lastMessage || agent.summary;
      if (last) {
        out.push(`    last said: ${clip(oneLine(last), 220)}`);
      }
    }
    if (quiet > 0) {
      out.push(`  plus ${quiet} finished or stopped ${quiet === 1 ? "agent" : "agents"} (${listNames(agents.filter((agent) => !shown.includes(agent)).slice(0, 5).map(shortTitle))}).`);
    }
  }
  return out.join("\n");
}

function planLine(project: VoiceProject): string | null {
  const plan = project.plan;
  if (!plan) {
    return null;
  }
  const waiting = waitingPlans(project);
  const nodes = plan.nodes ?? [];
  if (waiting.length || plan.awaitingApproval) {
    const titles = nodes.filter((node) => node.status === "planned" || node.status === "ready").map((node) => clip(oneLine(node.title || node.id), 60));
    const what = waiting.length ? listNames(waiting.map((entry) => planWords(entry.request))) : "a plan";
    return `plan waiting for approval (launch_plan): ${what}${titles.length ? `, ${titles.length} ${titles.length === 1 ? "task" : "tasks"}: ${listNames(titles.slice(0, 6))}` : ""}.`;
  }
  if (!nodes.length) {
    return null;
  }
  const count = (status: string) => nodes.filter((node) => node.status === status).length;
  const parts = [`${count("merged")} of ${nodes.length} tasks merged`];
  if (count("running")) parts.push(`${count("running")} running`);
  if (count("review")) parts.push(`${count("review")} in review`);
  if (count("failed")) parts.push(`${count("failed")} failed`);
  return `plan: ${parts.join(", ")}.`;
}

function rank(agent: VoiceAgent): number {
  if ((agent.asks ?? []).length) return 0;
  if (agent.status === "waiting" || agent.status === "failed") return 1;
  if (agent.status === "review") return 2;
  if (agent.status === "working" || agent.status === "starting") return 3;
  return 4;
}

function agentLine(agent: VoiceAgent): string {
  const status = (agent.asks ?? []).length ? "needs the user" : STATUS_WORDS[agent.status ?? ""] ?? agent.status ?? "unknown";
  const who = [agent.backend, agent.model].filter(Boolean).join(" ");
  return `"${shortTitle(agent)}": ${status}${who ? ` (${who})` : ""}`;
}

const STATUS_WORDS: Record<string, string> = {
  working: "working",
  starting: "starting",
  waiting: "waiting for the user",
  review: "finished, ready to review and merge",
  done: "done",
  merged: "merged",
  stopped: "stopped",
  paused: "paused",
  failed: "failed",
  orphaned: "lost its process",
};

function shortTitle(agent: VoiceAgent): string {
  return clip(oneLine(agent.title || agent.task || agent.id), 80);
}

function askKind(tool: string | undefined): string {
  return tool === "AskUserQuestion" ? "question" : tool === "ExitPlanMode" ? "plan" : "permission";
}

type Question = { question: string; options: string[]; multiSelect: boolean };

function askQuestions(ask: VoiceAsk): Question[] {
  const input = (ask.input ?? {}) as { questions?: unknown };
  const list = Array.isArray(input.questions) ? input.questions : [];
  return list
    .map((raw) => {
      const q = (raw ?? {}) as { question?: unknown; options?: unknown; multiSelect?: unknown };
      const options = Array.isArray(q.options)
        ? q.options.map((option) => (typeof option === "string" ? option : str((option as { label?: unknown })?.label))).filter(Boolean)
        : [];
      return { question: str(q.question), options, multiSelect: q.multiSelect === true };
    })
    .filter((q) => q.question);
}

function describeQuestion(question: Question): string {
  return `${question.question}${question.options.length ? ` Options: ${question.options.join("; ")}.` : ""}`;
}

/** A spoken answer lands on the option it names exactly (or by number), or goes through as free text. */
function matchOption(question: Question, spoken: string): string {
  const said = tokens(spoken);
  if (!question.options.length || !said.length) {
    return spoken;
  }
  const exact = question.options.find((option) => option.toLowerCase() === spoken.toLowerCase());
  if (exact) {
    return exact;
  }
  const ordinal = /^(option )?(\d)$/.exec(spoken.trim().toLowerCase());
  if (ordinal && question.options[Number(ordinal[2]) - 1]) {
    return question.options[Number(ordinal[2]) - 1];
  }
  // Anything else goes through as said: "not SQLite" must not become "SQLite".
  const loose = question.options.find((option) => normalized(option) === normalized(spoken));
  return loose ?? spoken;
}

function describeAsk(ask: VoiceAsk): string {
  return `${describeAskBody(ask)} [prompt_id ${ask.id}]`;
}

function describeAskBody(ask: VoiceAsk): string {
  const kind = ask.kind ?? askKind(ask.tool);
  const input = (ask.input ?? {}) as Record<string, unknown>;
  if (kind === "plan") {
    return `approval of its plan: ${clip(oneLine(str(input.plan)), 500)}`;
  }
  if (kind === "question") {
    const questions = askQuestions(ask);
    return `${questions.length === 1 ? "a question" : `${questions.length} questions`}: ${questions.map((q, i) => `${i + 1}. ${describeQuestion(q)}`).join(" ")}`;
  }
  const detail = str(input.command) || str(input.description) || str(input.file_path) || str(input.url) || str(input.pattern) || jsonish(input);
  return `permission to use ${ask.tool || "a tool"}: ${clip(oneLine(detail), 240)}`;
}

function transcriptLine(message: { role?: string; text?: string; tool?: { name?: string; input?: unknown } }): string {
  if (message.role === "tool") {
    const input = message.tool?.input;
    const detail = typeof input === "string" ? input : jsonish(input);
    return `  [${message.tool?.name ?? "tool"}] ${clip(oneLine(detail), 120)}`;
  }
  const text = oneLine(str(message.text));
  if (!text) {
    return "";
  }
  return `  ${message.role === "user" ? "User" : message.role === "assistant" ? "Agent" : "Note"}: ${clip(text, message.role === "assistant" ? 600 : 300)}`;
}

// ---------------------------------------------------------------------------

function parseArgs(raw: unknown): Record<string, unknown> {
  if (raw && typeof raw === "object" && !Array.isArray(raw)) {
    return raw as Record<string, unknown>;
  }
  if (typeof raw !== "string" || !raw.trim()) {
    return {};
  }
  const parsed = JSON.parse(raw);
  return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : {};
}

function str(value: unknown): string {
  return typeof value === "string" ? value : typeof value === "number" ? String(value) : "";
}

function jsonish(value: unknown): string {
  try {
    return value === undefined ? "" : JSON.stringify(value);
  } catch {
    return "";
  }
}

function oneLine(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

function clip(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

function listNames(names: string[], joiner = "and"): string {
  const quoted = names.map((name) => `"${name}"`);
  return quoted.length <= 1 ? quoted.join("") : `${quoted.slice(0, -1).join(", ")} ${joiner} ${quoted[quoted.length - 1]}`;
}
