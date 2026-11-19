import type { RunRecord } from "./types.js";
import { CODEX_RUDDER_PLANNER_CONFIG_ARGS, CODEX_RUDDER_WORKER_CONFIG_ARGS, codexShellEnvConfigArgs } from "./codex-config.js";
import { codexEffortConfigArgs, normalizeEffortForBackend } from "./effort.js";
import { opencodeEffortEnv } from "./opencode.js";
import { PLAN_MODE_CONTRACT } from "./plan-mode.js";
import { CODEX_DEFAULT_MODEL, lastUsedCodexModelSync, liveCodexModel } from "./state.js";
import { taskDisplayLabel } from "./task-summary.js";
import { toolCacheEnv } from "./tool-cache.js";
import { shellQuote, stripRudderPromptWrappers } from "./util.js";

export function nativeAgentCommand(params: {
  run: RunRecord;
  prompt: string;
  contract: string;
  mode?: "execute" | "plan";
  codexCommand?: string;
}): string {
  const mode = params.mode ?? params.run.mode ?? "execute";
  const prompt = stripRudderPromptWrappers(params.prompt);
  const args = mode === "plan"
    ? planArgs(params.run, prompt, params.codexCommand)
    : params.run.backend === "codex"
      ? codexArgs(params.run, prompt, params.contract, params.codexCommand)
      : params.run.backend === "opencode"
        ? opencodeArgs(params.run, [params.contract, "", prompt].join("\n"), "build")
        : params.run.backend === "antigravity"
          ? antigravityArgs(params.run, [params.contract, "", prompt].join("\n"), ["--dangerously-skip-permissions"])
          : claudeArgs(params.run, prompt, params.contract);
  return args.map(shellQuote).join(" ");
}

function claudeArgs(run: RunRecord, prompt: string, contract: string): string[] {
  const model = run.model || "sonnet";
  const effort = normalizeEffortForBackend("claude", run.effort);
  const sessionId = run.session?.nativeSessionId;
  return compact([
    "env",
    "CLAUDE_CODE_NO_FLICKER=0",
    "claude",
    "--model",
    model,
    effort ? "--effort" : undefined,
    effort,
    "--permission-mode",
    "bypassPermissions",
    "--dangerously-skip-permissions",
    "--append-system-prompt",
    contract,
    "--name",
    paneTitle(run),
    sessionId ? "--session-id" : undefined,
    sessionId,
    prompt,
  ]);
}

/**
 * OpenCode's interactive TUI, as the dashboard launches it (native
 * launch.rs). Workers run its build agent with `--auto`, which approves what
 * no rule denies (the worker is isolated in its own workspace); planners run
 * its plan agent, which does not edit. `--prompt` carries the first turn of
 * a new session, whose id OpenCode mints (its TUI ignores `--prompt` in a
 * session it opens with `--session`). Models are `provider/model`; an empty
 * model is OpenCode's own default. An effort is the model's variant.
 */
function opencodeArgs(run: RunRecord, prompt: string, agent: "build" | "plan"): string[] {
  const effort = opencodeEffortEnv(run.model, run.effort);
  return compact([
    effort.length ? "env" : undefined,
    ...effort,
    "opencode",
    agent === "build" ? "--auto" : "--agent",
    agent === "build" ? undefined : "plan",
    run.model ? "-m" : undefined,
    run.model || undefined,
    "--prompt",
    prompt,
  ]);
}

/**
 * Antigravity's interactive TUI (`agy`): `-i` runs the first turn and keeps
 * the session open, `--conversation` continues the run's own conversation.
 * Workers skip permission prompts like Claude and Codex workers; planners run
 * in agy's plan mode.
 */
function antigravityArgs(run: RunRecord, prompt: string, modeArgs: string[]): string[] {
  const effort = normalizeEffortForBackend("antigravity", run.effort);
  const conversation = run.session?.nativeSessionId;
  return compact([
    "agy",
    ...modeArgs,
    run.model ? "--model" : undefined,
    run.model || undefined,
    effort ? "--effort" : undefined,
    effort,
    conversation ? "--conversation" : undefined,
    conversation,
    "-i",
    prompt,
  ]);
}

function codexArgs(run: RunRecord, prompt: string, contract: string, codexCommand = "codex"): string[] {
  const model = liveCodexModel(run.model) || lastUsedCodexModelSync() || CODEX_DEFAULT_MODEL;
  return [
    codexCommand,
    "--no-alt-screen",
    "--model",
    model,
    "--dangerously-bypass-approvals-and-sandbox",
    "--enable",
    "goals",
    ...CODEX_RUDDER_WORKER_CONFIG_ARGS,
    ...codexEffortConfigArgs(run.effort),
    ...codexShellEnvConfigArgs(toolCacheEnv("agent", { workspace: run.workspace.path })),
    "--cd",
    run.workspace.path,
    [contract, "", prompt].join("\n"),
  ].filter((value): value is string => Boolean(value));
}

function planArgs(run: RunRecord, prompt: string, codexCommand?: string): string[] {
  return run.backend === "codex"
    ? codexPlanArgs(run, prompt, codexCommand)
    : run.backend === "opencode"
      ? opencodeArgs(run, `${PLAN_MODE_CONTRACT}\n\n${prompt}`, "plan")
      : run.backend === "antigravity"
        ? antigravityArgs(run, `${PLAN_MODE_CONTRACT}\n\n${prompt}`, ["--mode", "plan"])
        : claudePlanArgs(run, prompt);
}

function claudePlanArgs(run: RunRecord, prompt: string): string[] {
  const model = run.model || "sonnet";
  const effort = normalizeEffortForBackend("claude", run.effort);
  return compact([
    "env",
    "CLAUDE_CODE_NO_FLICKER=0",
    "claude",
    "--model",
    model,
    effort ? "--effort" : undefined,
    effort,
    "--permission-mode",
    "default",
    "--tools",
    CLAUDE_PLAN_TOOLS.join(","),
    "--allowedTools",
    CLAUDE_PLAN_TOOLS.join(","),
    "--disallowedTools",
    CLAUDE_PLAN_DISALLOWED_TOOLS.join(","),
    "--append-system-prompt",
    PLAN_MODE_CONTRACT,
    "--name",
    `plan:${paneTitle(run)}`,
    prompt,
  ]);
}

function codexPlanArgs(run: RunRecord, prompt: string, codexCommand = "codex"): string[] {
  const model = liveCodexModel(run.model) || lastUsedCodexModelSync() || CODEX_DEFAULT_MODEL;
  return compact([
    codexCommand,
    "--no-alt-screen",
    "--model",
    model,
    "--sandbox",
    "read-only",
    "--ask-for-approval",
    "never",
    "--enable",
    "goals",
    "--search",
    ...CODEX_RUDDER_PLANNER_CONFIG_ARGS,
    ...codexEffortConfigArgs(run.effort),
    "--cd",
    run.workspace.path,
    `${PLAN_MODE_CONTRACT}\n\n${prompt}`,
  ]);
}

function paneTitle(run: RunRecord): string {
  const words = taskDisplayLabel(run, 34);
  return `${run.backend}:${words || run.id.slice(0, 12)}`;
}

function compact(values: Array<string | undefined>): string[] {
  return values.filter((value): value is string => Boolean(value));
}

const CLAUDE_PLAN_TOOLS = [
  "Read",
  "Grep",
  "Glob",
  "LS",
  "WebSearch",
  "WebFetch",
];

const CLAUDE_PLAN_DISALLOWED_TOOLS = [
  "Edit",
  "Write",
  "MultiEdit",
  "NotebookEdit",
  "Bash",
  "Bash(rm *)",
  "Bash(mv *)",
  "Bash(cp *)",
  "Bash(mkdir *)",
  "Bash(touch *)",
  "Bash(chmod *)",
  "Bash(chown *)",
  "Bash(git add*)",
  "Bash(git commit*)",
  "Bash(git checkout*)",
  "Bash(git switch*)",
  "Bash(git reset*)",
  "Bash(git clean*)",
  "Bash(git merge*)",
  "Bash(git rebase*)",
  "Bash(git push*)",
  "Bash(fly deploy*)",
  "Bash(fly secrets set*)",
  "Bash(fly secrets unset*)",
  "Bash(fly scale*)",
  "Bash(fly apps destroy*)",
];
