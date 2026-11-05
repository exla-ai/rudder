// Links clicked in a `rudder cloud` dashboard open on this computer.
//
// The dashboard runs on the Cloud machine, which has no browser. When a click
// lands on a link there, it writes `ESC ] 7770 ; open ; <url> BEL` to its
// terminal (native/src/selection.rs), and the bytes reach this client with the
// frame. Terminals ignore that OSC, so the output passes through unchanged.
//
// The relay replays recent output to every new connection, so a request only
// counts right after this terminal sent a left click, and once per click: a
// reattach never reopens old links, and a read-only observer opens nothing.

const OPEN_PREFIX = "\x1b]7770;open;";
const CLICK_WINDOW_MS = 5_000;
const MAX_REQUEST_BYTES = 8 * 1024;
const OPENABLE = /^(?:https?:\/\/|mailto:)[^\s\x00-\x1f\x7f]+$/i;
// SGR mouse release: `ESC [ < button ; col ; row m`.
const MOUSE_RELEASE = /\x1b\[<(\d+);\d+;\d+m/g;

export class LinkOpener {
  private clickAt = Number.NEGATIVE_INFINITY;
  private carry = "";
  private readonly open: (url: string) => void;
  private readonly now: () => number;

  constructor(open: (url: string) => void, now: () => number = Date.now) {
    this.open = open;
    this.now = now;
  }

  /** Keys sent to the dashboard: a left click allows one open. */
  noteInput(data: Buffer): void {
    for (const match of data.toString("latin1").matchAll(MOUSE_RELEASE)) {
      // The left button; shift/alt/ctrl bits may be set, motion or wheel not.
      if ((Number(match[1]) & ~0b11100) === 0) {
        this.clickAt = this.now();
      }
    }
  }

  /** Output from the dashboard. Opens what it asks for; returns those urls. */
  noteOutput(data: Buffer): string[] {
    const text = this.carry + data.toString("latin1");
    this.carry = "";
    const opened: string[] = [];
    let from = 0;
    for (;;) {
      const start = text.indexOf(OPEN_PREFIX, from);
      if (start < 0) {
        break;
      }
      const body = start + OPEN_PREFIX.length;
      const bel = text.indexOf("\x07", body);
      const st = text.indexOf("\x1b\\", body);
      const end = bel < 0 ? st : st < 0 ? bel : Math.min(bel, st);
      if (end < 0) {
        // Split across frames: finish it with the next one.
        if (text.length - start <= MAX_REQUEST_BYTES) {
          this.carry = text.slice(start);
        }
        return opened;
      }
      const url = Buffer.from(text.slice(body, end), "latin1").toString("utf8");
      if (this.take(url)) {
        opened.push(url);
      }
      from = end + 1;
    }
    // The frame may end partway into the prefix.
    for (let n = Math.min(OPEN_PREFIX.length - 1, text.length - from); n > 0; n -= 1) {
      if (OPEN_PREFIX.startsWith(text.slice(text.length - n))) {
        this.carry = text.slice(text.length - n);
        break;
      }
    }
    return opened;
  }

  private take(url: string): boolean {
    if (this.now() - this.clickAt > CLICK_WINDOW_MS || !OPENABLE.test(url)) {
      return false;
    }
    this.clickAt = Number.NEGATIVE_INFINITY;
    this.open(url);
    return true;
  }
}
