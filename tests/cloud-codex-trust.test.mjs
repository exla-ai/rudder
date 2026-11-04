// The home keeps Codex trust entries for its own repos in the vault-written
// ~/.codex/config.toml (cloud/worker/codex-trust.mjs), so a resumed Codex agent
// never stops at "Trust this folder?" after a restart.
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { ensureCodexTrust, homeRepoDirs, withCodexTrust } from "../cloud/worker/codex-trust.mjs";

const LAPTOP = `model = "gpt-6.1-sol"

[projects."/Users/viraat/code/rudder"]
trust_level = "trusted"
`;

test("adds a trusted table per repo and keeps everything else", () => {
  const next = withCodexTrust(LAPTOP, ["/workspace/code/exla-ai-rudder", "/workspace/code/offstage"]);
  assert.ok(next.startsWith(LAPTOP), "the laptop's config is untouched");
  assert.match(next, /\n\[projects\."\/workspace\/code\/exla-ai-rudder"\]\ntrust_level = "trusted"\n/);
  assert.match(next, /\n\[projects\."\/workspace\/code\/offstage"\]\ntrust_level = "trusted"\n/);
  assert.equal(withCodexTrust(next, ["/workspace/code/offstage"]), next, "idempotent");
});

test("a table the user already wrote wins, even untrusted", () => {
  const mine = `${LAPTOP}\n[projects."/workspace/code/secret"]\ntrust_level = "untrusted"\n`;
  assert.equal(withCodexTrust(mine, ["/workspace/code/secret"]), mine);
});

test("quotes and backslashes in a path stay valid TOML", () => {
  assert.match(withCodexTrust("", ['/w/a"b\\c']), /\[projects\."\/w\/a\\"b\\\\c"\]/);
  assert.equal(withCodexTrust("x = 1", ["/w"]), 'x = 1\n\n[projects."/w"]\ntrust_level = "trusted"\n');
});

test("ensureCodexTrust trusts each repo under the code dir, by real path, after a vault rewrite", () => {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "rudder-codex-trust-")));
  const code = path.join(root, "code");
  fs.mkdirSync(path.join(code, "rudder"), { recursive: true });
  fs.mkdirSync(path.join(code, "offstage"));
  fs.mkdirSync(path.join(code, ".hidden"));
  fs.writeFileSync(path.join(code, "notes.txt"), "not a repo");
  const link = path.join(root, "code-link");
  fs.symlinkSync(code, link);
  const config = path.join(root, ".codex", "config.toml");

  assert.deepEqual(homeRepoDirs(link), [path.join(code, "offstage"), path.join(code, "rudder")]);
  assert.equal(ensureCodexTrust({ configPath: config, codeDir: link }), true, "creates the file");
  assert.equal(ensureCodexTrust({ configPath: config, codeDir: link }), false, "nothing to add the second time");
  assert.equal(fs.statSync(config).mode & 0o777, 0o600);

  // The vault writes the laptop's copy again; the next call restores trust.
  fs.writeFileSync(config, LAPTOP);
  assert.equal(ensureCodexTrust({ configPath: config, codeDir: link }), true);
  const text = fs.readFileSync(config, "utf8");
  assert.ok(text.startsWith(LAPTOP));
  assert.ok(text.includes(`[projects."${path.join(code, "rudder")}"]`));
  assert.ok(text.includes(`[projects."${path.join(code, "offstage")}"]`));
  assert.ok(!text.includes(".hidden") && !text.includes("notes.txt"));
  assert.equal(ensureCodexTrust({ configPath: config, codeDir: path.join(root, "missing") }), false);
});
