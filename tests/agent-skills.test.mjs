import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { agentSkillTargets, agentSkillsSourceDir, installAgentSkills } from "../dist/agent-skills.js";

async function tempDir(name) {
  return fsp.mkdtemp(path.join(os.tmpdir(), `rudder-agent-skills-${name}-`));
}

test("the shipped Google Cloud skill is installed for Claude and Codex as rudder-google-cloud", async () => {
  const home = await tempDir("home");
  const targets = agentSkillTargets(home, {});
  const result = await installAgentSkills({ targets });
  assert.ok(result.installed.includes("claude:rudder-google-cloud"));
  assert.ok(result.installed.includes("codex:rudder-google-cloud"));
  for (const target of targets) {
    const skill = await fsp.readFile(path.join(target.dir, "rudder-google-cloud", "SKILL.md"), "utf8");
    assert.match(skill, /^---\nname: rudder-google-cloud\ndescription: .+\n---\n/);
    const script = path.join(target.dir, "rudder-google-cloud", "scripts", "gcp.mjs");
    assert.ok((await fsp.stat(script)).mode & 0o100, "helper script is executable");
  }
});

test("the shipped Namecheap skill is installed for Claude and Codex as rudder-namecheap", async () => {
  const home = await tempDir("namecheap");
  const targets = agentSkillTargets(home, {});
  const result = await installAgentSkills({ targets });
  assert.ok(result.installed.includes("claude:rudder-namecheap"));
  assert.ok(result.installed.includes("codex:rudder-namecheap"));
  for (const target of targets) {
    const skill = await fsp.readFile(path.join(target.dir, "rudder-namecheap", "SKILL.md"), "utf8");
    assert.match(skill, /^---\nname: rudder-namecheap\ndescription: .+\n---\n/);
    const script = path.join(target.dir, "rudder-namecheap", "scripts", "namecheap");
    assert.ok((await fsp.stat(script)).mode & 0o100, "helper script is executable");
  }
});

test("installing again rewrites nothing, repairs edits, and leaves the user's own skills alone", async () => {
  const home = await tempDir("idempotent");
  const targets = agentSkillTargets(home, {});
  await installAgentSkills({ targets });
  const mine = path.join(targets[0].dir, "my-skill", "SKILL.md");
  await fsp.mkdir(path.dirname(mine), { recursive: true });
  await fsp.writeFile(mine, "mine");
  assert.equal((await installAgentSkills({ targets })).updatedFiles, 0);

  const shipped = path.join(targets[0].dir, "rudder-google-cloud", "SKILL.md");
  await fsp.writeFile(shipped, "stale");
  assert.equal((await installAgentSkills({ targets })).updatedFiles, 1);
  assert.notEqual(await fsp.readFile(shipped, "utf8"), "stale");
  assert.equal(await fsp.readFile(mine, "utf8"), "mine");
});

test("skill targets honor CLAUDE_CONFIG_DIR and CODEX_HOME", () => {
  const targets = agentSkillTargets("/home/u", { CLAUDE_CONFIG_DIR: "/cfg/claude", CODEX_HOME: "/cfg/codex" });
  assert.deepEqual(targets.map((t) => t.dir), ["/cfg/claude/skills", "/cfg/codex/skills"]);
});

test("a missing source directory installs nothing instead of throwing", async () => {
  const result = await installAgentSkills({ sourceDir: path.join(await tempDir("none"), "absent"), targets: agentSkillTargets(await tempDir("h"), {}) });
  assert.deepEqual(result, { installed: [], updatedFiles: 0, removed: [] });
});

test("every shipped skill has a SKILL.md whose name matches its installed directory", async () => {
  for (const entry of await fsp.readdir(agentSkillsSourceDir(), { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const skill = await fsp.readFile(path.join(agentSkillsSourceDir(), entry.name, "SKILL.md"), "utf8");
    assert.match(skill, new RegExp(`^---\\nname: rudder-${entry.name}\\n`));
  }
});

test("a retired skill's old rudder- copy is removed on install, and nothing else", async () => {
  const home = await tempDir("retired");
  const targets = agentSkillTargets(home, {});
  const old = path.join(targets[0].dir, "rudder-namecheap-dns", "SKILL.md");
  const mine = path.join(targets[0].dir, "namecheap-dns", "SKILL.md");
  for (const file of [old, mine]) {
    await fsp.mkdir(path.dirname(file), { recursive: true });
    await fsp.writeFile(file, "x");
  }
  const result = await installAgentSkills({ targets });
  assert.deepEqual(result.removed, ["claude:rudder-namecheap-dns"]);
  await assert.rejects(fsp.stat(old));
  assert.equal(await fsp.readFile(mine, "utf8"), "x");
});
