/** Explicit managed Claude renderer + local control transport. Agent JSON stays
 * on the child pipes; only rendered text reaches the engine-owned terminal. */
import { spawn } from "node:child_process";
import fs from "node:fs/promises";
import { constants } from "node:fs";
import http from "node:http";
import path from "node:path";
import readline from "node:readline";
import { Transform } from "node:stream";
import { WebSocket, WebSocketServer } from "ws";
import { ClaudeSdkProtocol, SDK_FRAME_LIMIT, SDK_PROFILE, SDK_TEXT_LIMIT, SdkControlError, id, object, text } from "./claude-sdk-protocol.js";

export type ClaudeSdkLaunch = {
  format_version: 1;
  run_id: string;
  attempt_id: string;
  socket_path: string;
  claude_program: string;
  claude_args: string[];
  initial_prompt: string | null;
};

export async function readClaudeSdkLaunch(file: string): Promise<ClaudeSdkLaunch> {
  const handle = await fs.open(file, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const stat = await handle.stat();
    if (!stat.isFile() || stat.size > 256 * 1024 || stat.mode & 0o077 || process.getuid && stat.uid !== process.getuid()) throw new Error("Managed Claude launch must be a private regular file");
    const raw: unknown = JSON.parse(await handle.readFile("utf8"));
    const allowed = ["format_version", "run_id", "attempt_id", "socket_path", "claude_program", "claude_args", "initial_prompt"];
    if (!object(raw) || Object.keys(raw).some((key) => !allowed.includes(key)) || raw.format_version !== 1 || !id(raw.run_id) || !id(raw.attempt_id)
        || typeof raw.socket_path !== "string" || !path.isAbsolute(raw.socket_path) || path.basename(raw.socket_path) !== "cx.sock" || Buffer.byteLength(raw.socket_path) >= 104
        || typeof raw.claude_program !== "string" || !raw.claude_program || raw.claude_program.includes("\0") || raw.claude_program.length > 4096
        || !Array.isArray(raw.claude_args) || raw.claude_args.length > 1024 || raw.claude_args.some((arg) => typeof arg !== "string" || arg.includes("\0"))
        || !(raw.initial_prompt === null || text(raw.initial_prompt))) throw new Error("Invalid managed Claude launch");
    const parent = await fs.lstat(path.dirname(raw.socket_path));
    if (!parent.isDirectory() || parent.mode & 0o077 || process.getuid && parent.uid !== process.getuid()) throw new Error("Managed Claude socket directory must be private");
    if (process.env.RUDDER_FABRIC_ATTEMPT_ID && process.env.RUDDER_FABRIC_ATTEMPT_ID !== raw.attempt_id) throw new Error("Managed Claude launch has a stale attempt identity");
    return raw as ClaudeSdkLaunch;
  } finally { await handle.close(); }
}

export function claudeSdkArgv(original: string[]): string[] {
  const args: string[] = [];
  for (let at = 0; at < original.length; at++) {
    const arg = original[at];
    if (["--dangerously-load-development-channels", "--channels", "--remote-control"].includes(arg)) throw new Error("This Claude launch requires the interactive profile");
    if (["--input-format", "--output-format"].includes(arg)) { if (!original[++at]) throw new Error("Missing Claude format option"); continue; }
    if (["-p", "--print", "--verbose", "--replay-user-messages", "--include-partial-messages"].includes(arg)) continue;
    if (arg.startsWith("--input-format=") || arg.startsWith("--output-format=")) continue;
    args.push(arg);
  }
  args.push("--print", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--replay-user-messages", "--include-partial-messages");
  return args;
}

/** Identity comes from the engine's explicit CLI selection, never peer text.
 * A fork without an explicit new id waits for the backend's own init event. */
export function selectedClaudeSdkSession(args: string[]): string | null {
  const selected: string[] = [], resumed: string[] = [];
  for (let at = 0; at < args.length; at++) {
    if (args[at] === "--session-id" || args[at] === "--resume" || args[at] === "-r") {
      const flag = args[at], value = args[++at];
      if (!id(value) || value.startsWith("-")) throw new Error("Managed Claude needs an explicit session id");
      (flag === "--session-id" ? selected : resumed).push(value);
    }
  }
  if (new Set(selected).size > 1 || new Set(resumed).size > 1) throw new Error("Ambiguous managed Claude session selection");
  if (args.includes("--fork-session")) return selected[0] ?? null;
  if (selected.length && resumed.length && selected[0] !== resumed[0]) throw new Error("Conflicting managed Claude session selection");
  return selected[0] ?? resumed[0] ?? null;
}

/** Complete JSON frames are bounded before parsing, even without newlines. */
export class SdkJsonLines {
  private pending = Buffer.alloc(0);
  constructor(private readonly frame: (value: unknown) => void, private readonly failed: (error: Error) => void) {}
  private broken = false;
  push(chunk: Buffer): void {
    if (this.broken) return;
    let start = 0;
    while (start < chunk.length) {
      const end = chunk.indexOf(10, start);
      const part = chunk.subarray(start, end < 0 ? chunk.length : end);
      if (this.pending.length + part.length > SDK_FRAME_LIMIT) { this.fail(new Error("Claude JSON frame exceeds 1 MiB")); return; }
      this.pending = Buffer.concat([this.pending, part]);
      if (end < 0) return;
      this.consume();
      if (this.broken) return;
      start = end + 1;
    }
  }
  end(): void { if (this.pending.length && !this.broken) this.consume(); }
  private consume(): void {
    const raw = this.pending; this.pending = Buffer.alloc(0);
    if (!raw.toString("utf8").trim()) return;
    try { this.frame(JSON.parse(raw.toString("utf8"))); } catch (error) { this.fail(error instanceof Error ? error : new Error("Invalid Claude JSON")); }
  }
  private fail(error: Error): void { this.broken = true; this.failed(error); }
}

function safeDisplay(value: unknown, limit = 8192): string {
  return String(value ?? "").slice(0, limit).replace(/[\x00-\x08\x0b-\x1f\x7f]/g, "");
}

class SdkRenderer {
  private streamed = false;
  constructor(private readonly output: NodeJS.WritableStream) {}
  private write(value: string): void {
    // A detached/slow surface cannot cause an unbounded Node stdout queue.
    if (((this.output as { writableLength?: number }).writableLength ?? 0) < SDK_FRAME_LIMIT) this.output.write(value);
  }
  notice(value: string): void { this.write(`\n[rudder] ${safeDisplay(value)}\n`); }
  frame(value: unknown): void {
    if (!object(value)) return;
    if (value.type === "stream_event" && object(value.event) && object(value.event.delta) && value.event.delta.type === "text_delta") {
      this.streamed = true; this.write(safeDisplay(value.event.delta.text)); return;
    }
    if (value.type === "assistant" && object(value.message) && Array.isArray(value.message.content)) {
      for (const block of value.message.content) if (object(block)) {
        if (block.type === "text" && !this.streamed) this.write(`${safeDisplay(block.text)}\n`);
        else if (block.type === "tool_use") this.write(`\n[tool] ${safeDisplay(block.name, 128)}\n`);
      }
      this.streamed = false;
    } else if (value.type === "tool_progress") this.write(`\n[tool] ${safeDisplay(value.tool_name, 128)} running\n`);
    else if (value.type === "result") {
      this.streamed = false;
      if (value.terminal_reason === "aborted_streaming" || value.terminal_reason === "aborted_tools") this.notice("Turn interrupted.");
      else if (value.is_error === true) this.notice(`Claude reported a failed turn: ${safeDisplay(Array.isArray(value.errors) ? value.errors.join("; ") : value.result, 2048)}`);
      else if (value.num_turns === 0 && value.terminal_reason == null && typeof value.result === "string") this.write(`${safeDisplay(value.result)}\n`);
      else this.write("\n");
    }
  }
}

function boundedHumanInput(input: NodeJS.ReadStream, oversize: () => void, boundary: (discard: boolean) => void): Transform {
  let count = 0, dropping = false, previousCR = false;
  const guarded = new Transform({ transform(chunk: Buffer, _encoding, callback) {
    let begin = 0;
    for (let at = 0; at < chunk.length; at++) {
      if (chunk[at] === 10 || chunk[at] === 13 || chunk[at] === 3) {
        if (!dropping) this.push(chunk.subarray(begin, at));
        if (chunk[at] !== 3 && !(chunk[at] === 10 && previousCR)) boundary(dropping);
        previousCR = chunk[at] === 13;
        this.push(chunk.subarray(at, at + 1)); begin = at + 1; count = 0; dropping = false;
      } else if (!dropping && ++count > SDK_TEXT_LIMIT) {
        previousCR = false;
        this.push(chunk.subarray(begin, at)); begin = at + 1; dropping = true; oversize();
      } else previousCR = false;
    }
    if (!dropping) this.push(chunk.subarray(begin));
    callback();
  } });
  // readline retains the normal terminal editing/key behavior while the guard
  // caps a pasted line before readline can accumulate it without a bound.
  Object.assign(guarded, { isTTY: input.isTTY, setRawMode: input.setRawMode?.bind(input) });
  input.pipe(guarded);
  return guarded;
}

function rpcError(kind: string, message: string): { code: number; message: string } {
  return { code: kind === "unsupported" ? -32011 : -32010, message };
}

/** Hidden CLI: rudder __claude-sdk --launch-file <private JSON>. */
export async function runClaudeSdkBridge(args: string[]): Promise<void> {
  if (args.length !== 2 || args[0] !== "--launch-file") throw new Error("usage: rudder __claude-sdk --launch-file <path>");
  const launch = await readClaudeSdkLaunch(args[1]);
  const cwd = await fs.realpath(process.cwd());
  const renderer = new SdkRenderer(process.stdout);
  const peers = new Set<WebSocket>();
  let controller: WebSocket | null = null, closing = false, stopTimer: NodeJS.Timeout | null = null;
  const server = http.createServer((_request, response) => { response.writeHead(404); response.end(); });
  const sockets = new WebSocketServer({ noServer: true, maxPayload: SDK_FRAME_LIMIT, perMessageDeflate: false });
  const child = spawn(launch.claude_program, claudeSdkArgv(launch.claude_args), { cwd, env: process.env, stdio: ["pipe", "pipe", "pipe"] });
  let childError: Error | null = null, childExit: number | null = null;
  const exited = new Promise<void>((resolve) => {
    child.once("error", (error) => { childError = error; resolve(); });
    child.once("exit", (code) => { childExit = code; resolve(); });
  });
  const protocol = new ClaudeSdkProtocol(launch.run_id, launch.attempt_id, cwd, (frame) => {
    if (!child.stdin.writable) throw new Error("Claude stdin is closed");
    const bytes = `${JSON.stringify(frame)}\n`;
    if (Buffer.byteLength(bytes) > SDK_FRAME_LIMIT || child.stdin.writableLength > 2 * SDK_FRAME_LIMIT) throw new Error("Claude stdin queue is full");
    child.stdin.write(bytes, (error) => { if (error) protocol.failTransport("Claude stdin write failed; do not replay"); });
  }, (frame) => { for (const peer of peers) if (peer.readyState === WebSocket.OPEN) {
    if (peer.bufferedAmount >= SDK_FRAME_LIMIT) peer.close(1013, "Adapter observer fell behind");
    else peer.send(JSON.stringify(frame));
  } }, (message) => renderer.notice(message), 30_000, selectedClaudeSdkSession(launch.claude_args));
  child.stdin.on("error", () => protocol.failTransport("Claude stdin transport closed"));
  const parser = new SdkJsonLines((frame) => { protocol.ingest(frame); renderer.frame(frame); }, (error) => {
    protocol.failTransport(error.message); child.kill("SIGTERM");
  });
  child.stdout.on("data", (chunk: Buffer) => parser.push(chunk));
  child.stdout.on("end", () => parser.end());
  child.stderr.on("data", (chunk: Buffer) => renderer.notice(safeDisplay(chunk.toString("utf8"), 4096)));

  server.on("upgrade", (request, socket, head) => {
    if (request.headers.origin || controller || closing) { socket.end("HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n"); return; }
    sockets.handleUpgrade(request, socket, head, (peer) => sockets.emit("connection", peer));
  });
  sockets.on("connection", (peer: WebSocket) => {
    controller = peer; peers.add(peer); let initialized = false, inFlight = 0;
    peer.on("error", () => {});
    peer.on("close", () => { peers.delete(peer); if (controller === peer) controller = null; });
    const reply = (value: unknown) => { if (peer.readyState === WebSocket.OPEN) {
      if (peer.bufferedAmount >= SDK_FRAME_LIMIT) peer.close(1013, "Adapter observer fell behind");
      else peer.send(JSON.stringify(value));
    } };
    peer.on("message", (data) => {
      void (async () => {
        let raw: unknown;
        try { raw = JSON.parse(data.toString()); } catch { reply({ id: null, error: { code: -32700, message: "Invalid JSON" } }); return; }
        if (!object(raw) || !id(raw.method) || !(Number.isSafeInteger(raw.id) || id(raw.id) || raw.id === undefined)) { reply({ id: null, error: { code: -32600, message: "Invalid local adapter request" } }); return; }
        if (raw.method === "initialized" && raw.id === undefined) return;
        if (inFlight >= 4) { reply({ id: raw.id, error: rpcError("rejected", "Too many adapter requests") }); return; }
        inFlight++;
        try {
          if (raw.method === "initialize") {
            if (initialized) throw new SdkControlError("rejected", "Adapter connection is already initialized");
            initialized = true;
            reply({ id: raw.id, result: { profile: SDK_PROFILE, format_version: 1, run_id: launch.run_id, attempt_id: launch.attempt_id, steer_mode: "interrupt_then_replace" } }); return;
          }
          if (!initialized) throw new SdkControlError("rejected", "Initialize the adapter connection first");
          const params = object(raw.params) ? raw.params : {};
          const thread = protocol.thread();
          if (raw.method === "thread/loaded/list") { reply({ id: raw.id, result: { data: thread ? [thread.id] : [], rudder_uncertain: protocol.snapshot().uncertain } }); return; }
          if (!thread || params.threadId !== thread.id) throw new SdkControlError("rejected", "Managed Claude session is not qualified or does not match");
          if (raw.method === "thread/read" || raw.method === "thread/resume") { reply({ id: raw.id, result: { thread } }); return; }
          if (raw.method === "thread/turns/list") { reply({ id: raw.id, result: { data: thread.turns, nextCursor: null } }); return; }
          if (!["turn/start", "turn/steer", "turn/interrupt"].includes(raw.method)) throw new SdkControlError("unsupported", "Unsupported managed Claude adapter method");
          if (!object(params._rudder) || !object(params._rudder.operation)) throw new SdkControlError("rejected", "Authenticated engine command metadata is required");
          const op = params._rudder.operation.kind;
          if (raw.method === "turn/interrupt" && op !== "interrupt" || raw.method === "turn/steer" && op !== "steer" || raw.method === "turn/start" && op !== "deliver" && op !== "resume") throw new SdkControlError("rejected", "Adapter method and operation disagree");
          if (raw.method === "turn/interrupt" && params.turnId !== params._rudder.expected_turn_id || raw.method === "turn/steer" && params.expectedTurnId !== params._rudder.expected_turn_id) throw new SdkControlError("rejected", "Adapter turn identities disagree");
          const result = await protocol.execute(params._rudder, String(params.threadId));
          reply({ id: raw.id, result });
        } catch (error) {
          const failure = error instanceof SdkControlError ? error : new SdkControlError("uncertain", error instanceof Error ? error.message : "Managed Claude command failed");
          reply(failure.kind === "uncertain" ? { id: raw.id, result: { rudderUncertain: failure.message } } : { id: raw.id, error: rpcError(failure.kind, failure.message) });
        } finally { inFlight--; }
      })();
    });
  });

  let rl: readline.Interface | null = null, guarded: Transform | null = null;
  const stop = () => { if (!closing) { closing = true; child.kill("SIGTERM"); stopTimer = setTimeout(() => {
    if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
  }, 2000); stopTimer.unref(); } };
  const onTerm = () => stop();
  process.on("SIGTERM", onTerm); process.on("SIGHUP", onTerm);
  try {
    await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(launch.socket_path, () => { server.removeListener("error", reject); resolve(); }); });
    await fs.chmod(launch.socket_path, 0o600);
    renderer.notice("Claude managed session. Enter sends; /interrupt stops a turn; /approve or /deny answers a permission request.");
    await Promise.race([protocol.initialize(), exited.then(() => { throw childError ?? new Error("Claude exited before initialization"); })]);
    const discardedLines: boolean[] = [];
    guarded = boundedHumanInput(process.stdin, () => renderer.notice("Input exceeds 64 KiB; that line was discarded."), (discard) => discardedLines.push(discard));
    rl = readline.createInterface({ input: guarded, output: process.stdout, terminal: !!process.stdin.isTTY, prompt: "> ", crlfDelay: Infinity });
    rl.on("line", (line) => {
      if (discardedLines.shift()) return;
      const body = line.trim(); if (!body) return;
      const answer = /^\/(approve|deny) ([a-zA-Z0-9-]+)$/.exec(body);
      if (answer) { try { protocol.answerPermission(answer[2], answer[1] === "approve"); } catch (error) { renderer.notice((error as Error).message); } }
      else if (body === "/interrupt") void protocol.humanInterrupt().catch((error: Error) => renderer.notice(error.message));
      else if (body === "/quit") stop();
      else void protocol.humanInput(body).catch((error: Error) => renderer.notice(error.message));
      if (process.stdin.isTTY) rl?.prompt();
    });
    rl.on("SIGINT", () => { void protocol.humanInterrupt().catch((error: Error) => renderer.notice(error.message)); });
    rl.on("close", stop);
    if (process.stdin.isTTY) rl.prompt();
    if (launch.initial_prompt) void protocol.humanInput(launch.initial_prompt).catch((error: Error) => renderer.notice(error.message));
    await exited;
    if (childError) throw childError;
    if (!closing && childExit !== 0) throw new Error(`Claude managed process exited with code ${childExit ?? "unknown"}`);
  } finally {
    closing = true; protocol.close();
    if (stopTimer) clearTimeout(stopTimer);
    rl?.close();
    if (guarded) { process.stdin.unpipe(guarded); guarded.destroy(); }
    for (const peer of peers) peer.terminate();
    sockets.close(); server.close();
    process.off("SIGTERM", onTerm); process.off("SIGHUP", onTerm);
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGTERM");
      await Promise.race([exited, new Promise<void>((resolve) => { const timer = setTimeout(() => { child.kill("SIGKILL"); resolve(); }, 2000); timer.unref(); })]);
    }
    await fs.unlink(launch.socket_path).catch(() => {});
  }
}
