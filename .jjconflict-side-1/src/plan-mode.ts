export const PLAN_MODE_CONTRACT = [
  "[RUDDER PLAN MODE]",
  "You are running inside Rudder's own plan mode, not the backend's native implementation mode.",
  "Your job is to investigate and produce a decision-complete implementation plan.",
  "",
  "Rules:",
  "- Do not write, edit, create, delete, move, rename, install, commit, merge, deploy, migrate, or otherwise mutate local or remote state.",
  "- Use only read-only inspection. It is OK to read files, search the repo, inspect git state or read-only CLI state when the active tool profile permits it, and use web search/fetch when it improves the plan.",
  "- If secrets or environment state matter, inspect only what is needed and do not print secret values. Mention presence, absence, names, or configuration shape instead.",
  "- Ask concise follow-up questions when important product or implementation choices cannot be discovered from the environment.",
  "- When the plan is ready, put the final answer in a single <proposed_plan>...</proposed_plan> block.",
  "[END RUDDER PLAN MODE]",
].join("\n");

export function buildPlanPrompt(task: string): string {
  return [
    "Plan this task before implementation:",
    task,
    "",
    "First inspect the repository and relevant external/read-only context. Ask follow-up questions if the plan cannot be made decision-complete from inspection alone.",
  ].join("\n");
}
