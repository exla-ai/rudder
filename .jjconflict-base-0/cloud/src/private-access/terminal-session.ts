import { StringDecoder } from "node:string_decoder";
import { performance } from "node:perf_hooks";

/** Opt-in screen protocol. Only the authoritative emulator creates screen and
 * scrollback; never populate these from a raw escape-byte replay tail. Every
 * screen is a complete parser-generated ANSI repaint, including cursor/modes.
 * Incomplete UTF-8/OSC/CSI/synchronized updates stay in that emulator. */
export interface TerminalScreen {
  format: "vt100-ansi-v1";
  columns: number;
  rows: number;
  screen: string;
  scrollback: string[];
  text?: string;
}
export interface TerminalCursor { generation: string; sequence: number }
export interface ScreenFrame extends TerminalCursor {
  version: 1;
  type: "terminal-screen";
  reset: boolean;
  gap: boolean;
  screen: TerminalScreen;
}
export interface TerminalSocket {
  readyState: number;
  bufferedAmount: number;
  send(data: string, callback: (error?: Error) => void): void;
  close?(): void;
}
export interface TerminalTrace {
  correlationId: string;
  phase: "relay-arrival" | "input-dispatch" | "screen-send" | "gap" | "disconnect";
  localMs: number;
  clock: "process-monotonic";
}
interface Timing {
  now(): number;
  set(callback: () => void, ms: number): unknown;
  clear(timer: unknown): void;
}
const timing: Timing = {
  now: () => performance.now(),
  set: (callback, ms) => setTimeout(callback, ms),
  clear: (timer) => clearTimeout(timer as ReturnType<typeof setTimeout>),
};
export interface TerminalView {
  readonly clientId: string;
  readonly connection: number;
  readonly socket: TerminalSocket;
}
interface View extends TerminalView {
  decoder: StringDecoder;
  firstInput: boolean;
  sent?: TerminalCursor;
  awaiting?: TerminalCursor;
  initial?: TerminalCursor;
  pending: boolean;
  resetRequested: boolean;
  inflight: boolean;
  timer?: unknown;
  deadline?: number;
  exitSent: boolean;
}

/** Per authorized project/PTY. Construction and attach must follow existing
 * Rudder tenant/project checks. This object grants no scheduling authority and
 * terminal delivery is never an operation receipt. It buffers no input. */
export class TerminalScreenSession {
  #views = new Map<string, View>();
  #controller?: View;
  #connection = 0;
  #disposed = false;
  #sequence = 0;
  #generation: string;
  #latest?: TerminalScreen;
  #exit?: number;
  #flushTimer?: unknown;
  readonly #timing: Timing;
  constructor(generation: string, readonly options: {
    writeInput: (text: string, owner: string, correlationId: string) => void;
    resize?: (columns: number, rows: number, owner: string) => void;
    trace?: (event: TerminalTrace) => void;
    timing?: Timing;
    batchMs?: number;
    maxScreenBytes?: number;
    maxInputBytes?: number;
    maxViews?: number;
    highWaterBytes?: number;
    stalledMs?: number;
  }) {
    if (!generation) throw new Error("terminal owner generation is required");
    this.#generation = generation;
    this.#timing = options.timing ?? timing;
  }
  #trace(phase: TerminalTrace["phase"], correlationId: string): void {
    this.options.trace?.({ phase, correlationId, localMs: this.#timing.now(), clock: "process-monotonic" });
  }
  #current(view: TerminalView): view is View {
    return this.#views.get(view.clientId) === view && view.socket.readyState === 1;
  }
  get generation(): string { return this.#generation; }
  get cursor(): TerminalCursor { return { generation: this.generation, sequence: this.#sequence }; }
  get controller(): string | undefined { return this.#controller?.clientId; }
  get viewCount(): number { return this.#views.size; }
  attach(clientId: string, socket: TerminalSocket, cursor?: TerminalCursor): TerminalView {
    if (this.#disposed) throw new Error("terminal session is disposed");
    if (!clientId || socket.readyState !== 1) throw new Error("live identified terminal view required");
    const previous = this.#views.get(clientId);
    const controlled = previous === this.#controller;
    if (previous) { this.#remove(previous, false); if (previous.socket !== socket) previous.socket.close?.(); }
    if (this.#views.size >= (this.options.maxViews ?? 32)) throw new Error("terminal viewer limit reached");
    const view: View = { clientId, socket, connection: ++this.#connection, decoder: new StringDecoder("utf8"),
      firstInput: true, pending: true, resetRequested: true, inflight: false, exitSent: false, initial: cursor && { ...cursor } };
    this.#views.set(clientId, view);
    if (!this.#controller || controlled) this.#controller = view;
    this.#pump(view);
    return view;
  }
  #remove(view: View, promote: boolean): void {
    if (this.#views.get(view.clientId) !== view) return;
    this.#views.delete(view.clientId);
    if (view.timer !== undefined) this.#timing.clear(view.timer);
    view.decoder = new StringDecoder("utf8");
    if (this.#controller === view) {
      this.#controller = undefined;
      if (promote) {
        this.#controller = this.#views.values().next().value;
        if (this.#controller) {
          this.#controller.decoder = new StringDecoder("utf8");
          this.#controller.firstInput = true;
        }
      }
    }
    this.#trace("disconnect", `connection:${view.connection}`);
  }
  detach(view: TerminalView): void { if (this.#views.get(view.clientId) === view) this.#remove(view as View, true); }
  /** Fences old callbacks AND input tokens, even when a client id is reused. */
  replaceOwner(generation: string): void {
    if (this.#disposed) throw new Error("terminal session is disposed");
    if (!generation || generation === this.generation) throw new Error("a fresh terminal owner generation is required");
    for (const view of [...this.#views.values()]) { this.#remove(view, false); view.socket.close?.(); }
    if (this.#flushTimer !== undefined) this.#timing.clear(this.#flushTimer);
    this.#flushTimer = undefined;
    this.#generation = generation;
    this.#sequence = 0; this.#latest = undefined; this.#exit = undefined;
  }
  input(view: TerminalView, generation: string, bytes: Uint8Array, correlationId: string): boolean {
    if (!this.#current(view) || this.#controller !== view || generation !== this.generation || this.#exit !== undefined
      || bytes.byteLength > (this.options.maxInputBytes ?? 64 * 1024) || !correlationId || correlationId.length > 128) return false;
    this.#trace("relay-arrival", correlationId);
    let data = Buffer.from(bytes);
    if (view.firstInput) {
      let start = 0;
      while (start < data.length && (data[start] & 0xc0) === 0x80) start++;
      data = data.subarray(start);
      if (data.length) view.firstInput = false;
    }
    const text = view.decoder.write(data);
    if (text) { this.#trace("input-dispatch", correlationId); this.options.writeInput(text, generation, correlationId); }
    return true;
  }
  requestSnapshot(view: TerminalView, cursor?: TerminalCursor): boolean {
    if (!this.#current(view)) return false;
    view.pending = true; view.resetRequested = true;
    view.initial = cursor && { ...cursor };
    view.awaiting = undefined;
    this.#pump(view); return true;
  }
  /** Acknowledges rendered state only, never a command or agent action. The
   * viewer sends this after its generation-fenced actual paint callback. */
  acknowledge(view: TerminalView, cursor: TerminalCursor): boolean {
    if (!this.#current(view) || !view.awaiting || view.awaiting.generation !== cursor.generation
      || view.awaiting.sequence !== cursor.sequence) return false;
    view.awaiting = undefined;
    view.deadline = undefined;
    if (view.timer !== undefined) this.#timing.clear(view.timer);
    view.timer = undefined;
    this.#pump(view); return true;
  }
  resize(view: TerminalView, generation: string, columns: number, rows: number): boolean {
    if (!this.#current(view) || this.#controller !== view || generation !== this.generation || this.#exit !== undefined
      || !Number.isInteger(columns) || !Number.isInteger(rows) || columns < 1 || rows < 1 || columns > 1000 || rows > 1000) return false;
    this.options.resize?.(columns, rows, generation); return true;
  }
  publish(screen: TerminalScreen): void {
    if (this.#disposed) throw new Error("terminal session is disposed");
    if (this.#exit !== undefined) throw new Error("terminal already exited");
    if (!screen || typeof screen !== "object" || typeof screen.screen !== "string" || !Array.isArray(screen.scrollback)
      || screen.format !== "vt100-ansi-v1" || !Number.isInteger(screen.columns) || !Number.isInteger(screen.rows)
      || screen.columns < 1 || screen.rows < 1 || screen.columns > 1000 || screen.rows > 1000
      || screen.scrollback.length > 10000) throw new Error("invalid terminal screen");
    const limit = this.options.maxScreenBytes ?? 2 * 1024 * 1024;
    if (screen.text !== undefined && typeof screen.text !== "string") throw new Error("invalid terminal text");
    let bytes = Buffer.byteLength(screen.screen) + Buffer.byteLength(screen.text ?? "");
    for (const line of screen.scrollback) {
      if (typeof line !== "string") throw new Error("invalid terminal history");
      bytes += Buffer.byteLength(line); if (bytes > limit) break;
    }
    if (bytes > limit) throw new Error("terminal screen exceeds its byte budget");
    // Copy to fence caller mutation. Exactly one current screen and at most one
    // bounded in-flight frame per viewer, independent of output rate.
    this.#latest = { ...screen, scrollback: [...screen.scrollback] };
    this.#sequence++;
    for (const view of this.#views.values()) view.pending = true;
    if (this.#flushTimer === undefined) this.#flushTimer = this.#timing.set(() => { this.#flushTimer = undefined; this.flush(); }, this.options.batchMs ?? 4);
  }
  flush(): void { for (const view of this.#views.values()) this.#pump(view); }
  #watch(view: View): void {
    if (view.timer !== undefined) return;
    view.deadline ??= this.#timing.now() + (this.options.stalledMs ?? 5000);
    view.timer = this.#timing.set(() => {
      view.timer = undefined;
      if (!this.#current(view)) { this.detach(view); return; }
      if (this.#timing.now() >= view.deadline!) { this.detach(view); view.socket.close?.(); return; }
      this.#pump(view);
      if (view.inflight || view.awaiting || view.pending || this.#exit !== undefined && !view.exitSent) this.#watch(view);
    }, Math.min(25, this.options.stalledMs ?? 5000));
  }
  #send(view: View, payload: object, done: () => void): void {
    view.inflight = true;
    this.#watch(view);
    try {
      view.socket.send(JSON.stringify(payload), (error) => {
        if (!this.#current(view)) return;
        if (error) { this.detach(view); view.socket.close?.(); return; }
        view.inflight = false;
        if (!view.awaiting) {
          view.deadline = undefined;
          if (view.timer !== undefined) this.#timing.clear(view.timer);
          view.timer = undefined;
        }
        done(); this.#pump(view);
      });
    } catch { this.detach(view); view.socket.close?.(); }
  }
  #pump(view: View): void {
    if (!this.#current(view) || view.inflight || view.awaiting) return;
    if (view.socket.bufferedAmount >= (this.options.highWaterBytes ?? 256 * 1024)) { this.#watch(view); return; }
    if (view.pending && this.#latest) {
      const cursor = this.cursor;
      const previous = view.resetRequested ? view.initial : view.sent ?? view.initial;
      const gap = !!previous && (previous.generation !== cursor.generation || previous.sequence !== cursor.sequence - 1 && previous.sequence !== cursor.sequence);
      const frame: ScreenFrame = { version: 1, type: "terminal-screen", ...cursor, reset: view.resetRequested || !view.sent || gap, gap, screen: this.#latest };
      view.pending = false;
      view.resetRequested = false;
      view.awaiting = cursor;
      this.#trace(gap ? "gap" : "screen-send", `${cursor.generation}:${cursor.sequence}`);
      this.#send(view, frame, () => { view.sent = cursor; });
      return;
    }
    if (this.#exit !== undefined && !view.exitSent) {
      view.exitSent = true;
      this.#send(view, { version: 1, type: "terminal-exit", ...this.cursor, code: this.#exit }, () => {});
    }
  }
  exit(code: number): void { this.#exit = code; this.flush(); }
  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    if (this.#flushTimer !== undefined) this.#timing.clear(this.#flushTimer);
    this.#flushTimer = undefined;
    for (const view of [...this.#views.values()]) this.#remove(view, false);
  }
}
