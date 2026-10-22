// One `rudder cloud` terminal across reconnects (the mosh shape).
//
// A dropped link used to hand the terminal back: raw mode off, out of the
// alternate screen, "Lost the connection..." under the shell prompt, a wait of
// up to 15s, then the dashboard repainted into the main buffer on top of the
// scrollback. Laptops sleep and Wi-Fi changes, so that happened all the time.
//
// The session keeps the terminal through the gap instead: the cloud's last
// picture stays up with a small badge in the corner, what you type is kept and
// sent once the link is back, Ctrl+C twice leaves, and the reconnect paints
// over the old picture in place.

import { randomUUID } from "node:crypto";
import { EchoPredictor } from "./predict.js";
import { THEME_QUERY, THEME_REPORTS_OFF, THEME_REPORTS_ON, TerminalThemeWatcher, type TerminalColors } from "./terminal-theme.js";

export type SessionInput = {
  on(event: "data", listener: (chunk: Buffer | string) => void): unknown;
  off(event: "data", listener: (chunk: Buffer | string) => void): unknown;
  resume(): unknown;
  pause(): unknown;
  setRawMode?(mode: boolean): unknown;
};

export type SessionOutput = {
  columns?: number;
  rows?: number;
  write(text: string): unknown;
  on(event: "resize", listener: () => void): unknown;
  off(event: "resize", listener: () => void): unknown;
};

export type SessionOptions = {
  interactive: boolean;
  /** false keeps the screen model but never draws predicted keystrokes. */
  predict?: boolean;
  /** Where a non-interactive session reports a drop (stderr). */
  log?: (text: string) => void;
  now?: () => number;
};

// The remote dashboard asks for DISAMBIGUATE_ESCAPE_CODES | REPORT_ALTERNATE_KEYS
// once, long before any attach; see runAttach in cloud.ts.
export const KITTY_KEYBOARD_PUSH = "\x1b[>5u";
export const KITTY_KEYBOARD_POP = "\x1b[<u";
const ALT_SCREEN_ON = "\x1b[?1049h";
const ALT_SCREEN_OFF = "\x1b[?1049l\x1b[?25h";
const MOUSE_ON = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF = "\x1b[?1006l\x1b[?1015l\x1b[?1005l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
// Leaving must give the shell a plain keyboard whatever the cloud stream did
// meanwhile: a remote dashboard that restarted mid-attach pushed its keyboard
// flags again, and one pop left Ghostty reporting every key (Cmd+C reached zsh
// as `;9;99~`, and with the mouse still reported nothing could be selected).
// Pop the whole kitty stack, zero the flags, modifyOtherKeys and focus off.
export const KEYBOARD_RESET = "\x1b[<99u\x1b[=0;1u\x1b[>4m\x1b[?1004l";
// Bracketed paste on the LOCAL terminal. The remote dashboard enables it once at
// startup, long gone from the bounded replay by the time a terminal attaches, so
// without this a paste arrives as raw keystrokes: Ghostty/iTerm ask "paste
// contains newlines?", each newline lands as Enter, and the dashboard never sees
// a paste to fold into a "[Pasted #N ...]" chip. (Rescued from an agent's
// unpushed change on the cloud machine's old viraatdas/rudder clone.)
const BRACKETED_PASTE_ON = "\x1b[?2004h";
const BRACKETED_PASTE_OFF = "\x1b[?2004l";
const SYNC_BEGIN = "\x1b[?2026h";
const SYNC_END = "\x1b[?2026l";
// CAN aborts an escape sequence the cloud's last bytes left open, so an
// overlay is never read as the tail of one. Outside a sequence it is ignored.
const CANCEL = "\x18";
const LEAVE_WINDOW_MS = 3_000;
const QUEUE_LIMIT_BYTES = 64 * 1024;

export function containsCtrlC(data: Buffer): boolean {
  return data.includes(0x03) || /\x1b\[99;5(?::[123])?u/.test(data.toString("latin1"));
}

export class AttachSession {
  /** Sent as `?client=`: the relay lets this terminal take its own place back. */
  readonly clientId = randomUUID();
  readonly interactive: boolean;
  /** The cloud's screen as last drawn (and predictive echo on top of it). */
  readonly predictor: EchoPredictor | null;
  /** This terminal's colors, for the cloud's agents to theme themselves by. */
  readonly theme: TerminalThemeWatcher | null;
  /** Connections that reached `open`. */
  connections = 0;
  /** The cloud has drawn on this terminal: a reconnect resumes that picture. */
  drewFrame = false;
  hasLeft = false;

  private readonly input: SessionInput;
  private readonly output: SessionOutput;
  private readonly label: string;
  private readonly log: (text: string) => void;
  private readonly now: () => number;
  private held = false;
  private released = false;
  private inGap = false;
  private status: string | null = null;
  private badge: { col: number; width: number } | null = null;
  private queued: Buffer[] = [];
  private queuedBytes = 0;
  private lastCtrlC = 0;
  private leaveListeners = new Set<() => void>();
  private themeListeners = new Set<(colors: TerminalColors) => void>();

  constructor(label: string, input: SessionInput, output: SessionOutput, options: SessionOptions) {
    this.label = label;
    this.input = input;
    this.output = output;
    this.interactive = options.interactive;
    this.log = options.log ?? ((text) => process.stderr.write(text));
    this.now = options.now ?? (() => Date.now());
    this.predictor = options.interactive
      ? new EchoPredictor(output.columns ?? 120, output.rows ?? 32, (text) => output.write(text), {
        predict: options.predict ?? true,
      })
      : null;
    this.theme = options.interactive
      ? new TerminalThemeWatcher(
        () => this.output.write(THEME_QUERY),
        (colors) => {
          for (const listener of [...this.themeListeners]) {
            listener(colors);
          }
        },
      )
      : null;
  }

  /** Called with this terminal's colors whenever they are learned or change. */
  onTheme(listener: (colors: TerminalColors) => void): () => void {
    this.themeListeners.add(listener);
    return () => this.themeListeners.delete(listener);
  }

  /** Input with the terminal's color replies taken out (they are not keys). */
  filterInput(data: Buffer): Buffer {
    return this.theme ? this.theme.filter(data) : data;
  }

  /** Take the terminal: alternate screen, raw keys, mouse and keyboard flags. */
  hold(): void {
    if (!this.interactive || this.held || this.released) {
      return;
    }
    this.held = true;
    this.output.write(ALT_SCREEN_ON);
    try {
      this.input.setRawMode?.(true);
    } catch {
      // ignore
    }
    // The remote dashboard's own enable sequences arrive as output bytes,
    // which the local terminal never treats as mode switches.
    this.output.write(MOUSE_ON);
    this.output.write(KITTY_KEYBOARD_PUSH);
    this.output.write(BRACKETED_PASTE_ON);
    // Light or dark: answered on stdin, and again whenever the appearance flips.
    this.output.write(THEME_REPORTS_ON);
    this.output.write(THEME_QUERY);
  }

  /** Give the terminal back exactly once, whatever state it is in. */
  release(): void {
    if (this.released) {
      return;
    }
    this.released = true;
    this.stopGapInput();
    if (this.held) {
      this.output.write(KITTY_KEYBOARD_POP);
      this.output.write(KEYBOARD_RESET);
      this.output.write(BRACKETED_PASTE_OFF);
      this.output.write(THEME_REPORTS_OFF);
      this.output.write(MOUSE_OFF);
      // Release the tab title so the shell prompt can rewrite it.
      this.output.write("\x1b]0;\x07");
      try {
        this.input.setRawMode?.(false);
      } catch {
        // ignore
      }
      this.output.write(ALT_SCREEN_OFF);
    }
    this.predictor?.dispose();
    this.input.pause();
  }

  /**
   * The link dropped and a reconnect follows. The screen stays as it was, a
   * badge says what is happening, and keys are kept for the next connection.
   */
  enterGap(): void {
    if (this.released || this.inGap) {
      return;
    }
    this.inGap = true;
    if (!this.interactive) {
      return;
    }
    this.closeOpenSequence();
    this.setTitle(`Rudder cloud: ${this.label} · reconnecting`);
    this.showStatus("Reconnecting · your agents keep running");
    this.input.on("data", this.onGapInput);
    this.output.on("resize", this.onGapResize);
    this.input.resume();
  }

  /** A retry is about to start; the badge counts them once it is slow. */
  noteAttempt(attempt: number): void {
    if (!this.interactive) {
      this.log(`\nLost the connection to Rudder Cloud (laptop slept or the network dropped; your agents kept running). Reconnecting${attempt > 1 ? ` (try ${attempt})` : ""}...\n`);
      return;
    }
    if (attempt >= 4) {
      this.showStatus(`Reconnecting (try ${attempt}) · Ctrl+C twice to leave`);
    }
  }

  /** A connection opened: it reads the keys from here on. */
  exitGap(): void {
    if (!this.inGap) {
      return;
    }
    this.inGap = false;
    this.stopGapInput();
    if (this.interactive && this.held) {
      this.setTitle(`Rudder cloud: ${this.label}`);
    }
  }

  /** Keys typed while the link was down, in order, for the new connection. */
  takeQueued(): Buffer | null {
    if (this.queuedBytes === 0) {
      return null;
    }
    const data = Buffer.concat(this.queued, this.queuedBytes);
    this.queued = [];
    this.queuedBytes = 0;
    return data;
  }

  /** Wait out a retry delay; true if the user left meanwhile. */
  wait(ms: number): Promise<boolean> {
    if (this.hasLeft) {
      return Promise.resolve(true);
    }
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        off();
        resolve(false);
      }, ms);
      const off = this.onLeave(() => {
        clearTimeout(timer);
        resolve(true);
      });
    });
  }

  onLeave(listener: () => void): () => void {
    this.leaveListeners.add(listener);
    return () => this.leaveListeners.delete(listener);
  }

  /**
   * Ctrl+C while nothing on the other end can take it: the first press says
   * how to leave, a second within a few seconds leaves. True when leaving.
   */
  ctrlCToLeave(): boolean {
    const now = this.now();
    if (now - this.lastCtrlC < LEAVE_WINDOW_MS) {
      this.leave();
      return true;
    }
    this.lastCtrlC = now;
    this.drawBadge("Press Ctrl+C again to leave");
    return false;
  }

  leave(): void {
    if (this.hasLeft) {
      return;
    }
    this.hasLeft = true;
    for (const listener of [...this.leaveListeners]) {
      listener();
    }
    this.leaveListeners.clear();
  }

  /** A lasting state worth a badge (worker away, read-only, reconnecting). */
  showStatus(text: string): void {
    this.status = text;
    this.drawBadge(text);
  }

  /** The state is over: take the badge off and repaint what it covered. */
  clearStatus(): void {
    this.status = null;
    this.eraseBadge();
  }

  get statusText(): string | null {
    return this.status;
  }

  /** The terminal changed size while connected: redraw the badge to fit. */
  noteResize(): void {
    this.badge = null;
    if (this.status) {
      this.drawBadge(this.status);
    }
  }

  private onGapInput = (chunk: Buffer | string): void => {
    const buffer = this.filterInput(typeof chunk === "string" ? Buffer.from(chunk, "utf8") : chunk);
    if (buffer.length === 0) {
      return;
    }
    if (containsCtrlC(buffer)) {
      // Never queued: an interrupt replayed into the dashboard after the
      // reconnect would land on whatever is running by then.
      this.ctrlCToLeave();
      return;
    }
    const room = QUEUE_LIMIT_BYTES - this.queuedBytes;
    if (room <= 0) {
      return;
    }
    const kept = buffer.length <= room ? buffer : buffer.subarray(0, room);
    this.queued.push(Buffer.from(kept));
    this.queuedBytes += kept.length;
  };

  private onGapResize = (): void => {
    this.predictor?.resize(this.output.columns ?? 120, this.output.rows ?? 32);
    this.noteResize();
  };

  private stopGapInput(): void {
    this.input.off("data", this.onGapInput);
    this.output.off("resize", this.onGapResize);
  }

  // The cloud's last bytes may have stopped inside an escape sequence or a
  // synchronized frame. End both on the terminal and in the model alike.
  private closeOpenSequence(): void {
    if (!this.held) {
      return;
    }
    const reset = `${CANCEL}${SYNC_END}`;
    this.output.write(reset);
    this.predictor?.onServerData(reset);
  }

  private drawBadge(text: string): void {
    if (!this.interactive || !this.held || this.released) {
      return;
    }
    const cols = this.output.columns ?? 80;
    const available = Math.max(4, cols - 2);
    let body = ` ⟳ ${text} `;
    if ([...body].length > available) {
      body = `${[...body].slice(0, available - 2).join("")}… `;
    }
    const width = [...body].length;
    const col = Math.max(0, cols - width - 1);
    let erase = "";
    if (this.badge && this.predictor) {
      erase = this.predictor.paintRegion(0, this.badge.col, this.badge.width);
    }
    this.badge = { col, width };
    this.output.write(
      `${CANCEL}${SYNC_BEGIN}${erase}\x1b7\x1b[1;${col + 1}H\x1b[0;30;43m${body}\x1b[0m\x1b8${SYNC_END}`,
    );
  }

  private eraseBadge(): void {
    const badge = this.badge;
    this.badge = null;
    if (!badge || !this.predictor || !this.held || this.released) {
      return;
    }
    this.output.write(`${CANCEL}${SYNC_BEGIN}${this.predictor.paintRegion(0, badge.col, badge.width)}${SYNC_END}`);
  }

  private setTitle(title: string): void {
    this.output.write(`\x1b]0;${title}\x07`);
  }
}
