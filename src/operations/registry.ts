import manifest from "./registry.json" with { type: "json" };

/** Surface metadata, not an authority grant or a replacement engine protocol. */
export const COMMAND_REGISTRY = manifest;
export const SURFACE_VERSION = 1 as const;
export type OperationDefinition = (typeof manifest.operations)[number];

export function operationDefinition(id: string): OperationDefinition {
  const operation = manifest.operations.find((entry) => entry.id === id);
  if (!operation) throw new Error(`Unknown operation: ${id}`);
  return operation;
}

export function assertAliasCurrent(alias: { name: string; expires: string; replacement: string }, now = new Date()): void {
  if (!Number.isFinite(now.getTime()) || now.getTime() >= Date.parse(`${alias.expires}T00:00:00Z`)) {
    throw new Error(`${alias.name} expired on ${alias.expires}; use ${alias.replacement}`);
  }
}

export function commandHelp(family?: string): string {
  if (family && !manifest.families.some((entry) => entry.name === family)) throw new Error(`Unknown command family: ${family}`);
  const entries = family
    ? manifest.operations.filter((entry) => entry.cli[0] === family)
        .map((entry) => `  rudder ${entry.cli.join(" ")}${entry.adapter === null ? "  (selected surface or authenticated engine tool)" : ""}`)
    : manifest.families.map((entry) => `  rudder ${entry.name.padEnd(9)} ${entry.summary}`);
  return ["rudder", "", "Usage:", ...entries, "", "Run scope: --location local|cloud --checkout isolated|main --cwd <project>",
    "New CLI tasks use isolated local checkouts. Main checkout execution requires its native owner.",
    "Use run --backend claude|codex|opencode; describe discussion or plan-only intent in the task.",
    "Use help <family>, help --json, or completion [family] to discover operations.",
    "Machine callbacks and engine APIs keep their existing JSON and exit contracts.", ""].join("\n");
}

/** Completion never advertises callbacks, retired names or compatibility aliases. */
export function commandCompletions(words: readonly string[] = []): string[] {
  if (words.length === 0) return manifest.families.map((entry) => entry.name);
  return [...new Set(manifest.operations.filter((entry) => words.every((word, index) => entry.cli[index] === word))
    .map((entry) => entry.cli[words.length]).filter((word): word is string => Boolean(word)))];
}

export type ComposerClassification =
  | { kind: "text"; text: string }
  | { kind: "backend"; text: string }
  | { kind: "rudder"; command: string; text: string }
  | { kind: "refused"; reason: string };

/** Backend composers own their slash namespace, including /model and /help. */
export function classifyComposerInput(text: string, composer: "project" | "backend", now = new Date()): ComposerClassification {
  if (!text.trimStart().startsWith("/")) return { kind: "text", text };
  if (composer === "backend") return { kind: "backend", text };
  const command = text.trim().split(/\s+/, 1)[0]!;
  if (manifest.retired_slash.includes(command)) return { kind: "refused", reason: `${command} is retired; use ordinary task input or a selected agent action` };
  if (manifest.slash.includes(command)) return { kind: "rudder", command, text };
  const alias = manifest.slash_aliases.find((entry) => entry.name === command);
  if (alias) {
    try { assertAliasCurrent(alias, now); }
    catch (error) { return { kind: "refused", reason: (error as Error).message }; }
    return { kind: "rudder", command, text };
  }
  return { kind: "refused", reason: `Unknown Rudder command ${command}; use the backend composer for backend slash commands` };
}
