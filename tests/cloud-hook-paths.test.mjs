// Laptop hook configs on the home machine: /Users/<name> paths are rewritten
// to the machine's home, and hooks whose program is still missing are dropped.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { absoluteProgram, rewriteHomePrefix, sanitizeHookConfig, sanitizeHomeHookConfigs } from "../cloud/worker/hook-paths.mjs";

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
