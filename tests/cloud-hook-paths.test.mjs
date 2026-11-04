// Laptop hook configs on the home machine: /Users/<name> paths are rewritten
// to the machine's home, and hooks whose program is still missing are dropped.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { absoluteProgram, carryCodexHookTrust, codexHookHash, rewriteHomePrefix, sanitizeHookConfig, sanitizeHomeHookConfigs } from "../cloud/worker/hook-paths.mjs";

const HOME = "/home/rudder";

test("laptop home prefixes are rewritten to this machine's home", () => {
  assert.equal(rewriteHomePrefix("/Users/viraat/.claude/hooks/eta.py pre", HOME), "/home/rudder/.claude/hooks/eta.py pre");
  assert.equal(rewriteHomePrefix("python3 /Users/viraat/.claude/hooks/x.py", HOME), "python3 /home/rudder/.claude/hooks/x.py");
  assert.equal(rewriteHomePrefix('"/home/alice/bin/h" --flag', HOME), '"/home/rudder/bin/h" --flag');
  assert.equal(rewriteHomePrefix("PATH=/Users/v/bin:$PATH run", HOME), "PATH=/home/rudder/bin:$PATH run");
  assert.equal(rewriteHomePrefix("/home/rudder/.claude/hooks/a.sh", HOME), "/home/rudder/.claude/hooks/a.sh");
  assert.equal(rewriteHomePrefix("/usr/local/bin/notify /opt/Users/x", HOME), "/usr/local/bin/notify /opt/Users/x");
  assert.equal(rewriteHomePrefix("$HOME/.claude/hooks/a.sh", HOME), "$HOME/.claude/hooks/a.sh");
});

test("only an absolute first token counts as the program", () => {
  assert.equal(absoluteProgram("/a/b.py pre"), "/a/b.py");
  assert.equal(absoluteProgram('"/a b/c.sh" x'), "/a b/c.sh");
  assert.equal(absoluteProgram("/a/b.sh;echo"), "/a/b.sh");
  for (const command of ["python3 /a/b.py", "[ -x /a ] && /a", "$HOME/x", "FOO=1 /a", "cd /x && y", "~/x"]) {
    assert.equal(absoluteProgram(command), null, command);
  }
});

test("missing programs are dropped, empty groups and events removed, the rest kept", () => {
  const present = new Set(["/home/rudder/.claude/hooks/eta.py"]);
  const config = {
    model: "opus",
    hooks: {
      PreToolUse: [
        { matcher: "Bash", hooks: [{ type: "command", command: "/Users/viraat/.claude/hooks/eta.py pre" }] },
        { matcher: "Edit", hooks: [{ type: "command", command: "/Users/viraat/bin/gone.sh" }] },
      ],
      PostToolUse: [
        { matcher: "*", hooks: [
          { type: "command", command: "/opt/homebrew/bin/terminal-notifier -m hi" },
          { type: "command", command: "[ -x /x ] && /x" },
        ] },
      ],
      Stop: [{ hooks: [{ type: "command", command: "/Users/viraat/.local/bin/ding" }] }],
    },
  };
  const before = JSON.stringify(config);
  const out = sanitizeHookConfig(config, { home: HOME, exists: (p) => present.has(p) });
  assert.equal(JSON.stringify(config), before, "input is not mutated");
  assert.equal(out.changed, true);
  assert.deepEqual(out.config, {
    model: "opus",
    hooks: {
      PreToolUse: [{ matcher: "Bash", hooks: [{ type: "command", command: "/home/rudder/.claude/hooks/eta.py pre" }] }],
      PostToolUse: [{ matcher: "*", hooks: [{ type: "command", command: "[ -x /x ] && /x" }] }],
    },
  });
  assert.deepEqual(out.dropped.map((d) => d.missing), ["/home/rudder/bin/gone.sh", "/opt/homebrew/bin/terminal-notifier", "/home/rudder/.local/bin/ding"]);
});

test("a config with nothing to fix is returned untouched", () => {
  const config = { hooks: { Stop: [{ hooks: [{ type: "command", command: "echo done" }] }] } };
  const out = sanitizeHookConfig(config, { home: HOME, exists: () => false });
  assert.equal(out.changed, false);
  assert.equal(out.config, config);
  assert.equal(sanitizeHookConfig({ theme: "dark" }, { home: HOME }).changed, false);
});

test("the home's settings.json and codex hooks.json are fixed on disk, and synced hook scripts made executable", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-hooks-"));
  try {
    fs.mkdirSync(path.join(home, ".claude", "hooks"), { recursive: true });
    fs.mkdirSync(path.join(home, ".codex"), { recursive: true });
    const script = path.join(home, ".claude", "hooks", "eta.py");
    fs.writeFileSync(script, "#!/usr/bin/env python3\n", { mode: 0o600 });
    fs.writeFileSync(path.join(home, ".claude", "settings.json"), JSON.stringify({
      hooks: {
        PostToolUse: [{ matcher: "Bash", hooks: [
          { type: "command", command: "/Users/viraat/.claude/hooks/eta.py post" },
          { type: "command", command: "/Users/viraat/.claude/hooks/missing.py" },
        ] }],
      },
    }));
    fs.writeFileSync(path.join(home, ".codex", "hooks.json"), JSON.stringify({
      hooks: { Stop: [{ hooks: [{ type: "command", command: "/Users/viraat/gone" }] }] },
    }));
    const lines = [];
    sanitizeHomeHookConfigs(home, (line) => lines.push(line));
    const settings = JSON.parse(fs.readFileSync(path.join(home, ".claude", "settings.json"), "utf8"));
    assert.deepEqual(settings.hooks.PostToolUse[0].hooks, [{ type: "command", command: `${script} post` }]);
    const codex = JSON.parse(fs.readFileSync(path.join(home, ".codex", "hooks.json"), "utf8"));
    assert.equal(codex.hooks, undefined);
    assert.ok(fs.statSync(script).mode & 0o100, "hook script is executable");
    assert.ok(lines.some((l) => l.includes("dropped PostToolUse") && l.includes("missing.py")), lines.join("\n"));
    assert.ok(lines.some((l) => l.includes(".codex/hooks.json: dropped Stop")), lines.join("\n"));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test("a hook whose command reads a missing home path is dropped, a guarded one is kept", async () => {
  const { missingHomePath } = await import("../cloud/worker/hook-paths.mjs");
  const exists = (p) => p === "/home/rudder/ok.mjs";
  assert.equal(missingHomePath('node "/home/rudder/.codex/libra-hooks/hook.mjs" codex', "/home/rudder", exists), "/home/rudder/.codex/libra-hooks/hook.mjs");
  assert.equal(missingHomePath('SUPERSET_AGENT_ID=codex "/home/rudder/.superset/notify.sh"', "/home/rudder", exists), "/home/rudder/.superset/notify.sh");
  assert.equal(missingHomePath("if [ -f '/home/rudder/x/i.js' ]; then node '/home/rudder/x/i.js'; fi", "/home/rudder", exists), null);
  assert.equal(missingHomePath("node /home/rudder/ok.mjs", "/home/rudder", exists), null);
});

// Hashes Codex itself recorded for these laptop hooks (its ~/.codex/config.toml).
const LIBRA = 'node "/Users/viraat/.codex/libra-hooks/libra-hook.mjs" codex';
const LEDGER = (event) => `if [ -f '/Users/viraat/code/ledger-agent-plugin/dist/hook/index.js' ]; then node '/Users/viraat/code/ledger-agent-plugin/dist/hook/index.js' ${event}; fi`;

test("the Codex declaration hash matches what Codex records", () => {
  assert.equal(codexHookHash("UserPromptSubmit", undefined, { type: "command", command: LIBRA, timeout: 10 }),
    "sha256:0ba4d062fee2b8eb6ebf55dacd4165d5e5151aee137227bd761460e5d51c619a");
  // No timeout: 600s by default, 1s for SessionEnd.
  assert.equal(codexHookHash("SessionStart", undefined, { type: "command", command: LEDGER("SessionStart") }),
    "sha256:a15becd5234a2dcedd507d6afa507a26392d49afd5403f3aac0678c77984ecdb");
  assert.equal(codexHookHash("SessionEnd", undefined, { type: "command", command: LEDGER("SessionEnd") }),
    "sha256:c440185ba13a21befff5e33788d1c7c86d959321f89315d8133ebbc4700a0314");
  // UserPromptSubmit has no matcher, so one in the file changes nothing.
  assert.equal(codexHookHash("UserPromptSubmit", "x", { type: "command", command: LIBRA, timeout: 10 }),
    codexHookHash("UserPromptSubmit", undefined, { type: "command", command: LIBRA, timeout: 10 }));
  assert.equal(codexHookHash("Stop", undefined, { type: "mcp_tool", server: "s", tool: "t" }), null);
});

test("only hooks approved on the laptop keep their approval under the new key", () => {
  const laptopConfig = [
    'model = "gpt"',
    "",
    "[hooks.state]",
    "",
    '[hooks.state."/Users/viraat/.codex/hooks.json:user_prompt_submit:3:0"]',
    'trusted_hash = "sha256:0ba4d062fee2b8eb6ebf55dacd4165d5e5151aee137227bd761460e5d51c619a"',
    "",
    // A plugin's approval is never reused for the user's hooks.json.
    '[hooks.state."vercel@claude-plugins-official:hooks/hooks.json:session_start:0:0"]',
    `trusted_hash = "${codexHookHash("SessionStart", undefined, { type: "command", command: LEDGER("SessionStart") })}"`,
    "",
  ].join("\n");
  const hooks = {
    hooks: {
      UserPromptSubmit: [{ hooks: [{ type: "command", command: LIBRA, timeout: 10 }] }],
      SessionStart: [{ hooks: [{ type: "command", command: LEDGER("SessionStart") }] }],
      Stop: [{ hooks: [{ type: "command", command: "/Users/viraat/new-hook.sh" }] }],
    },
  };
  const { kept } = sanitizeHookConfig(hooks, { home: HOME, exists: () => true });
  assert.deepEqual(kept.map((hook) => [hook.event, hook.group, hook.handler]), [["UserPromptSubmit", 0, 0], ["SessionStart", 0, 0], ["Stop", 0, 0]]);
  const next = carryCodexHookTrust(laptopConfig, { hooksPaths: ["/home/rudder/.codex/hooks.json"], kept });
  const rewritten = codexHookHash("UserPromptSubmit", undefined, { type: "command", command: 'node "/home/rudder/.codex/libra-hooks/libra-hook.mjs" codex', timeout: 10 });
  assert.equal(next, `${laptopConfig}\n[hooks.state."/home/rudder/.codex/hooks.json:user_prompt_submit:0:0"]\ntrusted_hash = "${rewritten}"\n`);
  assert.equal(carryCodexHookTrust(next, { hooksPaths: ["/home/rudder/.codex/hooks.json"], kept }), null, "idempotent");
  assert.equal(carryCodexHookTrust('model = "gpt"\n', { hooksPaths: ["/home/rudder/.codex/hooks.json"], kept }), null, "nothing approved");
});

test("a home's boot carries Codex approvals into its own config.toml", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-codex-trust-"));
  try {
    fs.mkdirSync(path.join(home, ".codex", "libra-hooks"), { recursive: true });
    fs.writeFileSync(path.join(home, ".codex", "libra-hooks", "libra-hook.mjs"), "");
    fs.writeFileSync(path.join(home, ".codex", "hooks.json"), JSON.stringify({
      hooks: { UserPromptSubmit: [{ hooks: [{ type: "command", command: LIBRA, timeout: 10 }] }] },
    }));
    const config = [
      '[hooks.state."/Users/viraat/.codex/hooks.json:user_prompt_submit:0:0"]',
      'trusted_hash = "sha256:0ba4d062fee2b8eb6ebf55dacd4165d5e5151aee137227bd761460e5d51c619a"',
      "",
    ].join("\n");
    fs.writeFileSync(path.join(home, ".codex", "config.toml"), config, { mode: 0o600 });
    const logs = [];
    sanitizeHomeHookConfigs(home, (line) => logs.push(line));
    const text = fs.readFileSync(path.join(home, ".codex", "config.toml"), "utf8");
    const rewritten = codexHookHash("UserPromptSubmit", undefined, { type: "command", command: `node "${home}/.codex/libra-hooks/libra-hook.mjs" codex`, timeout: 10 });
    assert.ok(text.startsWith(config));
    assert.ok(text.includes(`[hooks.state."${home}/.codex/hooks.json:user_prompt_submit:0:0"]\ntrusted_hash = "${rewritten}"`), text);
    assert.equal(fs.statSync(path.join(home, ".codex", "config.toml")).mode & 0o777, 0o600);
    assert.ok(logs.some((line) => /kept the laptop's approval for 1 hook entry/.test(line)), logs.join("\n"));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});
