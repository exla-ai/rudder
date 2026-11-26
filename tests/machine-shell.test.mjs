import assert from "node:assert/strict";
import test from "node:test";
import { setTimeout as sleep } from "node:timers/promises";
import { createMachineShell } from "../cloud/worker/machine-shell.mjs";

function fakePty() {
  const made = [];
  const spawn = (file, args, options) => {
    const pty = { file, options, written: [], sizes: [], onData(fn) { pty.data = fn; }, onExit(fn) { pty.exit = fn; },
      write(d) { pty.written.push(d); }, resize(c, r) { pty.sizes.push([c, r]); } };
    made.push(pty);
    return pty;
  };
  return { made, spawn };
}

test("a later open repaints what the shell showed, and nothing arrives twice", async () => {
  const { made, spawn } = fakePty();
  const sent = [];
  const shell = createMachineShell({ spawn, cwd: "/home/x", env: { A: "1" }, send: (m) => sent.push(m) });
  assert.deepEqual(shell.open({ cols: 80, rows: 24 }), { replay: "" });
  assert.equal(made[0].file, "/bin/bash");
  made[0].data("$ ");
  await sleep(20);
  assert.deepEqual(sent, [{ type: "shell-output", data: "$ " }]);
  shell.input("ls\r");
  assert.deepEqual(made[0].written, ["ls\r"]);
  made[0].data("ls\r\nfile\r\n");
  // Opened before that batch went out: it goes to the terminals already
  // there, and the new one gets it in the repaint only.
  assert.deepEqual(shell.open({ cols: 100, rows: 30 }), { replay: "$ ls\r\nfile\r\n" });
  assert.deepEqual(sent[1], { type: "shell-output", data: "ls\r\nfile\r\n" });
  assert.deepEqual(made[0].sizes, [[100, 30]]);
  await sleep(20);
  assert.equal(sent.length, 2);
  made[0].exit({ exitCode: 0 });
  assert.deepEqual(sent[2], { type: "shell-exit" });
  assert.deepEqual(shell.open({ cols: 80, rows: 24 }), { replay: "" });
  assert.equal(made.length, 2);
  assert.throws(() => shell.open({ cols: 0, rows: 24 }), /size/);
});
