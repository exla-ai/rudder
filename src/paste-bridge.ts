// Pasting files from this computer into a dashboard on the cloud machine.
//
// An agent on the home machine reads ITS clipboard and ITS disk: Claude Code's
// Ctrl+V runs xclip there (no clipboard, nothing pasted), and a screenshot
// dragged into the terminal arrives as a path like /Users/me/Desktop/shot.png
// that does not exist there. So the attach client looks at what is about to go
// up, and when it is a clipboard image (Ctrl+V) or a paste made only of local
// file paths, it uploads the files first (the phone's upload route, into the
// repo's .rudder/uploads) and sends the cloud paths as a paste instead. Claude
// Code and Codex both turn a pasted image path into an attached image; the
// dashboard's task box turns it into an [Image #N] chip.
//
// Everything else goes straight through, untouched and unbuffered.

import { execFile } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

export const PASTE_UPLOAD_MAX_BYTES = 25 * 1024 * 1024;
const PASTE_BEGIN = "\x1b[200~";
const PASTE_END = "\x1b[201~";
const MAX_FILES = 20;

export type ClipboardContents = { files: string[] } | { image: string } | null;

export type PasteBridgeOptions = {
  send: (data: Buffer) => void;
  /** Upload one local file, resolve to its path on the cloud machine. */
  upload: (file: string, name: string) => Promise<string>;
  readClipboard?: () => Promise<ClipboardContents>;
  status?: (text: string | null) => void;
};

/** Ctrl+V, as a plain control byte or in the kitty keyboard encoding. */
export function isCtrlV(data: Buffer): boolean {
  if (data.length === 1 && data[0] === 0x16) {
    return true;
  }
  return /^\x1b\[118(?::\d*)*;5(?::1)?u$/.test(data.toString("latin1"));
}

/**
 * The local files a paste names, or null when it is anything else. Every
 * token must be an existing regular file, so a pasted snippet that merely
 * mentions a path is never rewritten. Handles what terminals produce for a
 * drop: 'quoted', "quoted", backslash-escaped spaces, file:// urls.
 */
export function pastedLocalFiles(text: string, home = os.homedir()): string[] | null {
  const tokens = shellTokens(text.trim());
  if (!tokens || tokens.length === 0 || tokens.length > MAX_FILES) {
    return null;
  }
  const files: string[] = [];
  for (let token of tokens) {
    if (token.startsWith("file://")) {
      try {
        token = decodeURIComponent(new URL(token).pathname);
      } catch {
        return null;
      }
    }
    if (token.startsWith("~/")) {
      token = path.join(home, token.slice(2));
    }
    if (!path.isAbsolute(token)) {
      return null;
    }
    try {
      const stat = fs.statSync(token);
      if (!stat.isFile() || stat.size === 0 || stat.size > PASTE_UPLOAD_MAX_BYTES) {
        return null;
      }
    } catch {
      return null;
    }
    files.push(token);
  }
  return files;
}

function shellTokens(text: string): string[] | null {
  if (!text || /[\x00-\x08\x0b-\x1f\x7f]/.test(text)) {
    return null;
  }
  const tokens: string[] = [];
  let current = "";
  let inToken = false;
  let quote: "'" | "\"" | null = null;
  for (let i = 0; i < text.length; i += 1) {
    const ch = text[i];
    if (quote) {
      if (ch === quote) {
        quote = null;
      } else if (ch === "\\" && quote === "\"" && i + 1 < text.length) {
        current += text[++i];
      } else {
        current += ch;
      }
      continue;
    }
    if (ch === "'" || ch === "\"") {
      quote = ch;
      inToken = true;
    } else if (ch === "\\" && i + 1 < text.length) {
      current += text[++i];
      inToken = true;
    } else if (/\s/.test(ch)) {
      if (inToken) {
        tokens.push(current);
      }
      current = "";
      inToken = false;
    } else {
      current += ch;
      inToken = true;
    }
  }
  if (quote) {
    return null;
  }
  if (inToken) {
    tokens.push(current);
  }
  return tokens;
}

export class PasteBridge {
  private readonly options: PasteBridgeOptions;
  /** Pass everything straight through (a target that cannot take uploads). */
  disabled = false;
  private busy = false;
  private pending: Buffer[] = [];

  constructor(options: PasteBridgeOptions) {
    this.options = options;
  }

  /** Every keystroke chunk for the cloud goes through here, in order. */
  forward(data: Buffer): void {
    if (this.busy) {
      this.pending.push(data);
      return;
    }
    const job = this.disabled ? null : this.jobFor(data);
    if (!job) {
      this.options.send(data);
      return;
    }
    this.busy = true;
    void job.then((replacement) => {
      this.options.send(replacement ?? data);
    }).catch((error) => {
      this.options.send(data);
      this.options.status?.(`Could not send that file to the cloud: ${(error as Error).message}`);
      setTimeout(() => this.options.status?.(null), 4000).unref?.();
    }).finally(() => {
      this.busy = false;
      const queued = this.pending;
      this.pending = [];
      for (const chunk of queued) {
        this.forward(chunk);
      }
    });
  }

  private jobFor(data: Buffer): Promise<Buffer | null> | null {
    if (isCtrlV(data)) {
      if (!this.options.readClipboard) {
        return null;
      }
      return this.fromClipboard();
    }
    const text = data.toString("utf8");
    let inner = text;
    let bracketed = false;
    if (text.startsWith(PASTE_BEGIN) && text.endsWith(PASTE_END)) {
      inner = text.slice(PASTE_BEGIN.length, text.length - PASTE_END.length);
      bracketed = true;
    } else if (text.length < 4 || !/^['"~\/f]/.test(text)) {
      return null;
    }
    const files = pastedLocalFiles(inner);
    if (!files) {
      return null;
    }
    const trailing = /\s$/.test(inner) ? " " : "";
    return this.uploadAll(files).then((remote) => {
      const body = remote.join(" ") + trailing;
      return Buffer.from(bracketed ? PASTE_BEGIN + body + PASTE_END : body, "utf8");
    });
  }

  private async fromClipboard(): Promise<Buffer | null> {
    const contents = await this.options.readClipboard!().catch(() => null);
    if (!contents) {
      return null;
    }
    let remote: string[];
    if ("image" in contents) {
      try {
        remote = await this.uploadAll([contents.image], "image");
      } finally {
        fs.rmSync(contents.image, { force: true });
      }
    } else {
      remote = await this.uploadAll(contents.files);
    }
    return Buffer.from(PASTE_BEGIN + remote.join(" ") + PASTE_END, "utf8");
  }

  private async uploadAll(files: string[], kind = "file"): Promise<string[]> {
    const noun = kind === "image" ? "image" : files.length === 1 ? path.basename(files[0]) : `${files.length} files`;
    this.options.status?.(`Sending ${noun} to the cloud...`);
    try {
      const remote: string[] = [];
      for (const file of files) {
        remote.push(await this.options.upload(file, kind === "image" ? "clipboard.png" : path.basename(file)));
      }
      return remote;
    } finally {
      this.options.status?.(null);
    }
  }
}

// The clipboard of THIS computer: files copied in Finder win, then an image
// (a screenshot, "Copy Image"), the same order Claude Code uses locally.
export async function readLocalClipboard(): Promise<ClipboardContents> {
  const out = path.join(os.tmpdir(), `rudder-clip-${process.pid}-${Date.now()}.png`);
  if (process.platform === "darwin") {
    const raw = await run("osascript", ["-l", "JavaScript", "-e", MAC_CLIPBOARD_SCRIPT, out]).catch(() => "");
    try {
      const parsed = JSON.parse(raw.trim() || "{}") as { files?: string[]; image?: string };
      const files = (parsed.files ?? []).filter((file) => pastedLocalFiles(JSON.stringify(file)) !== null);
      if (files.length) {
        return { files };
      }
      if (parsed.image && fs.existsSync(parsed.image) && fs.statSync(parsed.image).size > 0) {
        return { image: parsed.image };
      }
    } catch {
      // not JSON: no clipboard we can use
    }
    return null;
  }
  if (process.platform === "linux") {
    for (const [cmd, args] of [["wl-paste", ["--type", "image/png"]], ["xclip", ["-selection", "clipboard", "-t", "image/png", "-o"]]] as const) {
      const bytes = await runBytes(cmd, [...args]).catch(() => null);
      if (bytes && bytes.length > 8 && bytes.subarray(1, 4).toString("latin1") === "PNG") {
        fs.writeFileSync(out, bytes, { mode: 0o600 });
        return { image: out };
      }
    }
  }
  return null;
}

const MAC_CLIPBOARD_SCRIPT = `
ObjC.import('AppKit');
function run(argv) {
  const pb = $.NSPasteboard.generalPasteboard;
  const opts = $.NSDictionary.dictionaryWithObjectForKey($.NSNumber.numberWithBool(true), $.NSPasteboardURLReadingFileURLsOnlyKey);
  const urls = pb.readObjectsForClassesOptions($.NSArray.arrayWithObject($.NSURL), opts);
  const files = [];
  if (urls && !urls.isNil()) {
    for (let i = 0; i < urls.count; i++) files.push(urls.objectAtIndex(i).path.js);
  }
  if (files.length) return JSON.stringify({ files });
  const types = ObjC.deepUnwrap(pb.types) || [];
  const imageTypes = ['public.png', 'public.tiff', 'public.jpeg', 'com.apple.pict'];
  if (!types.some((t) => imageTypes.includes(t))) return '{}';
  const image = $.NSImage.alloc.initWithPasteboard(pb);
  if (image.isNil()) return '{}';
  const rep = $.NSBitmapImageRep.imageRepWithData(image.TIFFRepresentation);
  const png = rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $({}));
  if (!png.writeToFileAtomically(argv[0], true)) return '{}';
  return JSON.stringify({ image: argv[0] });
}`;

function run(cmd: string, args: string[]): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile(cmd, args, { timeout: 5000, maxBuffer: 1024 * 1024 }, (error, stdout) => (error ? reject(error) : resolve(String(stdout))));
  });
}

function runBytes(cmd: string, args: string[]): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    execFile(cmd, args, { timeout: 5000, maxBuffer: PASTE_UPLOAD_MAX_BYTES, encoding: "buffer" }, (error, stdout) => (error ? reject(error) : resolve(stdout as Buffer)));
  });
}
