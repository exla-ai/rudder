import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, readFile, readlink, readdir, symlink, rm, realpath } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import { build } from "esbuild";

async function load(entry) {
  const result = await build({ entryPoints: [new URL(entry, import.meta.url).pathname], bundle: true, write: false, platform: "node", format: "esm", target: "node22" });
  return import(`data:text/javascript;base64,${Buffer.from(result.outputFiles[0].text).toString("base64")}`);
}
const { AgentSetup, setupPaths, skillFrontmatter } = await load("../src/project-files/agent-setup.ts");
const sha = (text) => createHash("sha256").update(text).digest("hex");
const scope = (id) => ({ projectId: "agent-setup", workspaceId: id, attemptId: id, ownerEpoch: 0 });

test("one instructions file and shared skills for both agents; sign-in files are never served", async (t) => {
  const home = await realpath(await mkdtemp(path.join(os.tmpdir(), "rudder-agent-setup-")));
  t.after(() => rm(home, { recursive: true, force: true }));
  const claude = path.join(home, ".claude"), codex = path.join(home, ".codex");
  const project = path.join(home, "work", "app");
  await mkdir(path.join(claude, "skills", "alpha"), { recursive: true });
  await mkdir(path.join(claude, "skills", "beta"), { recursive: true });
  await mkdir(path.join(claude, "skills", "synced", "x"), { recursive: true });
  await mkdir(path.join(codex, "skills", "alpha"), { recursive: true });
  await mkdir(project, { recursive: true });
  await writeFile(path.join(claude, "CLAUDE.md"), "# Claude rules\n");
  await writeFile(path.join(claude, "settings.json"), "{}");
  await writeFile(path.join(claude, ".credentials.json"), "{\"token\":\"secret\"}");
  await writeFile(path.join(codex, "AGENTS.md"), "# Codex rules\n");
  await writeFile(path.join(codex, "auth.json"), "{\"token\":\"secret\"}");
  await writeFile(path.join(claude, "skills", "alpha", "SKILL.md"), "---\nname: alpha\ndescription: \"Claude's alpha\"\n---\nBody\n");
  await writeFile(path.join(claude, "skills", "alpha", "auth.json"), "secret");
  await writeFile(path.join(claude, "skills", "beta", "SKILL.md"), "---\nname: beta\ndescription: Beta things\n---\n");
  await writeFile(path.join(codex, "skills", "alpha", "SKILL.md"), "---\nname: alpha\ndescription: Codex's alpha\n---\n");
  // A "skill" that is really the Codex config folder, where auth.json lives.
  await symlink(codex, path.join(claude, "skills", "sneaky"));
  const setup = new AgentSetup({ home, paths: setupPaths({}, home), projectDirs: () => [project] });

  // Separate files: both shown, not shared. Linking needs the revision the user saw.
  let view = await setup.overview();
  assert.equal(view.instructions.shared, false);
  assert.deepEqual([view.instructions.claude.state, view.instructions.codex.state], ["file", "file"]);
  await assert.rejects(setup.handle({ version: 1, operation: "link-instructions", keep: "claude" }), { code: "changed" });
  view = await setup.handle({ version: 1, operation: "link-instructions", keep: "claude", replacedRevision: sha("# Codex rules\n") });
  assert.equal(view.instructions.shared, true);
  assert.equal(view.instructions.codex.state, "link");
  assert.equal(await readlink(path.join(codex, "AGENTS.md")), path.join(claude, "CLAUDE.md"));
  const kept = (await readdir(codex)).find((name) => name.startsWith("AGENTS.md.replaced-"));
  assert.equal(await readFile(path.join(codex, kept), "utf8"), "# Codex rules\n");
  // Codex's override wins while it has text, and then the files differ again.
  await writeFile(path.join(codex, "AGENTS.override.md"), "  \n");
  assert.equal((await setup.overview()).instructions.shared, true);
  await writeFile(path.join(codex, "AGENTS.override.md"), "Override\n");
  view = await setup.overview();
  assert.equal(view.instructions.shared, false);
  assert.equal(view.instructions.codex.path, "~/.codex/AGENTS.override.md");
  await rm(path.join(codex, "AGENTS.override.md"));

  // The instructions root shows that one file and saves it with baseRevision.
  const list = await setup.handle({ version: 1, operation: "list", scope: scope("instructions:codex"), path: "" });
  assert.deepEqual(list.entries.map((e) => e.name), ["CLAUDE.md"]);
  await assert.rejects(setup.handle({ version: 1, operation: "read", scope: scope("instructions:claude"), path: "settings.json" }), { code: "denied" });
  const saved = await setup.handle({ version: 1, operation: "write", scope: scope("instructions:codex"), path: "CLAUDE.md", baseRevision: sha("# Claude rules\n"), text: "# Both\n" });
  assert.equal(saved.outcome, "saved");
  assert.equal(await readFile(path.join(claude, "CLAUDE.md"), "utf8"), "# Both\n");

  // Skills: separate copies are flagged; sharing links one folder into the other agent.
  const row = (name) => view.skills.filter((s) => s.name === name);
  view = await setup.overview();
  assert.deepEqual(row("alpha").map((s) => s.agents), [["claude"], ["codex"]]);
  assert.match(row("alpha")[0].note, /own copy/);
  assert.equal(view.skills.some((s) => s.name === "synced"), false);
  assert.equal(row("sneaky")[0]?.root ?? null, null);
  await assert.rejects(setup.handle({ version: 1, operation: "share-skill", name: "alpha", from: "claude" }), { code: "invalid" });
  view = await setup.handle({ version: 1, operation: "share-skill", name: "beta", from: "claude" });
  assert.deepEqual(row("beta")[0].agents, ["claude", "codex"]);
  view = await setup.handle({ version: 1, operation: "new-skill", name: "gamma", description: "Use when testing" });
  assert.deepEqual(row("gamma")[0].agents, ["claude", "codex"]);
  assert.match(await readFile(path.join(home, ".agents", "skills", "gamma", "SKILL.md"), "utf8"), /description: Use when testing/);
  assert.equal(await readlink(path.join(claude, "skills", "gamma")), path.join(home, ".agents", "skills", "gamma"));

  // Sign-in files: refused by name, and a root never reaches an agent config folder.
  const alpha = await setup.handle({ version: 1, operation: "list", scope: scope("skill:claude:alpha"), path: "" });
  assert.deepEqual(alpha.entries.map((e) => e.name), ["SKILL.md"]);
  await assert.rejects(setup.handle({ version: 1, operation: "read", scope: scope("skill:claude:alpha"), path: "auth.json" }), { code: "denied" });
  await assert.rejects(setup.handle({ version: 1, operation: "read", scope: scope("skill:claude:sneaky"), path: "AGENTS.md" }), { code: "denied" });
  await assert.rejects(setup.handle({ version: 1, operation: "list", scope: scope("folder:../.claude"), path: "" }), { code: "denied" });

  // A project's CLAUDE.md that links to or imports AGENTS.md: both agents read it.
  await writeFile(path.join(project, "AGENTS.md"), "# Project\n");
  await symlink("AGENTS.md", path.join(project, "CLAUDE.md"));
  let guide = await setup.projectGuide(project);
  assert.equal(guide.shared, true);
  assert.deepEqual(guide.files.find((f) => f.path === "AGENTS.md").readBy, ["claude", "codex"]);
  assert.equal(guide.files.find((f) => f.path === "CLAUDE.md").linksTo, "AGENTS.md");
  await rm(path.join(project, "CLAUDE.md"));
  await writeFile(path.join(project, "CLAUDE.md"), "See @AGENTS.md\n");
  guide = await setup.projectGuide(project);
  assert.equal(guide.shared, true);
  await writeFile(path.join(project, "CLAUDE.md"), "Claude only\n");
  assert.equal((await setup.projectGuide(project)).shared, false);
});

test("a skill description written as YAML block text reads as its text, not the marker", () => {
  const folded = "---\nname: leads\ndescription: |\n  Find companies that run macOS CI\n  and rank them.\nmetadata:\n  kind: x\n---\n# Leads\n";
  assert.deepEqual(skillFrontmatter(folded), { name: "leads", description: "Find companies that run macOS CI\nand rank them." });
  assert.equal(skillFrontmatter("---\nname: a\ndescription: >-\n  One\n  line.\n---\n").description, "One line.");
  assert.equal(skillFrontmatter("---\nname: a\ndescription: Starts here\n  and goes on.\n---\n").description, "Starts here and goes on.");
  assert.equal(skillFrontmatter("---\nname: a\ndescription: \"Quoted\"\n---\n").description, "Quoted");
});
