// Escape sequences in the key stream a terminal sends: arrows and kitty keys,
// mouse reports, paste markers and the terminal's own replies.
//
// `rudder cloud` reads that stream in whatever pieces the OS hands it. Bytes of
// a sequence are never typed text: the predictive echo once drew the tail of a
// mouse click as "[<0;23;3m" at the caret. And a piece can end inside one: the
// dashboard's parser (crossterm) takes an ESC that ends a read for the Esc key
// and types the rest of the sequence into the composer.

/** Where a byte falls in the key stream. */
export type KeyByte = "text" | "start" | "inside" | "end";

const GROUND = 0;
const ESC = 1;
const CSI = 2;
const SS3 = 3;
// ESC followed by `]`, `P` or `_`: a terminal reply if the next byte says so.
const STRING_OPEN = 4;
const STRING = 5;
const STRING_ESC = 6;

// A reply longer than this lost its terminator; stop waiting for it.
const MAX_STRING_BYTES = 4096;

export class KeySequenceScanner {
  private state = GROUND;
  private opener = 0;
  private length = 0;

  /** The last byte left a sequence unfinished. */
  get open(): boolean {
    return this.state !== GROUND;
  }

  /**
   * A read ended here. An ESC left open on its own (or with a `]`, `P` or `_`)
   * is a key of its own, Esc or Alt+key, as the dashboard's parser reads it.
   */
  endRead(): void {
    if (this.state === ESC || this.state === STRING_OPEN) {
      this.state = GROUND;
    }
  }

  next(byte: number): KeyByte {
    switch (this.state) {
      case ESC:
        if (byte === 0x5b) {
          this.state = CSI;
          return "inside";
        }
        if (byte === 0x4f) {
          this.state = SS3;
          return "inside";
        }
        if (byte === 0x5d || byte === 0x50 || byte === 0x5f) {
          this.state = STRING_OPEN;
          this.opener = byte;
          return "inside";
        }
        if (byte === 0x1b) {
          // The first ESC was a key of its own.
          return "start";
        }
        this.state = GROUND;
        return "end";
      case CSI:
        if (byte >= 0x20 && byte <= 0x3f) {
          return "inside";
        }
        if (byte >= 0x40 && byte <= 0x7e) {
          this.state = GROUND;
          return "end";
        }
        break;
      case SS3:
        this.state = GROUND;
        return "end";
      case STRING_OPEN:
        // Replies: OSC `ESC ] 11;rgb:...`, DCS `ESC P 1$r...` or `ESC P >|...`,
        // APC `ESC _ G...`. Anything else was Alt+], Alt+P or Alt+_ in a
        // terminal without the kitty keyboard, and this byte is a new key.
        if (
          (this.opener === 0x5d && byte >= 0x30 && byte <= 0x39)
          || (this.opener === 0x50 && ((byte >= 0x30 && byte <= 0x39) || byte === 0x3e))
          || (this.opener === 0x5f && byte === 0x47)
        ) {
          this.state = STRING;
          this.length = 0;
          return "inside";
        }
        break;
      case STRING:
      case STRING_ESC:
        if (this.state === STRING_ESC && byte === 0x5c) {
          this.state = GROUND;
          return "end";
        }
        if (byte === 0x07 || (this.length += 1) > MAX_STRING_BYTES) {
          this.state = GROUND;
          return "end";
        }
        this.state = byte === 0x1b ? STRING_ESC : STRING;
        return "inside";
    }
    this.state = GROUND;
    if (byte === 0x1b) {
      this.state = ESC;
      return "start";
    }
    return "text";
  }
}

/** Where the escape sequence a chunk stops inside of begins, or -1. */
export function unfinishedSequenceStart(data: Uint8Array): number {
  const scanner = new KeySequenceScanner();
  let start = -1;
  for (let index = 0; index < data.length; index += 1) {
    if (scanner.next(data[index]) === "start") {
      start = index;
    }
  }
  return scanner.open ? start : -1;
}

/**
 * How long an unfinished sequence waits for the rest of its bytes. The rest is
 * already in this machine's terminal buffer when a read splits a sequence, so
 * this only delays a real Esc press in a terminal without the kitty keyboard
 * (with it, Esc is `CSI 27 u`).
 */
export const ESC_WAIT_MS = 50;

/**
 * Keys leave this terminal in whole escape sequences: an unfinished one at the
 * end of a read waits for the rest, so every chunk sent to the cloud parses
 * there exactly as the terminal meant it.
 */
export class KeyFramer {
  private held: Buffer | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private readonly emit: (chunk: Buffer) => void;
  private readonly waitMs: number;

  constructor(emit: (chunk: Buffer) => void, waitMs = ESC_WAIT_MS) {
    this.emit = emit;
    this.waitMs = waitMs;
  }

  push(chunk: Buffer): void {
    this.stopTimer();
    let data = this.held ? Buffer.concat([this.held, chunk]) : chunk;
    this.held = null;
    const cut = unfinishedSequenceStart(data);
    if (cut >= 0 && data.length - cut <= MAX_STRING_BYTES) {
      this.held = Buffer.from(data.subarray(cut));
      data = data.subarray(0, cut);
      this.timer = setTimeout(() => this.flush(), this.waitMs);
      this.timer.unref?.();
    }
    if (data.length) {
      this.emit(data);
    }
  }

  /** Send what is held: the wait ran out, so it was a key of its own. */
  flush(): void {
    this.stopTimer();
    const held = this.held;
    this.held = null;
    // A reply cut short is not keys: typed into the dashboard, it is noise.
    if (held && !(held.length > 2 && held[0] === 0x1b && (held[1] === 0x5d || held[1] === 0x50 || held[1] === 0x5f))) {
      this.emit(held);
    }
  }

  dispose(): void {
    this.stopTimer();
    this.held = null;
  }

  private stopTimer(): void {
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }
}
