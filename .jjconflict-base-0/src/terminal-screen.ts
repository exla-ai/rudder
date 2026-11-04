import { queryOwnerSurface } from "./project-files/engine.js";

/** Complete parsed state from the exact native App owner. A phone text view may
 * use text; negotiated terminal views use screen. Neither is a byte replay tail. */
export async function readCanonicalScreen(repo: string, runId: string, signal = AbortSignal.timeout(3000)) {
  return readOwnedScreen(repo, runId, signal, false);
}

/** A historical marker grants no terminal stream authority. Only the current
 * owner may authorize reading a retired run's persisted mirror. */
export async function readOwnedScreen(repo: string, runId: string, signal = AbortSignal.timeout(3000), allowHistory = true) {
  if (!runId || runId.length > 256 || /[\x00-\x1f\x7f]/.test(runId)) throw new Error("Invalid terminal identity");
  const { raw, status } = await queryOwnerSurface(repo, "screen", signal, undefined, runId);
  const frame = JSON.parse(raw).terminal_screen;
  if (allowHistory && frame?.version === 1 && frame.type === "terminal-history" && frame.runId === runId) {
    return { ...frame, owner: status.owner };
  }
  if (frame?.version !== 1 || frame.type !== "terminal-screen" || typeof frame.generation !== "string"
    || !Number.isSafeInteger(frame.sequence) || frame.sequence < 0 || frame.screen?.format !== "vt100-ansi-v1"
    || typeof frame.text !== "string") throw new Error("Complete terminal screen is unavailable");
  return { ...frame, owner: status.owner, updatedAt: Date.now() };
}
