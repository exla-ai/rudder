import fsp from "node:fs/promises";
import path from "node:path";
import type { RunRecord, SpecContract, VerificationResult } from "./types.js";
import { currentBranch, currentCommit, runHasChanges, workspaceDiff, workspaceStatus } from "./git.js";
import { pathExists, readJson, runCommand, writeJson } from "./util.js";
import { agentContextPath, specPath, verifierPath } from "./state.js";
import { PROPAGATION_RULES } from "./surfaces.js";
import { deriveGoal, deriveSuccess, extractSuccess, formatGoalPrompt } from "./goal.js";

const INSTRUCTION_FILES = ["AGENTS.md", "CLAUDE.md", "README.md"];
const INSTRUCTION_FILE_LIMIT = 6_000;
const PLANNER_INSTRUCTION_BUDGET = 8_000;

// Prompt-budget truncation must be visible: without the marker, workers
// silently lose every rule past the limit and have no cue to read the file.
function truncateInstructionContent(name: string, content: string, limit = INSTRUCTION_FILE_LIMIT): string {
  if (content.length <= limit) {
    return content;
  }
  const marker = `\n\n[Rudder: ${name} truncated; ${content.length} chars available in the workspace file.]`;
  let end = Math.max(0, limit - marker.length);
  if (end && /[\ud800-\udbff]/.test(content[end - 1] ?? "")) end--;
  return `${content.slice(0, end)}${marker}`;
}

/**
 * Workers get file references: native backends already load applicable rules
 * and can read anything else they need. The tool-less API planner gets bounded
 * AGENTS/CLAUDE text, never a duplicated README or generated task history.
 */
export async function loadInstructionFiles(
  workspace: string,
  repoRoot?: string,
  options: { mode?: "references" | "planning" } = {},
): Promise<Array<{ path: string; content: string }>> {
  const instructionsFiles: Array<{ path: string; content: string }> = [];
  let remaining = PLANNER_INSTRUCTION_BUDGET;
  for (const name of INSTRUCTION_FILES) {
    const filePath = path.join(workspace, name);
    if (!(await pathExists(filePath))) {
      continue;
    }
    const stat = await fsp.stat(filePath);
    const inline = options.mode === "planning" && name !== "README.md" && remaining >= 256;
    const content = inline ? truncateInstructionContent(name, await fsp.readFile(filePath, "utf8"), Math.min(INSTRUCTION_FILE_LIMIT, remaining))
      : `${name === "README.md" ? "Usage reference" : "Repository rules"}; read on demand if not already loaded (${stat.size} bytes).`;
    if (inline) remaining = Math.max(0, remaining - content.length);
    instructionsFiles.push({
      path: name,
      content,
    });
  }
  const contextFile = agentContextPath(repoRoot ?? workspace);
  if (await pathExists(contextFile)) {
    instructionsFiles.push({
      path: "RUDDER.md",
      content: "Generated live coordination; consult when work overlaps or before integration.",
    });
  }
  return instructionsFiles;
}

export async function createSpec(run: RunRecord): Promise<SpecContract> {
  const workspace = run.workspace.path;
  const instructionsFiles = await loadInstructionFiles(workspace, run.repoRoot);

  const task = run.currentPrompt ?? run.task;
  const acceptanceCriteria = buildAcceptanceCriteria(task);
  const spec: SpecContract = {
    runId: run.id,
    runtimeRole: run.workspace.enabled ? "isolated-worker" : "main-owner",
    task,
    // The plain `rudder <task>` / TUI single-task spawn has no planner-supplied
    // goal: the objective is the task statement, and the success condition is
    // the acceptance criteria. renderContract leads with these in objective format.
    // When the task is already an objective-formatted planner prompt, recover its
    // explicit objective + done-when instead of re-deriving.
    goal: deriveGoal(task),
    success: extractSuccess(task) ?? deriveSuccess(acceptanceCriteria),
    createdAt: new Date().toISOString(),
    repo: {
      root: workspace,
      branch: await currentBranch(workspace),
      baseCommit: (await currentCommit(workspace)) || run.baseCommit,
      status: await workspaceStatus(run),
    },
    instructionsFiles,
    acceptanceCriteria,
    suggestedTests: await suggestTests(workspace),
  };
  await writeJson(specPath(run.repoRoot, run.id), spec);
  return spec;
}

// Parallel-work guidance for every implementing agent. Mirrors the Rust
// `parallel_work_rule!` in native/src/tasks.rs; keep the text in parity.
export const PARALLEL_WORK_RULE =
  "Batch independent reads and commands. Use concurrent subagents for substantial independent work when helpful; integrate and verify their results. Never let two subagents edit the same file.";

export function renderContract(spec: SpecContract): string {
  const instructions = spec.instructionsFiles
    .map((file) => file.path)
    .join(", ");
  const body = [
    "Rudder-specific context injected by Rudder",
    spec.runtimeRole === "main-owner"
      ? "You own this request end to end in the user's main checkout. Preserve unrelated edits."
      : "You are an isolated worker. Use jj status/diff; Rudder handles commits and integration. Keep to your assignment.",
    "",
    "Acceptance criteria:",
    ...spec.acceptanceCriteria.map((item) => `- ${item}`),
    "",
    spec.suggestedTests.length > 0
      ? ["Suggested verification:", ...spec.suggestedTests.map((item) => `- ${item}`)].join("\n")
      : "Suggested verification: choose the smallest relevant checks for this repo.",
    "",
    "Working rules:",
    "- Follow applicable repo rules once if not already loaded; read additional references only when relevant.",
    "- Implement, run relevant checks and verify the requested outcome. Preserve prior authorization; report unfinished work honestly.",
    `- ${PARALLEL_WORK_RULE}`,
    ...PROPAGATION_RULES.map((rule) => `- ${rule}`),
    "",
    instructions ? `Available repository context: ${instructions}. File contents are not duplicated here.` : "",
  ]
    .filter(Boolean)
    .join("\n");
  // Lead with the goal header so the backend sees the objective and verifiable
  // stopping condition before the body without invoking a slash command.
  return formatGoalPrompt({
    goal: spec.goal || deriveGoal(spec.task),
    success: spec.success || deriveSuccess(spec.acceptanceCriteria),
    body,
  });
}

/**
 * Judge one pass. `transcriptFrom` is the byte length `output.txt` had when
 * this pass started: the file holds EVERY turn, and scanning all of it meant
 * a first turn's "I didn't run any checks" failed every later check too, so
 * the steerer re-prompted a finished, verified run until its budget ran out
 * (seen on a cloud sail: done in 2.5 minutes, steered for 4.5 more).
 */
export async function verifyRun(
  run: RunRecord,
  options: { transcriptFrom?: number } = {},
): Promise<VerificationResult> {
  const diff = await workspaceDiff(run);
  const changed = await runHasChanges(run);
  const outputPath = path.join(run.repoRoot, ".rudder", "runs", run.id, "output.txt");
  const whole = await fsp.readFile(outputPath).catch(() => Buffer.alloc(0));
  const transcript = whole.subarray(Math.min(options.transcriptFrom ?? 0, whole.length)).toString("utf8");
  const spec = await readJson<SpecContract>(specPath(run.repoRoot, run.id));

  const missing: string[] = [];
  const satisfied: string[] = [];
  if (!changed) {
    if (expectsCodeChanges(run.task)) {
      missing.push("No file changes were detected.");
    } else {
      satisfied.push("No file changes were needed for this prompt.");
    }
  } else {
    satisfied.push(diff.trim() ? "The run produced a git diff." : "The run produced git status changes.");
  }
  // Verify against the node's own contract, not a keyword scan: the old
  // /test|spec|check/ mention-regex passed a transcript that literally said
  // "I did not run the tests". Look for evidence a concrete expected command
  // ran (the spec's success line and suggested tests name them), or real
  // test-runner output, and treat an explicit "couldn't run" as a gap.
  const expectedCommands = [
    ...extractCommands(spec?.success ?? ""),
    ...(spec?.suggestedTests ?? []),
  ].filter((cmd, index, all) => cmd && all.indexOf(cmd) === index);
  const ranCommand = expectedCommands.find((cmd) => transcript.includes(cmd));
  const testOutputEvidence =
    /\b(\d+\s+(?:passed|passing)|tests?\s+pass(?:ed)?|test result: ok|all checks passed|0 failures|exit code 0)\b/i.test(
      transcript,
    );
  const refusedVerification =
    /\b(did not run|didn'?t run|could not run|couldn'?t run|unable to run|skipp(?:ed|ing) (?:the )?(?:tests?|checks?|verification))\b/i.test(
      transcript,
    );
  if (changed || expectsCodeChanges(run.task)) {
    if (refusedVerification) {
      missing.push("The transcript says verification was skipped or could not run.");
    } else if (ranCommand) {
      satisfied.push(`The transcript shows an expected check ran (${ranCommand}).`);
    } else if (testOutputEvidence) {
      satisfied.push("The transcript contains test/check output evidence.");
    } else if (expectedCommands.length > 0) {
      missing.push(
        `No evidence the expected checks ran (looked for: ${expectedCommands.slice(0, 4).join(", ")}).`,
      );
    }
  }

  const successLine = spec?.success?.trim();
  const result: VerificationResult = {
    satisfied,
    missing,
    notes: changed
      ? successLine
        ? `Local verifier checked produced changes and execution evidence against the task's own bar: ${successLine}`
        : "Local verifier checked for produced changes and evidence of verification."
      : expectsCodeChanges(run.task)
        ? "Local verifier found no changes. The worker may have only analyzed the task."
        : "Local verifier accepted a no-change response for a non-edit prompt.",
    shouldContinue: missing.some((item) => item.startsWith("No file changes")),
  };
  await writeJson(verifierPath(run.repoRoot, run.id), result);
  return result;
}

/**
 * Concrete runnable commands named by a success/done-when line: backtick-quoted
 * spans plus a bare common-runner invocation ("npm run check", "cargo test",
 * "pytest"). Used to check the transcript for execution evidence.
 */
function extractCommands(text: string): string[] {
  const out: string[] = [];
  for (const match of text.matchAll(/`([^`]+)`/g)) {
    const cmd = match[1].trim();
    if (cmd) {
      out.push(cmd);
    }
  }
  const runner = text.match(
    /\b((?:npm|npx|pnpm|yarn|cargo|pytest|go|make|node)\s+(?:run\s+)?[a-zA-Z0-9_.:-]+)/,
  );
  if (runner) {
    out.push(runner[1].trim());
  }
  return out;
}

function expectsCodeChanges(task: string): boolean {
  if (/\b(do not|don't|without|no)\s+(edit|change|modify|write|touch)\b/i.test(task)) {
    return false;
  }
  return /\b(add|build|change|create|delete|edit|fix|implement|modify|patch|refactor|remove|rename|repair|replace|rewrite|ship|update|write)\b/i.test(
    task,
  );
}

function buildAcceptanceCriteria(task: string): string[] {
  const criteria = ["Address the user's requested task directly.", "Keep the change scoped and reviewable."];
  if (/test|fail|bug|fix/i.test(task)) {
    criteria.push("Run or explain the relevant failing test/check.");
  }
  if (/refactor/i.test(task)) {
    criteria.push("Preserve existing behavior while improving structure.");
  }
  return criteria;
}

export async function suggestTests(workspace: string): Promise<string[]> {
  const suggestions: string[] = [];
  const packageJson = await readJson<{ scripts?: Record<string, string> }>(
    path.join(workspace, "package.json"),
  );
  if (packageJson?.scripts) {
    for (const script of ["test", "check", "typecheck", "lint"]) {
      if (packageJson.scripts[script]) {
        suggestions.push(`npm run ${script}`);
      }
    }
  }
  if (await pathExists(path.join(workspace, "Cargo.toml"))) {
    suggestions.push("cargo test");
  }
  if (await pathExists(path.join(workspace, "pyproject.toml"))) {
    suggestions.push("pytest");
  }
  if (suggestions.length === 0) {
    const result = await runCommand("git", ["ls-files"], { cwd: workspace, allowFailure: true });
    if (result.stdout.includes("Makefile")) {
      suggestions.push("make test");
    }
  }
  return suggestions;
}
