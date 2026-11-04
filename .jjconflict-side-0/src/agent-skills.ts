import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Capability skills Rudder ships to every agent it runs (Claude Code and
// Codex), unlike the orchestrator's `.claude/skills/rudder-*` dashboard
// controls. Sources live in the package under assets/skills/<name>/ and are
// installed user-wide as rudder-<name>, so isolated workspaces, the main
// checkout and cloud workers all see them. Files are rewritten only when they
// differ; anything else in those directories is the user's and left alone.

export type AgentSkillTarget = { agent: "claude" | "codex"; dir: string };

export function agentSkillsSourceDir(): string {
  return path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "assets", "skills");
}

export function agentSkillTargets(home = os.homedir(), env: NodeJS.ProcessEnv = process.env): AgentSkillTarget[] {
  return [
    { agent: "claude", dir: path.join(env.CLAUDE_CONFIG_DIR || path.join(home, ".claude"), "skills") },
    { agent: "codex", dir: path.join(env.CODEX_HOME || path.join(home, ".codex"), "skills") },
  ];
}

async function listFiles(root: string, rel = ""): Promise<string[]> {
  const entries = await fsp.readdir(path.join(root, rel), { withFileTypes: true });
  const files: string[] = [];
  for (const entry of entries) {
    const child = path.join(rel, entry.name);
    if (entry.isDirectory()) files.push(...(await listFiles(root, child)));
    else if (entry.isFile()) files.push(child);
  }
  return files;
}

// Skills Rudder once shipped under another name. Their rudder-<name> copies are
// Rudder's own (never the user's), so an upgrade removes them rather than leave
// agents two skills for the same job.
export const RETIRED_AGENT_SKILLS = ["namecheap-dns"];

export type InstallResult = { installed: string[]; updatedFiles: number; removed: string[] };

export async function installAgentSkills(options: { sourceDir?: string; targets?: AgentSkillTarget[] } = {}): Promise<InstallResult> {
  const sourceDir = options.sourceDir ?? agentSkillsSourceDir();
  const targets = options.targets ?? agentSkillTargets();
  let skills: string[];
  try {
    skills = (await fsp.readdir(sourceDir, { withFileTypes: true })).filter((e) => e.isDirectory()).map((e) => e.name);
  } catch {
    return { installed: [], updatedFiles: 0, removed: [] };
  }
  const installed: string[] = [];
  let updatedFiles = 0;
  for (const skill of skills) {
    const files = await listFiles(path.join(sourceDir, skill));
    if (!files.includes("SKILL.md")) continue;
    for (const target of targets) {
      const dest = path.join(target.dir, `rudder-${skill}`);
      for (const file of files) {
        const from = path.join(sourceDir, skill, file);
        const to = path.join(dest, file);
        const content = await fsp.readFile(from);
        const current = await fsp.readFile(to).catch(() => null);
        if (current && current.equals(content)) continue;
        await fsp.mkdir(path.dirname(to), { recursive: true });
        await fsp.writeFile(to, content);
        if (file.startsWith(`scripts${path.sep}`)) await fsp.chmod(to, 0o755);
        updatedFiles++;
      }
      installed.push(`${target.agent}:rudder-${skill}`);
    }
  }
  const removed: string[] = [];
  for (const retired of RETIRED_AGENT_SKILLS) {
    if (skills.includes(retired)) continue;
    for (const target of targets) {
      const dir = path.join(target.dir, `rudder-${retired}`);
      if (!(await fsp.stat(dir).catch(() => null))) continue;
      await fsp.rm(dir, { recursive: true, force: true });
      removed.push(`${target.agent}:rudder-${retired}`);
    }
  }
  return { installed, updatedFiles, removed };
}

/** Best effort: a read-only home or odd permissions must never block a run. */
export async function ensureAgentSkills(env: NodeJS.ProcessEnv = process.env): Promise<void> {
  if (env.RUDDER_DISABLE_AGENT_SKILLS === "1") return;
  await installAgentSkills().catch(() => undefined);
}
