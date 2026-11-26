// The app's Terminal: a shell on the account's own cloud machine
// (worker/machine-shell.mjs), typed into from the app (app/shell-panel.js).
// The app asks for a ticket over its signed-in API, then opens a socket with
// it: a browser socket cannot carry the app's sign-in token, and the Mac and
// iPhone apps have no cookie. A ticket opens one socket, within half a minute.
//
// Frames are JSON text. From the app: { type: "open" | "resize", cols, rows }
// and { type: "input", data }. To the app: { type: "ready", replay },
// { type: "output", data }, { type: "exit" } and { type: "error", message }.
// The home sends its output as control messages; every terminal the account
// has open sees the same shell.
import { randomBytes } from "node:crypto";
import type { WebSocket } from "ws";

const TICKET_MS = 30_000;
const MAX_INPUT = 65_536;

export type ShellRelayDeps = {
  /** The account's home with its control socket up, woken if it sleeps. Throws when there is none. */
  home(accountId: string): Promise<string>;
  rpc(homeId: string, method: string, params: Record<string, number>): Promise<unknown>;
  /** One control message to the home; dropped when its socket is down. */
  post(homeId: string, message: Record<string, unknown>): void;
};

export class ShellRelay {
  #deps: ShellRelayDeps;
  #tickets = new Map<string, { accountId: string; expires: number }>();
  #clients = new Map<string, Set<WebSocket>>();

  constructor(deps: ShellRelayDeps) {
    this.#deps = deps;
  }

  ticket(accountId: string, now = Date.now()): string {
    for (const [ticket, { expires }] of this.#tickets) if (expires <= now) this.#tickets.delete(ticket);
    const ticket = randomBytes(24).toString("base64url");
    this.#tickets.set(ticket, { accountId, expires: now + TICKET_MS });
    return ticket;
  }

  /** The account a ticket was made for, once. */
  redeem(ticket: string, now = Date.now()): string | null {
    const held = this.#tickets.get(ticket);
    this.#tickets.delete(ticket);
    return held && held.expires > now ? held.accountId : null;
  }

  attach(accountId: string, ws: WebSocket): void {
    let homeId: string | null = null;
    let opening = false;
    const say = (frame: Record<string, unknown>) => { if (ws.readyState === 1) ws.send(JSON.stringify(frame)); };
    const dims = (frame: Record<string, unknown>) => ({ cols: Number(frame.cols), rows: Number(frame.rows) });
    ws.on("message", (data) => {
      let frame: Record<string, unknown>;
      try { frame = JSON.parse(String(data)) as Record<string, unknown>; } catch { ws.close(4400, "bad frame"); return; }
      if (frame.type === "open" && !homeId && !opening) {
        opening = true;
        void (async () => {
          const id = await this.#deps.home(accountId);
          const opened = await this.#deps.rpc(id, "shell-open", dims(frame)) as { replay: string };
          if (ws.readyState !== 1) return;
          homeId = id;
          let clients = this.#clients.get(id);
          if (!clients) this.#clients.set(id, clients = new Set());
          clients.add(ws);
          say({ type: "ready", replay: opened.replay });
        })().catch((error: unknown) => {
          say({ type: "error", message: error instanceof Error ? error.message : String(error) });
          ws.close(4500, "the shell did not open");
        });
        return;
      }
      if (!homeId) return;
      if (frame.type === "input" && typeof frame.data === "string" && frame.data.length <= MAX_INPUT) {
        this.#deps.post(homeId, { type: "shell-input", data: frame.data });
      } else if (frame.type === "resize") {
        this.#deps.post(homeId, { type: "shell-resize", ...dims(frame) });
      }
    });
    ws.on("close", () => {
      if (!homeId) return;
      const clients = this.#clients.get(homeId);
      clients?.delete(ws);
      if (clients && !clients.size) this.#clients.delete(homeId);
    });
    ws.on("error", () => undefined);
  }

  /** A control message from a home. True when it was the shell's. */
  fromHome(homeId: string, message: Record<string, unknown>): boolean {
    const frame = message.type === "shell-output" && typeof message.data === "string" ? { type: "output", data: message.data }
      : message.type === "shell-exit" ? { type: "exit" } : null;
    if (!frame) return false;
    const text = JSON.stringify(frame);
    for (const ws of this.#clients.get(homeId) ?? []) if (ws.readyState === 1) ws.send(text);
    return true;
  }

  /** The home's control socket dropped: its terminals close, and the app opens them again. */
  homeGone(homeId: string): void {
    const clients = this.#clients.get(homeId);
    this.#clients.delete(homeId);
    for (const ws of clients ?? []) ws.close(4001, "the cloud machine disconnected");
  }
}
