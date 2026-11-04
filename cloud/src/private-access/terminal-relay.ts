import { randomUUID } from "node:crypto";
import { TerminalScreenSession, type TerminalCursor, type TerminalScreen, type TerminalSocket } from "./terminal-session.js";

export type TerminalSelection = { actor: string; workspaceId: string; runId: string };
export interface NegotiatedTerminalSocket extends TerminalSocket {
  on(event: "message", callback: (data: unknown, binary: boolean) => void): unknown;
  on(event: "close" | "error", callback: () => void): unknown;
  close(code?: number, reason?: string): void;
}
export type CanonicalTerminalFrame = {
  version: 1; type: "terminal-screen"; generation: string; sequence: number;
  owner: { project_id: string; owner_id: string; epoch: number };
  screen: TerminalScreen; text: string;
};
const id = (value: unknown): value is string => typeof value === "string" && value.length > 0 && value.length <= 256 && !/[\x00-\x1f\x7f]/.test(value);
export function canonicalTerminalFrame(value: unknown): CanonicalTerminalFrame {
  const frame = value as CanonicalTerminalFrame;
  if (!frame || frame.version !== 1 || frame.type !== "terminal-screen" || !id(frame.generation)
    || !Number.isSafeInteger(frame.sequence) || frame.sequence < 0 || !id(frame.owner?.project_id)
    || !id(frame.owner?.owner_id) || !Number.isSafeInteger(frame.owner?.epoch) || frame.owner.epoch < 1
    || typeof frame.text !== "string" || Buffer.byteLength(frame.text) > 128 * 1024) throw new Error("Current canonical terminal is unavailable");
  return frame;
}
function cursor(value: unknown): TerminalCursor | undefined {
  const v = value as TerminalCursor;
  return v && typeof v.generation === "string" && v.generation.length <= 2048 && Number.isSafeInteger(v.sequence) && v.sequence >= 0
    ? { generation: v.generation, sequence: v.sequence } : undefined;
}

/** Explicit canonical-screen-text-v1 capability for the read-only agent pane.
 * Each socket captures one authenticated run/owner. It never touches the
 * legacy dashboard byte queue, input writer or scheduler. Paint ACKs release
 * only this viewer's screen slot. Owner replacement requires a new handshake.
 */
export class CanonicalTerminalRelay {
  #active = new Set<() => void>();
  constructor(readonly options: {
    authorize(selection: TerminalSelection): Promise<boolean>;
    read(selection: TerminalSelection, signal: AbortSignal): Promise<unknown>;
    pollMs?: number; maxViews?: number;
  }) {}
  get activeViews(): number { return this.#active.size; }
  async attach(selection: TerminalSelection, clientId: string, socket: NegotiatedTerminalSocket): Promise<void> {
    if (!id(selection.actor) || !id(selection.workspaceId) || !id(selection.runId) || !id(clientId)
      || socket.readyState !== 1 || this.#active.size >= (this.options.maxViews ?? 64)) throw new Error("Terminal view is unavailable");
    const controller = new AbortController();
    let closed = false, timer: ReturnType<typeof setTimeout> | undefined, session: TerminalScreenSession | undefined;
    const stop = () => {
      if (closed) return; closed = true; controller.abort(); clearTimeout(timer);
      session?.dispose(); this.#active.delete(stop);
    };
    this.#active.add(stop);
    socket.on("close", stop); socket.on("error", stop);
    try {
      const read = async () => {
        if (!(await this.options.authorize(selection))) throw new Error("Terminal access was revoked");
        const value = await this.options.read(selection, AbortSignal.any([controller.signal, AbortSignal.timeout(3000)]));
        if (!(await this.options.authorize(selection))) throw new Error("Terminal access was revoked");
        if (closed || socket.readyState !== 1) throw new Error("Terminal view disconnected");
        return canonicalTerminalFrame(value);
      };
      const first = await read();
      const ownerGeneration = JSON.stringify([first.owner.project_id, first.owner.owner_id, first.owner.epoch, selection.runId, first.generation]);
      // Each negotiated attachment starts a fresh relay sequence. A reconnect
      // must therefore negotiate a fresh generation, even for the same PTY.
      const generation = JSON.stringify([ownerGeneration, randomUUID()]);
      session = new TerminalScreenSession(generation, { writeInput: () => {}, maxScreenBytes: 256 * 1024 });
      socket.send(JSON.stringify({version:1,type:"terminal-capability",capability:"canonical-screen-text-v1",access:"read-only",generation}), () => {});
      const view = session.attach(clientId, socket);
      socket.on("message", (data, binary) => {
        if (closed || binary || typeof data !== "string" && !Buffer.isBuffer(data)) return;
        const raw = String(data); if (Buffer.byteLength(raw) > 4096) { stop(); socket.close(1009,"Terminal control exceeds bound"); return; }
        let message: Record<string, unknown>; try { message = JSON.parse(raw); } catch { return; }
        if (!message || message.version !== 1) return;
        if (message.type === "terminal-painted") { const c = cursor(message); if (c) session!.acknowledge(view,c); }
        if (message.type === "terminal-snapshot-request") session!.requestSnapshot(view,cursor(message.cursor));
      });
      let previous = "";
      const publish = (frame: CanonicalTerminalFrame) => {
        if (JSON.stringify([frame.owner.project_id,frame.owner.owner_id,frame.owner.epoch,selection.runId,frame.generation]) !== ownerGeneration) throw new Error("Terminal owner changed");
        const key = `${frame.generation}:${frame.sequence}`;
        if (key !== previous) { session!.publish({...frame.screen,text:frame.text}); previous = key; }
      };
      publish(first);
      const poll = async () => {
        try { publish(await read()); }
        catch { stop(); socket.close(4001,"Current terminal custody is unavailable"); return; }
        if (!closed) timer = setTimeout(() => void poll(),this.options.pollMs ?? 250);
      };
      timer = setTimeout(() => void poll(),this.options.pollMs ?? 250);
    } catch (error) { stop(); throw error; }
  }
  close(): void { for (const stop of [...this.#active]) stop(); }
}
