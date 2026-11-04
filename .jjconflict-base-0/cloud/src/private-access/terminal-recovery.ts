/** Browser-side adapter for the opt-in terminal-screen v1 protocol. A screen
 * replaces rendered state including scrollback, geometry, cursor and modes;
 * never append it to an arbitrary suffix of the former socket's byte stream. */
export interface Screen {
  format: "vt100-ansi-v1";
  columns: number;
  rows: number;
  screen: string;
  scrollback: string[];
  /** Canonical text projection for read-only accessibility views. */
  text?: string;
}
export interface Cursor { generation: string; sequence: number }
export interface ScreenMessage extends Cursor {
  version: 1;
  type: "terminal-screen";
  reset: boolean;
  gap: boolean;
  screen: Screen;
}
type TracePhase = "input-capture" | "input-send" | "predictive-paint" | "authoritative-paint" | "rollback" | "gap";
export interface PaintTrace { correlationId: string; phase: TracePhase; localMs: number; clock: "process-monotonic" }
export class TerminalRecovery {
  #connection = 0;
  #owner = "";
  #connected = false;
  #controller = false;
  #decoder = new TextDecoder("utf-8");
  #cursor?: Cursor;
  #needsReset = true;
  #painted?: Cursor;
  #pendingExit?: { connection: number; sequence: number; code: number };
  constructor(readonly adapter: {
    restore(screen: Screen, paint: { isCurrent(): boolean; painted(): void }): void;
    writeLegacy?(text: string): void;
    requestSnapshot(cursor?: Cursor): void;
    sendInput(bytes: Uint8Array, generation: string, correlationId: string): boolean;
    acknowledge(cursor: Cursor): void;
    exit?(code: number): void;
    trace?(event: PaintTrace): void;
    now?: () => number;
    maxScreenBytes?: number;
    maxInputBytes?: number;
  }) {}
  #trace(phase: TracePhase, correlationId: string): void {
    this.adapter.trace?.({ phase, correlationId, localMs: this.adapter.now?.() ?? performance.now(), clock: "process-monotonic" });
  }
  get cursor(): Cursor | undefined { return this.#cursor && { ...this.#cursor }; }
  /** Capture this return value in EVERY message/close/error callback. Owner
   * identity comes from the authenticated attach handshake, never a frame. */
  connect(owner: string, controller: boolean): number {
    this.#connection++; this.#owner = owner;
    this.#connected = true; this.#controller = controller;
    this.#decoder = new TextDecoder("utf-8"); this.#needsReset = true;
    this.#painted = undefined; this.#pendingExit = undefined;
    return this.#connection;
  }
  disconnect(connection: number): void {
    if (connection !== this.#connection) return;
    this.#connected = false; this.#controller = false;
    this.#decoder = new TextDecoder("utf-8"); this.#needsReset = true;
    this.#painted = undefined; this.#pendingExit = undefined;
  }
  setController(connection: number, controller: boolean): void {
    if (connection !== this.#connection || !this.#connected) return;
    this.#controller = controller;
  }
  #current(connection: number): boolean { return connection === this.#connection && this.#connected; }
  accept(connection: number, frame: ScreenMessage): boolean {
    if (!this.#current(connection) || !frame || typeof frame !== "object" || frame.version !== 1 || frame.type !== "terminal-screen" || frame.generation !== this.#owner
      || !Number.isSafeInteger(frame.sequence) || frame.sequence < 0) return false;
    if (this.#cursor?.generation === frame.generation && frame.sequence < this.#cursor.sequence) return false;
    const mismatch = !this.#cursor || this.#cursor.generation !== frame.generation || frame.sequence !== this.#cursor.sequence + 1;
    if ((this.#needsReset || mismatch || frame.gap) && !frame.reset) {
      this.#needsReset = true; this.#trace("gap", `${frame.generation}:${frame.sequence}`);
      this.adapter.requestSnapshot(this.cursor); return false;
    }
    const screen = frame.screen;
    if (!screen || screen.format !== "vt100-ansi-v1" || typeof screen.screen !== "string" || !Array.isArray(screen.scrollback)
      || !Number.isInteger(screen.columns) || !Number.isInteger(screen.rows) || screen.columns < 1 || screen.rows < 1
      || screen.columns > 1000 || screen.rows > 1000 || screen.scrollback.length > 10000) return false;
    const limit = this.adapter.maxScreenBytes ?? 2 * 1024 * 1024;
    // UTF-16 code units are a conservative precheck before UTF-8 allocation.
    if (screen.text !== undefined && typeof screen.text !== "string") return false;
    let units = screen.screen.length + (screen.text?.length ?? 0);
    for (const line of screen.scrollback) { if (typeof line !== "string") return false; units += line.length; if (units > limit) return false; }
    if (units > limit) return false;
    let bytes = new TextEncoder().encode(screen.screen).byteLength + new TextEncoder().encode(screen.text ?? "").byteLength;
    for (const line of screen.scrollback) { bytes += new TextEncoder().encode(line).byteLength; if (bytes > limit) return false; }
    if (bytes > limit) return false;
    if (frame.reset) this.#decoder = new TextDecoder("utf-8");
    this.#cursor = { generation: frame.generation, sequence: frame.sequence };
    this.#needsReset = false;
    const isCurrent = () => this.#current(connection) && this.#owner === frame.generation
      && this.#cursor?.sequence === frame.sequence;
    this.adapter.restore(screen, { isCurrent, painted: () => {
      if (!isCurrent() || this.#painted && this.#painted.sequence > frame.sequence) return;
      this.#painted = { generation: frame.generation, sequence: frame.sequence };
      this.#trace("authoritative-paint", `${frame.generation}:${frame.sequence}`);
      this.adapter.acknowledge({ generation: frame.generation, sequence: frame.sequence });
      this.#finishExit();
    } });
    return true;
  }
  /** Optional legacy streaming path. It provides no screen recovery guarantee;
   * keep it disabled on sequenced screen attachments. Decoder is socket-local. */
  legacyOutput(connection: number, bytes: Uint8Array): boolean {
    if (!this.#current(connection)) return false;
    const text = this.#decoder.decode(bytes, { stream: true });
    if (text) this.adapter.writeLegacy?.(text);
    return true;
  }
  input(connection: number, bytes: Uint8Array, correlationId: string): boolean {
    if (!this.#current(connection) || !this.#controller || !this.#owner || !correlationId || correlationId.length > 128
      || bytes.byteLength > (this.adapter.maxInputBytes ?? 64 * 1024)) return false;
    this.#trace("input-capture", correlationId);
    // No queue/retry: a failed send has an unknown effect. Privileged actions
    // use the durable operation/receipt path and must not use this byte API.
    if (!this.adapter.sendInput(bytes, this.#owner, correlationId)) return false;
    this.#trace("input-send", correlationId); return true;
  }
  predictionPainted(correlationId: string): void { this.#trace("predictive-paint", correlationId); }
  predictionRolledBack(correlationId: string): void { this.#trace("rollback", correlationId); }
  acceptExit(connection: number, frame: Cursor & { version: 1; type: "terminal-exit"; code: number }): boolean {
    if (!this.#current(connection) || !frame || typeof frame !== "object" || frame.version !== 1 || frame.type !== "terminal-exit" || this.#needsReset
      || frame.generation !== this.#cursor?.generation || frame.sequence !== this.#cursor.sequence) return false;
    this.#pendingExit = { connection, sequence: frame.sequence, code: frame.code };
    this.#finishExit(); return true;
  }
  #finishExit(): void {
    const exit = this.#pendingExit;
    if (!exit || !this.#current(exit.connection) || this.#painted?.sequence !== exit.sequence || this.#painted.generation !== this.#owner) return;
    this.adapter.exit?.(exit.code); this.disconnect(exit.connection);
  }
}

/** Percentiles use only local durations from the same process/clock. Correlation
 * ids join events; they do not make two monotonic clocks comparable. */
export function terminalLatencyBounds(samples: readonly number[]): { count: number; p95: number; p99: number; max: number } | null {
  if (!samples.length) return null;
  if (samples.some((value) => !Number.isFinite(value) || value < 0)) throw new Error("latency samples must share a local monotonic clock");
  const sorted = [...samples].sort((a, b) => a - b);
  const percentile = (p: number) => sorted[Math.max(0, Math.ceil(sorted.length * p) - 1)];
  return { count: sorted.length, p95: percentile(.95), p99: percentile(.99), max: sorted[sorted.length - 1] };
}
