import { StringDecoder } from "node:string_decoder";

// A gap must not leave the viewer inside a partial OSC/CSI or synchronized
// update. The dashboard supplies a fresh full frame after these parser resets.
const RECOVER_OUTPUT = Buffer.from("\x18\x1b\\\x1b[0m\x1b[?2026l");

/**
 * One PTY's ordered WebSocket transport. Keys are written immediately; output
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
} = {}) {
  let socket = null;
  let decoder = new StringDecoder("utf8");
  let firstInput = true;
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
      const text = decoder.write(bytes);
      if (text) writeInput(text);
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
      setPaused(false);
    },
  };
}
