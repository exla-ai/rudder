import { BoardAccessExpired, boardFetch } from "./access";

export type BoardEvent = { event: string; data: string };
const MAX_FRAME = 4 * 1024 * 1024;

/** Incremental bounded SSE parsing; comments and partial UTF-8 survive chunking. */
export async function readBoardEvents(response: Response, consume: (event: BoardEvent) => void, signal: AbortSignal): Promise<void> {
  if (!response.body) throw new Error("Board stream unavailable");
  const reader = response.body.getReader();
  const decoder = new TextDecoder("utf-8", { fatal: true });
  let pending = "", event = "message", data: string[] = [], bytes = 0;
  let idle = setTimeout(() => { void reader.cancel().catch(() => undefined); }, 45_000);
  const cancelled = () => { void reader.cancel().catch(() => undefined); };
  signal.addEventListener("abort", cancelled, { once: true });
  try {
    while (!signal.aborted) {
      const next = await reader.read();
      if (next.done) return;
      clearTimeout(idle);
      idle = setTimeout(() => { void reader.cancel().catch(() => undefined); }, 45_000);
      if (next.value.byteLength > 2 * MAX_FRAME) throw new Error("Board stream exceeds frame limit");
      pending += decoder.decode(next.value, { stream: true });
      if (pending.length > 2 * MAX_FRAME) throw new Error("Board stream exceeds frame limit");
      for (;;) {
        const end = pending.indexOf("\n");
        if (end < 0) break;
        const line = pending.slice(0, end).replace(/\r$/, "");
        pending = pending.slice(end + 1);
        bytes += new TextEncoder().encode(line).byteLength + 1;
        if (bytes > MAX_FRAME) throw new Error("Board stream exceeds frame limit");
        if (line === "") {
          if (data.length) consume({ event, data: data.join("\n") });
          event = "message"; data = []; bytes = 0;
        } else if (line.startsWith("event:")) event = line.slice(6).replace(/^ /, "");
        else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
      }
      if (pending.length + bytes > MAX_FRAME) throw new Error("Board stream exceeds frame limit");
    }
  } finally {
    clearTimeout(idle);
    signal.removeEventListener("abort", cancelled);
    await reader.cancel().catch(() => undefined);
    reader.releaseLock();
  }
}

function retryDelay(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const done = () => { clearTimeout(timer); signal.removeEventListener("abort", done); resolve(); };
    const timer = setTimeout(done, ms);
    if (signal.aborted) done(); else signal.addEventListener("abort", done, { once: true });
  });
}

export async function streamBoardEvents(input: string, options: { signal: AbortSignal; open: () => void;
  consume: (event: BoardEvent) => void; reconnecting: () => void }, fetcher = boardFetch): Promise<void> {
  let retries = 0;
  while (!options.signal.aborted) {
    const controller = new AbortController();
    const abort = () => controller.abort();
    const connecting = setTimeout(abort, 10_000);
    options.signal.addEventListener("abort", abort, { once: true });
    try {
      if (options.signal.aborted) return;
      const response = await fetcher(input, { headers: { Accept: "text/event-stream" }, signal: controller.signal });
      clearTimeout(connecting);
      if (!response.ok || !response.headers.get("content-type")?.startsWith("text/event-stream")) {
        await response.body?.cancel(); throw new Error("Board stream unavailable");
      }
      options.open(); retries = 0;
      await readBoardEvents(response, options.consume, controller.signal);
    } catch (error) {
      if (error instanceof BoardAccessExpired) return;
    } finally { clearTimeout(connecting); options.signal.removeEventListener("abort", abort); controller.abort(); }
    if (options.signal.aborted) return;
    options.reconnecting();
    await retryDelay(Math.min(10_000, 500 * 2 ** Math.min(retries++, 5)), options.signal);
  }
}
