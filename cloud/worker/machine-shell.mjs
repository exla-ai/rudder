// The machine's shell: one login shell on the home, typed into from the app's
// Terminal (cloud/app/shell-panel.js) through Cloud (server.ts, the shell
// relay). It starts on the first open and stays up between visits, so a
// reload or another device picks it back up; once it exits, the next open
// starts a new one. Output goes to Cloud in small batches as `shell-output`
// control messages. The newest REPLAY_CHARS of it are kept to repaint a
// terminal that opens later. Nothing typed or shown is logged.

const REPLAY_CHARS = 256 * 1024;
const BATCH_MS = 5;

const size = (value, max) => {
  if (!Number.isInteger(value) || value < 2 || value > max) throw new Error("the terminal's size is not valid");
  return value;
};

/** `spawn` is node-pty's; `send(message)` posts one control message to Cloud. */
export function createMachineShell({ spawn, cwd, env, send }) {
  let term = null;
  let replay = "";
  let pending = "";
  let timer = null;

  const flush = () => {
    clearTimeout(timer);
    timer = null;
    if (!pending) return;
    const data = pending;
    pending = "";
    send({ type: "shell-output", data });
  };

  const start = (cols, rows) => {
    const shell = spawn("/bin/bash", ["-l"], { name: "xterm-256color", cols, rows, cwd, env: { ...env, TERM: "xterm-256color" } });
    term = shell;
    replay = "";
    shell.onData((data) => {
      if (term !== shell) return;
      replay = (replay + data).slice(-REPLAY_CHARS);
      pending += data;
      timer ??= setTimeout(flush, BATCH_MS);
    });
    shell.onExit(() => {
      if (term !== shell) return;
      flush();
      term = null;
      send({ type: "shell-exit" });
    });
  };

  return {
    /** A terminal opened: the shell at that size, and what to repaint. What
     * was waiting goes out first, so nothing in `replay` arrives twice. */
    open({ cols, rows }) {
      size(cols, 1000);
      size(rows, 500);
      if (term) {
        flush();
        term.resize(cols, rows);
      } else start(cols, rows);
      return { replay };
    },
    input(data) {
      if (term && typeof data === "string" && data.length <= 65536) term.write(data);
    },
    resize({ cols, rows }) {
      if (term) term.resize(size(cols, 1000), size(rows, 500));
    },
  };
}
