// The laptop terminal's colors, for agents running on the cloud machine.
//
// opencode, Codex and Claude Code's auto theme pick light or dark by asking
// the terminal for its background (OSC 11) and, if they asked for mode 2031,
// hear when it changes (`CSI ? 997 ; 1|2 n`). On the cloud machine the
// terminal they ask is the dashboard's, which has no colors of its own, so they
// always came up dark. The attach client asks the REAL terminal, takes the
// answers out of the key stream, and reports them; the machine writes them
// where every dashboard there reads them and answers its agents from them.

export type TerminalColors = { fg: string | null; bg: string | null };

/** Ask the terminal for its colors and to report appearance changes. */
export const THEME_QUERY = "\x1b]10;?\x1b\\\x1b]11;?\x1b\\";
export const THEME_REPORTS_ON = "\x1b[?2031h";
export const THEME_REPORTS_OFF = "\x1b[?2031l";

// `ESC ] 10|11 ; rgb:RRRR/GGGG/BBBB` ended by BEL or ST; 1 to 4 hex digits each.
const COLOR_REPLY = /\x1b\](1[01]);rgb:([0-9a-fA-F]{1,4})\/([0-9a-fA-F]{1,4})\/([0-9a-fA-F]{1,4})(?:\x07|\x1b\\)/g;
// The appearance changed (1 dark, 2 light): ask for the colors again.
const THEME_CHANGED = /\x1b\[\?997;[12]n/g;

function channel(hex: string): string {
  // Scale 1 to 4 hex digits to 8 bits, as xterm does.
  const max = (1 << (hex.length * 4)) - 1;
  return Math.round((parseInt(hex, 16) / max) * 255).toString(16).padStart(2, "0");
}

export class TerminalThemeWatcher {
  colors: TerminalColors = { fg: null, bg: null };
  private readonly ask: () => void;
  private readonly changed: (colors: TerminalColors) => void;

  constructor(ask: () => void, changed: (colors: TerminalColors) => void) {
    this.ask = ask;
    this.changed = changed;
  }

  get known(): boolean {
    return this.colors.bg !== null;
  }

  /**
   * Take color replies and appearance notices out of a chunk of input and
   * return the keys that are left. Terminals answer in one write, so a reply
   * split across chunks is not stitched; it passes through as keys.
   */
  filter(data: Buffer): Buffer {
    const text = data.toString("latin1");
    if (!text.includes("\x1b]1") && !text.includes("\x1b[?997;")) {
      return data;
    }
    const next = { ...this.colors };
    let rest = text.replace(COLOR_REPLY, (_match, which: string, r: string, g: string, b: string) => {
      next[which === "10" ? "fg" : "bg"] = `#${channel(r)}${channel(g)}${channel(b)}`;
      return "";
    });
    let reask = false;
    rest = rest.replace(THEME_CHANGED, () => {
      reask = true;
      return "";
    });
    if (next.fg !== this.colors.fg || next.bg !== this.colors.bg) {
      this.colors = next;
      this.changed(next);
    }
    if (reask) {
      this.ask();
    }
    return rest.length === text.length ? data : Buffer.from(rest, "latin1");
  }
}
