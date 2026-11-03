import type { FileDocument, FileListing, FileScope } from "../../../project-files/types.js";
import type { ProjectFileTransport } from "./render.js";

export interface FileSnapshot { listing: FileListing; document: FileDocument | null }

/** One request cycle at a time, with bounded cancellation and no filesystem watchers. */
export function refreshFiles(options: {
  transport: ProjectFileTransport; scope: FileScope; directory: string; path: string | null;
  onSnapshot(snapshot: FileSnapshot): void; onError(error: unknown): void;
  intervalMs?: number; timeoutMs?: number;
}): () => void {
  let stopped = false;
  let next: ReturnType<typeof setTimeout> | undefined;
  let pending: AbortController | undefined;
  const update = async () => {
    pending = new AbortController();
    const timeout = setTimeout(() => pending?.abort(new Error("File request timed out")), options.timeoutMs ?? 10_000);
    try {
      const aborted = new Promise<never>((_, reject) => pending!.signal.addEventListener("abort", () => reject(pending!.signal.reason ?? new Error("File request cancelled")), { once: true }));
      const [listing, document] = await Promise.race([Promise.all([
        options.transport.list(options.scope, options.directory, pending.signal),
        options.path === null ? Promise.resolve(null) : options.transport.read(options.scope, options.path, pending.signal),
      ]), aborted]);
      if (!stopped && !pending.signal.aborted) options.onSnapshot({ listing, document });
    } catch (error) {
      pending.abort();
      if (!stopped) options.onError(error);
      // Do not repeatedly retry revoked access, deleted paths or unresponsive adapters.
      // The explicit Refresh action creates a fresh bounded session.
      stopped = true;
    } finally {
      clearTimeout(timeout);
      if (!stopped) next = setTimeout(update, Math.max(250, options.intervalMs ?? 2000));
    }
  };
  void update();
  return () => { stopped = true; clearTimeout(next); pending?.abort(); };
}
