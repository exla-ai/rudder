import { StringDecoder } from "node:string_decoder";

// A gap must not leave the viewer inside a partial OSC/CSI or synchronized
// update. The dashboard supplies a fresh full frame after these parser resets.
const RECOVER_OUTPUT = Buffer.from("\x18\x1b\\\x1b[0m\x1b[?2026l");

// How long a key's escape sequence that ends a message waits for the rest. The
// two halves of a split read are sent back to back, so the rest comes within a
// few milliseconds; only a lone Esc in a terminal without the kitty keyboard
// (and Alt+], Alt+P, Alt+_ there) waits this long.
const KEY_WAIT_MS = 50;
// A reply longer than this lost its terminator; stop taking keys for it.
const MAX_REPLY_BYTES = 4096;

const ESC = 0x1b;
const GROUND = 0;
const ESCAPE = 1;
const CSI = 2;
const SS3 = 3;
// ESC followed by `]`, `P` or `_`: a terminal reply if the next byte says so.
const STRING_OPEN = 4;
const STRING = 5;
const STRING_ESC = 6;
const PASTE = 7;
const ALT_CHAR = 8;
const PASTE_BEGIN = "\x1b[200~";
const PASTE_END = Buffer.from("\x1b[201~");

/**
 * Keys reach the dashboard in whole escape sequences, and the terminal's own
 * replies never reach it. Its key parser (crossterm) reads an ESC that ends a
 * read as the Esc key and types the rest of the sequence into the composer (a
 * click's "[<0;233;12M"), types the payload of OSC, DCS and APC replies after
 * an Alt key ("11;rgb:..."), types the rest of `CSI >` and `CSI =` replies, and
 * takes every key after a `CSI ?` reply for part of it until a "c" or "u".
 * Clients split sequences across messages wherever their reads did (`rudder
 * cloud` before 4.2.10 sent each read as it came), and each message is its own
 * PTY write, which the dashboard may read on its own. Every client's keys pass
 * here. src/key-sequences.ts frames the same way on the laptop.
 */
function createKeyFramer(write, waitMs = KEY_WAIT_MS) {
  let state = GROUND;
  // The bytes of the key sequence being read (not a reply's body).
  let sequence = [];
  let reply = false;
  let replyBytes = 0;
  let utf8Left = 0;
  let pasteMatched = 0;
  let timer = null;

  function reset() {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    state = GROUND;
    sequence = [];
  }

  // The wait ran out. A lone ESC is the Esc key, and ESC with `]`, `P` or `_`
  // is that Alt key. Anything longer was cut short: written, the dashboard
  // would take the keys typed after it for the rest of it.
  function expire() {
    timer = null;
    const held = state === ESCAPE || state === STRING_OPEN ? Buffer.from(sequence) : null;
    reset();
    if (held) write(held);
  }

  function push(chunk) {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    const out = [];
    const flush = () => {
      if (out.length) write(out.length === 1 ? out[0] : Buffer.concat(out));
      out.length = 0;
    };
    // Where the current run of plain bytes (typed text or a paste) began.
    let run = state === GROUND || state === PASTE ? 0 : -1;
    // The sequence is complete: send it (a key) or drop it (a reply).
    const finish = (keep, index) => {
      if (keep) out.push(Buffer.from(sequence));
      sequence = [];
      state = GROUND;
      run = index + 1;
    };
    // Not the sequence it began as: send what was a key of its own (or
    // nothing) and read this byte again as plain input.
    const bail = (keep, index) => {
      if (keep) out.push(Buffer.from(sequence));
      sequence = [];
      state = GROUND;
      run = index;
      return index - 1;
    };

    for (let index = 0; index < chunk.length; index += 1) {
      const byte = chunk[index];
      switch (state) {
        case GROUND:
          if (byte === ESC) {
            if (run >= 0 && index > run) out.push(chunk.subarray(run, index));
            run = -1;
            state = ESCAPE;
            sequence = [ESC];
          }
          break;
        case PASTE:
          // A paste is the dashboard's whole until its end marker, escapes in
          // it included, however it is split.
          pasteMatched = byte === PASTE_END[pasteMatched] ? pasteMatched + 1 : byte === ESC ? 1 : 0;
          if (pasteMatched === PASTE_END.length) {
            pasteMatched = 0;
            state = GROUND;
          }
          break;
        case ESCAPE:
          sequence.push(byte);
          if (byte === 0x5b) {
            state = CSI;
            reply = false;
          } else if (byte === 0x4f) {
            state = SS3;
          } else if (byte === 0x5d || byte === 0x50 || byte === 0x5f) {
            state = STRING_OPEN;
          } else if (byte === ESC) {
            // The first ESC was Esc on its own. Read with the next sequence,
            // crossterm would take ESC ESC for one Esc and type the rest.
            out.push(Buffer.from([ESC]));
            flush();
            sequence = [ESC];
          } else if (byte >= 0xc2 && byte <= 0xf4) {
            // Alt plus a character of several bytes: wait for all of them.
            utf8Left = byte >= 0xf0 ? 3 : byte >= 0xe0 ? 2 : 1;
            state = ALT_CHAR;
          } else {
            // Alt plus a key, in a terminal without the kitty keyboard.
            finish(true, index);
          }
          break;
        case ALT_CHAR:
          if ((byte & 0xc0) !== 0x80) {
            index = bail(true, index);
          } else {
            sequence.push(byte);
            if ((utf8Left -= 1) === 0) finish(true, index);
          }
          break;
        case CSI:
          if (byte >= 0x20 && byte <= 0x3f) {
            // `CSI ?`, `CSI >` and `CSI =` are replies; no key is spelled so.
            if (sequence.length === 2 && (byte === 0x3f || byte === 0x3e || byte === 0x3d)) reply = true;
            sequence.push(byte);
            if (sequence.length > MAX_REPLY_BYTES) finish(false, index);
          } else if (byte >= 0x40 && byte <= 0x7e) {
            sequence.push(byte);
            const paste = !reply && String.fromCharCode(...sequence) === PASTE_BEGIN;
            finish(!reply, index);
            if (paste) {
              state = PASTE;
              pasteMatched = 0;
            }
          } else {
            index = bail(false, index);
          }
          break;
        case SS3:
          if (byte >= 0x20 && byte <= 0x7e) {
            sequence.push(byte);
            finish(true, index);
          } else {
            index = bail(false, index);
          }
          break;
        case STRING_OPEN: {
          // Replies: OSC `ESC ] 11;rgb:...`, DCS `ESC P 1$r...` or `ESC P >|...`,
          // APC `ESC _ G...`. Anything else was Alt+], Alt+P or Alt+_ in a
          // terminal without the kitty keyboard, and this byte is a new key.
          const opener = sequence[1];
          if (
            (opener === 0x5d && byte >= 0x30 && byte <= 0x39)
            || (opener === 0x50 && ((byte >= 0x30 && byte <= 0x39) || byte === 0x3e))
            || (opener === 0x5f && byte === 0x47)
          ) {
            state = STRING;
            sequence = [];
            replyBytes = 0;
          } else {
            index = bail(true, index);
          }
          break;
        }
        case STRING:
          // A reply's body is dropped as it is read, up to BEL or ST.
          if (byte === 0x07 || (replyBytes += 1) > MAX_REPLY_BYTES) {
            finish(false, index);
          } else if (byte === ESC) {
            state = STRING_ESC;
          }
          break;
        case STRING_ESC:
          if (byte === 0x5c) {
            finish(false, index);
          } else {
            // ESC ended the reply without its terminator and starts a sequence.
            state = ESCAPE;
            sequence = [ESC];
            index -= 1;
          }
          break;
      }
    }
    if (run >= 0 && run < chunk.length) out.push(chunk.subarray(run));
    flush();
    if (state !== GROUND && state !== PASTE) {
      timer = setTimeout(expire, waitMs);
      timer.unref?.();
    }
  }

  return { push, reset };
}

/**
 * One PTY's ordered WebSocket transport. Keys are written as they come, in
 * whole escape sequences and without terminal replies (createKeyFramer); output
 * gets a short batching window and pauses the PTY reader behind a slow socket.
 * Socket sends are not remote receipts: after a disconnect, recover by repaint
 * instead of replaying a possibly incomplete terminal stream.
 */
export function createTerminalTransport({
  writeInput,
  pauseOutput = () => {},
  resumeOutput = () => {},
  requestRepaint = () => {},
  onError = () => {},
  batchMs = 4,
  maxQueuedBytes = 256 * 1024,
  highWaterBytes = 256 * 1024,
  lowWaterBytes = 64 * 1024,
  frameBytes = 64 * 1024,
  keyWaitMs = KEY_WAIT_MS,
} = {}) {
  let socket = null;
  let decoder = new StringDecoder("utf8");
  let firstInput = true;
  const keys = createKeyFramer((bytes) => {
    const text = decoder.write(bytes);
    if (text) writeInput(text);
  }, keyWaitMs);
  let queue = [];
  let queuedBytes = 0;
  let timer = null;
  let paused = false;
  let gap = false;
  let flushing = false;
  let disposed = false;

  const isCurrent = (candidate) => !disposed && socket === candidate;
  const open = () => socket?.readyState === 1;
  const bufferedBytes = () => Number(socket?.bufferedAmount) || 0;

  function setPaused(value) {
    if (paused === value) return;
    paused = value;
    if (value) pauseOutput();
    else resumeOutput();
  }

  function clearTimer() {
    if (timer !== null) clearTimeout(timer);
    timer = null;
  }

  function schedule(delay = batchMs) {
    if (disposed || timer !== null || !open()) return;
    timer = setTimeout(() => {
      timer = null;
      flush();
    }, delay);
    timer.unref?.();
  }

  function discardQueue(error) {
    const discarded = queue;
    queue = [];
    queuedBytes = 0;
    for (const entry of discarded) entry.callback?.(error);
  }

  function detach(candidate) {
    if (!isCurrent(candidate)) return false;
    socket = null;
    clearTimer();
    decoder = new StringDecoder("utf8");
    firstInput = true;
    keys.reset();
    // Some writes handed to the old socket may never have reached the viewer.
    // Continuing with their suffix would corrupt Unicode or terminal state.
    gap = true;
    discardQueue(new Error("terminal connection closed"));
    setPaused(false);
    return true;
  }

  function failed(error, candidate) {
    if (!isCurrent(candidate)) return;
    detach(candidate);
    onError(error, candidate);
  }

  function flush() {
    if (disposed || flushing || !open()) return;
    clearTimer();
    flushing = true;
    try {
      if (gap) {
        gap = false;
        queue.unshift({ data: RECOVER_OUTPUT, binary: true });
        queuedBytes += RECOVER_OUTPUT.length;
        requestRepaint();
      }
      while (queue.length && open()) {
        if (bufferedBytes() >= highWaterBytes) {
          setPaused(true);
          schedule();
          return;
        }
        const entries = [queue.shift()];
        let bytes = entries[0].data.length;
        // Keep original PTY chunks whole. Splitting a valid UTF-8 character
        // solely to meet a frame target breaks clients that decode per frame.
        if (entries[0].binary) {
          while (queue[0]?.binary && bytes + queue[0].data.length <= frameBytes) {
            const entry = queue.shift();
            entries.push(entry);
            bytes += entry.data.length;
          }
        }
        queuedBytes -= bytes;
        const data = entries.length === 1 ? entries[0].data : Buffer.concat(entries.map((entry) => entry.data), bytes);
        const target = socket;
        try {
          target.send(data, { binary: entries[0].binary }, (error) => {
            if (error) failed(error, target);
            for (const entry of entries) entry.callback?.(error);
            if (isCurrent(target) && (queue.length || paused)) schedule(0);
          });
        } catch (error) {
          failed(error, target);
          for (const entry of entries) entry.callback?.(error);
          return;
        }
      }
      if (bufferedBytes() + queuedBytes >= highWaterBytes) setPaused(true);
      else if (bufferedBytes() + queuedBytes <= lowWaterBytes) setPaused(false);
      if (paused) schedule();
    } finally {
      flushing = false;
    }
  }

  return {
    isCurrent,
    attach(candidate) {
      if (disposed) return false;
      if (socket && socket !== candidate) detach(socket);
      socket = candidate;
      decoder = new StringDecoder("utf8");
      firstInput = true;
      keys.reset();
      flush();
      return true;
    },
    detach,
    writeInput(candidate, data) {
      if (!isCurrent(candidate) || !open()) return false;
      let bytes = Buffer.isBuffer(data) ? data : Buffer.from(data);
      if (firstInput) {
        // A reconnect must not join the old socket's partial character, or type
        // replacement glyphs for orphaned continuation bytes from that stream.
        let start = 0;
        while (start < bytes.length && (bytes[start] & 0xc0) === 0x80) start += 1;
        bytes = bytes.subarray(start);
        if (bytes.length) firstInput = false;
      }
      keys.push(bytes);
      return true;
    },
    writeOutput(data) {
      if (disposed || gap) return;
      const bytes = Buffer.isBuffer(data) ? data : Buffer.from(data, "utf8");
      if (!bytes.length) return;
      if (queuedBytes + bytes.length > maxQueuedBytes) {
        // Discard the complete backlog, not a byte slice into UTF-8/OSC/CSI.
        // While disconnected, stop accumulating until a repaint is possible.
        discardQueue(new Error("terminal output exceeded its replay budget"));
        gap = true;
        if (open()) schedule(0);
        return;
      }
      queue.push({ data: bytes, binary: true });
      queuedBytes += bytes.length;
      if (!open()) return;
      if (queuedBytes + bufferedBytes() >= highWaterBytes) setPaused(true);
      if (queuedBytes >= frameBytes) flush();
      else schedule();
    },
    sendControl(payload, callback) {
      if (disposed || !open()) {
        callback?.(new Error("terminal connection is not open"));
        return false;
      }
      const data = Buffer.from(JSON.stringify(payload));
      // Reserve one frame for small lifecycle/control messages behind output,
      // but do not let repeated probes build an unbounded secondary queue.
      if (data.length > frameBytes || queuedBytes + data.length > maxQueuedBytes + frameBytes) {
        callback?.(new Error("terminal control queue exceeded its budget"));
        return false;
      }
      queue.push({ data, binary: false, callback });
      queuedBytes += data.length;
      flush();
      return true;
    },
    flush,
    dispose() {
      if (disposed) return;
      if (socket) detach(socket);
      else discardQueue(new Error("terminal transport disposed"));
      disposed = true;
      clearTimer();
      keys.reset();
      setPaused(false);
    },
  };
}
