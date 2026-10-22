// Predictive local echo for `rudder cloud` (the mosh trick).
//
// A keystroke to the cloud dashboard takes a round trip (~100ms from the US
// west coast to the machine in Virginia) before its echo is drawn. The
// dashboard always parks the real terminal cursor on the input caret (the task
// box, or the agent's composer), so the client can draw a typed character at
// the caret immediately and let the cloud's frame confirm it.
//
// The client keeps an exact model of the screen the cloud has drawn (xterm's
// headless terminal, fed only the cloud's bytes). Predictions are written to
// the real terminal BETWEEN cloud frames (never inside a synchronized-output
// frame or a half-received escape sequence), so they never tear a frame. A
// prediction is confirmed when the model shows the same character in that
// cell; one the cloud contradicts, or that is not echoed in time, is rolled
// back by repainting its cell from the model. Predictions only start once the
// measured round trip is worth hiding, and only on a visible caret over a
// blank cell (typing at the end of the input, the common case).

import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { Terminal } = require("@xterm/headless") as typeof import("@xterm/headless");

type HeadlessTerminal = InstanceType<typeof Terminal>;
type IBufferCell = import("@xterm/headless").IBufferCell;

type Prediction = {
  row: number;
  col: number;
  ch: string;
  /** SGR the character is drawn with: the look of the text it continues. */
  sgr: string;
  at: number;
  shown: boolean;
};

export type PredictorOptions = {
  /** Predict only once the smoothed round trip is at least this (ms). */
  thresholdMs?: number;
  /** A prediction not confirmed within this long is rolled back (ms). */
  timeoutMs?: number;
  /** After a rollback, stop predicting for this long (ms). */
  cooldownMs?: number;
  /** false keeps the screen model (for repaints) without predicting. */
  predict?: boolean;
  now?: () => number;
};

const SYNC_BEGIN = "\x1b[?2026h";
const SYNC_END = "\x1b[?2026l";
const CURSOR_SHOW = "\x1b[?25h";
const CURSOR_HIDE = "\x1b[?25l";

export class EchoPredictor {
  private readonly model: HeadlessTerminal;
  private readonly out: (text: string) => void;
  private readonly thresholdMs: number;
  private readonly timeoutMs: number;
  private readonly cooldownMs: number;
  private readonly predict: boolean;
  private readonly now: () => number;
  private predictions: Prediction[] = [];
  // The grey hint an empty input shows under its caret ("Ask anything…"),
  // hidden while the first characters typed over it wait for their echo.
  private placeholder: { row: number; start: number; end: number; sgr: string } | null = null;
  private pendingWrites = 0;
  private inSyncFrame = false;
  private midEscape = false;
  private cursorVisible = true;
  private sawFrame = false;
  // Where the caret was when the last complete cloud frame ended. Mid-frame the
  // model's cursor is wherever the cloud happens to be drawing.
  private caret: { row: number; col: number } | null = null;
  // A key we cannot predict (Enter, arrows, a mouse report) changes the screen
  // in ways the model has not seen yet: hold until the cloud catches up.
  private hold = false;
  private holdSince = 0;
  private suspendedUntil = 0;
  private srtt: number | null = null;
  private timer: NodeJS.Timeout | null = null;
  rollbacks = 0;
  confirmed = 0;

  constructor(cols: number, rows: number, out: (text: string) => void, options: PredictorOptions = {}) {
    this.model = new Terminal({ cols, rows, allowProposedApi: true, scrollback: 0 });
    this.out = out;
    this.thresholdMs = options.thresholdMs ?? 30;
    this.timeoutMs = options.timeoutMs ?? 1200;
    this.cooldownMs = options.cooldownMs ?? 1000;
    this.predict = options.predict ?? true;
    this.now = options.now ?? (() => Date.now());
  }

  get cols(): number {
    return this.model.cols;
  }

  /**
   * Redraw cells [col, col + width) of a row exactly as the cloud last drew
   * them, leaving the cursor where it was. Used to take an overlay (the
   * reconnecting badge) back off the screen without asking the cloud.
   */
  paintRegion(row: number, col: number, width: number): string {
    const line = this.model.buffer.active.getLine(row);
    let start = Math.max(0, col);
    // Starting on the right half of a wide character would split it.
    if (start > 0 && line?.getCell(start)?.getWidth() === 0) {
      start -= 1;
    }
    const end = Math.min(this.model.cols, col + width);
    let text = `\x1b7\x1b[${row + 1};${start + 1}H`;
    for (let c = start; c < end; c += 1) {
      if (line?.getCell(c)?.getWidth() === 0) {
        continue;
      }
      text += this.cellPaint(row, c);
    }
    return `${text}\x1b[0m\x1b8`;
  }

  /** Feed a measured round trip (ms); predictions start above the threshold. */
  noteRtt(ms: number): void {
    if (!Number.isFinite(ms) || ms < 0) {
      return;
    }
    this.srtt = this.srtt === null ? ms : this.srtt * 0.8 + ms * 0.2;
  }

  get active(): boolean {
    return this.srtt !== null && this.srtt >= this.thresholdMs;
  }

  resize(cols: number, rows: number): void {
    this.rollback();
    this.model.resize(Math.max(1, cols), Math.max(1, rows));
  }

  dispose(): void {
    if (process.env.RUDDER_PREDICT_DEBUG === "1") {
      process.stderr.write(`\npredict: srtt=${this.srtt?.toFixed(0) ?? "?"}ms confirmed=${this.confirmed} rollbacks=${this.rollbacks}\n`);
    }
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = null;
    }
    this.model.dispose();
  }

  /** Bytes the cloud drew. Call after writing them to the real terminal. */
  onServerData(data: Buffer | string): void {
    const text = typeof data === "string" ? data : data.toString("utf8");
    const lastBegin = text.lastIndexOf(SYNC_BEGIN);
    const lastEnd = text.lastIndexOf(SYNC_END);
    if (lastBegin !== -1 || lastEnd !== -1) {
      this.inSyncFrame = lastBegin > lastEnd;
      if (lastEnd > lastBegin) {
        this.sawFrame = true;
      }
    }
    const lastShow = text.lastIndexOf(CURSOR_SHOW);
    const lastHide = text.lastIndexOf(CURSOR_HIDE);
    if (lastShow !== -1 || lastHide !== -1) {
      this.cursorVisible = lastShow > lastHide;
    }
    this.midEscape = endsMidEscape(text);
    this.pendingWrites += 1;
    this.model.write(text, () => {
      this.pendingWrites -= 1;
      if (this.pendingWrites === 0) {
        if (this.atBoundary()) {
          const buffer = this.model.buffer.active;
          this.caret = { row: buffer.cursorY, col: buffer.cursorX };
        }
        this.reconcile();
      }
    });
  }

  /** Keystrokes the user typed (already sent to the cloud). */
  onInput(data: Buffer | string): void {
    const bytes = typeof data === "string" ? Buffer.from(data, "utf8") : data;
    if (!this.predict || !this.active) {
      return;
    }
    for (const byte of bytes) {
      if (byte >= 0x20 && byte <= 0x7e) {
        if (!this.predictChar(String.fromCharCode(byte))) {
          this.setHold();
          return;
        }
      } else if (byte === 0x7f || byte === 0x08) {
        if (!this.predictBackspace()) {
          this.setHold();
          return;
        }
      } else {
        this.setHold();
        return;
      }
    }
  }

  private atBoundary(): boolean {
    return this.pendingWrites === 0 && !this.inSyncFrame && !this.midEscape;
  }

  private setHold(): void {
    this.hold = true;
    this.holdSince = this.now();
  }

  private predictChar(ch: string): boolean {
    if (this.hold) {
      // The key that set the hold has had time to come back only after about
      // one round trip, and the model must have caught up with it.
      const settle = (this.srtt ?? 0) * 1.5 + 50;
      if (this.pendingWrites !== 0 || this.now() - this.holdSince < settle) {
        return false;
      }
      this.hold = false;
    }
    if (!this.sawFrame || !this.cursorVisible || this.now() < this.suspendedUntil) {
      return false;
    }
    let row: number;
    let col: number;
    const last = this.predictions[this.predictions.length - 1];
    if (last) {
      row = last.row;
      col = last.col + 1;
    } else {
      if (!this.caret) {
        return false;
      }
      row = this.caret.row;
      col = this.caret.col;
    }
    // Leave the last column alone (wrapping is the cloud's call), and only
    // type over blank cells (the end of the input) or an empty input's hint.
    if (col >= this.model.cols - 1) {
      return false;
    }
    if (!last && !this.cellBlank(row, col)) {
      this.placeholder = this.placeholderAt(row, col);
      if (!this.placeholder) {
        return false;
      }
    } else if (!this.typeable(row, col)) {
      return false;
    }
    const sgr = last ? last.sgr : this.placeholder ? this.placeholder.sgr : this.typingSgr(row, col);
    const prediction: Prediction = { row, col, ch, sgr, at: this.now(), shown: false };
    this.predictions.push(prediction);
    this.ensureTimer();
    if (this.atBoundary()) {
      this.show();
    }
    return true;
  }

  private predictBackspace(): boolean {
    const last = this.predictions.pop();
    if (!last) {
      // Deleting text the cloud already drew: let the cloud do it.
      return false;
    }
    if (last.shown && this.atBoundary()) {
      // Deleting the only character typed over a hint brings the hint back.
      const hint = this.predictions.length === 0 ? this.placeholder : null;
      const repaint = hint ? this.paintCells(hint.row, hint.start, hint.end) : this.cellPaint(last.row, last.col);
      const from = hint ? hint.start : last.col;
      this.out(`\x1b[${last.row + 1};${from + 1}H${repaint}\x1b[0m\x1b[${last.row + 1};${last.col + 1}H`);
    }
    if (this.predictions.length === 0) {
      this.placeholder = null;
    }
    return true;
  }

  /** Draw every pending prediction and park the cursor after the last one. */
  private show(): void {
    if (this.predictions.length === 0) {
      return;
    }
    let text = "";
    const hint = this.placeholder;
    if (hint) {
      // The echo will clear the hint: clear it now, in the input's background.
      text += `\x1b[${hint.row + 1};${hint.start + 1}H${hint.sgr}${" ".repeat(hint.end - hint.start)}`;
    }
    for (const prediction of this.predictions) {
      text += `\x1b[${prediction.row + 1};${prediction.col + 1}H${prediction.sgr}${prediction.ch}`;
      prediction.shown = true;
    }
    this.out(`${text}\x1b[0m`);
  }

  private reconcile(): void {
    if (this.pendingWrites !== 0) {
      return;
    }
    if (this.predictions.length === 0) {
      return;
    }
    // Confirm in order: the cloud echoes keystrokes in the order they were sent.
    while (this.predictions.length > 0) {
      const first = this.predictions[0];
      if (this.cellChar(first.row, first.col) !== first.ch) {
        break;
      }
      this.predictions.shift();
      this.confirmed += 1;
      // The cloud echoed over the hint, so it has cleared it itself.
      this.placeholder = null;
    }
    if (this.predictions.length === 0) {
      this.placeholder = null;
      return;
    }
    const first = this.predictions[0];
    if (!this.atBoundary() || !this.caret) {
      // Mid-frame the cursor is wherever the cloud is drawing: judge at the end.
      return;
    }
    // The cloud moved its caret past a prediction without drawing it, or onto
    // another row: the key did something other than type.
    const contradicted = this.caret.row !== first.row
      || this.caret.col > first.col
      || !this.typeable(first.row, first.col);
    if (contradicted || this.now() - first.at > this.timeoutMs) {
      this.rollback();
      return;
    }
    if (this.atBoundary()) {
      // The cloud's frame may have repainted these cells: draw them again.
      this.show();
    }
  }

  /** Repaint every predicted cell from the model and put the cursor back. */
  private rollback(): void {
    if (this.predictions.length === 0) {
      return;
    }
    const shown = this.predictions.filter((prediction) => prediction.shown);
    const hint = this.placeholder;
    this.predictions = [];
    this.placeholder = null;
    this.rollbacks += 1;
    this.suspendedUntil = this.now() + this.cooldownMs;
    if (shown.length === 0 || !this.atBoundary()) {
      return;
    }
    let text = hint ? `\x1b[${hint.row + 1};${hint.start + 1}H${this.paintCells(hint.row, hint.start, hint.end)}` : "";
    for (const prediction of shown) {
      text += `\x1b[${prediction.row + 1};${prediction.col + 1}H${this.cellPaint(prediction.row, prediction.col)}`;
    }
    const buffer = this.model.buffer.active;
    text += `\x1b[0m\x1b[${buffer.cursorY + 1};${buffer.cursorX + 1}H`;
    this.out(text);
  }

  private ensureTimer(): void {
    if (this.timer) {
      return;
    }
    this.timer = setInterval(() => {
      if (this.predictions.length === 0) {
        if (this.timer) {
          clearInterval(this.timer);
          this.timer = null;
        }
        return;
      }
      if (this.pendingWrites === 0 && this.now() - this.predictions[0].at > this.timeoutMs) {
        this.rollback();
      }
    }, 100);
    this.timer.unref?.();
  }

  private cellChar(row: number, col: number): string {
    return this.model.buffer.active.getLine(row)?.getCell(col)?.getChars() ?? "";
  }

  /** A cell a prediction may be drawn over: blank, or the hint it hides. */
  private typeable(row: number, col: number): boolean {
    const hint = this.placeholder;
    if (hint && row === hint.row && col >= hint.start && col < hint.end) {
      return true;
    }
    return this.cellBlank(row, col);
  }

  /**
   * The hint an empty input draws under its caret: text in grey or dim,
   * starting at the caret with nothing typed before it. Its extent is the run
   * of cells in that look; typed text takes the look of the input after it.
   */
  private placeholderAt(row: number, col: number): { row: number; start: number; end: number; sgr: string } | null {
    const line = this.model.buffer.active.getLine(row);
    const cell = line?.getCell(col);
    if (!line || !cell || !isHintLook(cell) || (col > 0 && !this.cellBlank(row, col - 1))) {
      return null;
    }
    const background = backgroundCodes(cell);
    let end = col;
    while (end < this.model.cols - 1) {
      const next = line.getCell(end);
      if (!next || !isHintLook(next) || !sameCodes(backgroundCodes(next), background)) {
        break;
      }
      end += 1;
    }
    const after = line.getCell(end);
    const text = after && sameCodes(backgroundCodes(after), background) ? foregroundCodes(after) : ["0"];
    return { row, start: col, end, sgr: `\x1b[${[...text, ...background].join(";")}m` };
  }

  private paintCells(row: number, start: number, end: number): string {
    let text = "";
    for (let col = start; col < end; col += 1) {
      text += this.cellPaint(row, col);
    }
    return text;
  }

  private cellBlank(row: number, col: number): boolean {
    const chars = this.cellChar(row, col);
    return chars === "" || chars === " ";
  }

  /** SGR + character that redraws one cell exactly as the model has it. */
  private cellPaint(row: number, col: number): string {
    const cell = this.model.buffer.active.getLine(row)?.getCell(col);
    if (!cell) {
      return "\x1b[0m ";
    }
    return `\x1b[${[...foregroundCodes(cell), ...backgroundCodes(cell)].join(";")}m${cell.getChars() || " "}`;
  }

  /**
   * How a character typed at this cell will look once the cloud echoes it.
   * An input box with its own background (opencode's is grey) must keep it, or
   * every predicted character flashes in a black box until the echo lands. The
   * colors come from the text just before the caret when there is some on the
   * same background, otherwise from the blank cell itself.
   */
  private typingSgr(row: number, col: number): string {
    const line = this.model.buffer.active.getLine(row);
    const cell = line?.getCell(col);
    if (!cell) {
      return "\x1b[0m";
    }
    const background = backgroundCodes(cell);
    const before = col > 0 ? line?.getCell(col - 1) : undefined;
    const text = before && !this.cellBlank(row, col - 1) && sameCodes(backgroundCodes(before), background)
      ? before
      : cell;
    return `\x1b[${[...foregroundCodes(text), ...background].join(";")}m`;
  }
}

function foregroundCodes(cell: IBufferCell): string[] {
  const codes: string[] = ["0"];
  if (cell.isBold()) codes.push("1");
  if (cell.isDim()) codes.push("2");
  if (cell.isItalic()) codes.push("3");
  if (cell.isUnderline()) codes.push("4");
  if (cell.isInverse()) codes.push("7");
  if (cell.isFgRGB()) {
    const c = cell.getFgColor();
    codes.push(`38;2;${(c >> 16) & 255};${(c >> 8) & 255};${c & 255}`);
  } else if (cell.isFgPalette()) {
    codes.push(`38;5;${cell.getFgColor()}`);
  }
  return codes;
}

/** Grey or dim text: how inputs draw their hint ("Ask anything…", "Try …"). */
function isHintLook(cell: IBufferCell): boolean {
  if (cell.isDim()) {
    return true;
  }
  if (cell.isFgRGB()) {
    const c = cell.getFgColor();
    const r = (c >> 16) & 255;
    const g = (c >> 8) & 255;
    const b = c & 255;
    return Math.max(r, g, b) - Math.min(r, g, b) <= 16 && Math.max(r, g, b) >= 64 && Math.max(r, g, b) <= 170;
  }
  if (cell.isFgPalette()) {
    const n = cell.getFgColor();
    return n === 8 || (n >= 240 && n <= 248);
  }
  return false;
}

function backgroundCodes(cell: IBufferCell): string[] {
  if (cell.isBgRGB()) {
    const c = cell.getBgColor();
    return [`48;2;${(c >> 16) & 255};${(c >> 8) & 255};${c & 255}`];
  }
  if (cell.isBgPalette()) {
    return [`48;5;${cell.getBgColor()}`];
  }
  return [];
}

function sameCodes(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((code, index) => code === b[index]);
}

/** Whether a chunk stops inside an escape sequence (the next chunk finishes it). */
export function endsMidEscape(text: string): boolean {
  const tail = text.slice(-256);
  const esc = tail.lastIndexOf("\x1b");
  if (esc === -1) {
    return false;
  }
  const rest = tail.slice(esc);
  if (rest.length === 1) {
    return true;
  }
  if (rest[1] === "[") {
    return !/^\x1b\[[0-?]*[ -/]*[@-~]/.test(rest);
  }
  if (rest[1] === "]" || rest[1] === "P" || rest[1] === "_") {
    return !/(\x07|\x1b\\)/.test(rest.slice(2));
  }
  return false;
}
