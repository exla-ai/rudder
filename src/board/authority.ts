import { createHash, randomBytes, randomUUID } from "node:crypto";
import type { IncomingMessage } from "node:http";

export const BOARD_ACCESS_VERSION = 1;
const TICKET_TTL_MS = 30_000;
const SESSION_TTL_MS = 60 * 60 * 1000;
const MAX_TICKETS = 64;
const MAX_SESSIONS = 64;
const MAX_STREAMS = 32;

export type BoardTarget = "project" | "index" | "usage";
export type BoardControlMode = "projector" | "scheduler";
export type BoardDescriptor = { format_version: 1; instance_id: string; origin: string };
export type BoardSessionInfo = BoardDescriptor & {
  expires_at_ms: number;
  owner_slug: string;
  control_mode: BoardControlMode;
  scope: { registered_projects: true; usage: true; owner_project_control: true };
};
export type BoardSession = {
  expiresAt: number;
  streams: Set<() => void>;
};

function digest(value: string): string { return createHash("sha256").update(value).digest("hex"); }
function secret(value: unknown): value is string { return typeof value === "string" && /^[a-f0-9]{64}$/.test(value); }

/** Host-created browser authority. Nothing here is persisted or served in HTML. */
export class BoardAuthority {
  readonly instanceId = randomUUID();
  #origin = "";
  #tickets = new Map<string, number>();
  #sessions = new Map<string, BoardSession>();
  #stopped = false;
  #timer: ReturnType<typeof setInterval>;

  constructor(readonly ownerSlug: string, readonly controlMode: BoardControlMode,
    private readonly clock: () => number = Date.now) {
    this.#timer = setInterval(() => this.reap(), 1000);
    this.#timer.unref();
  }

  bindOrigin(origin: string): void {
    const url = new URL(origin);
    if (this.#origin || url.protocol !== "http:" || url.hostname !== "127.0.0.1" || url.origin !== origin) {
      throw new Error("Invalid board origin");
    }
    this.#origin = origin;
  }

  descriptor(): BoardDescriptor {
    return { format_version: BOARD_ACCESS_VERSION, instance_id: this.instanceId, origin: this.#origin };
  }

  /** Host callbacks may mint only these fixed board destinations. */
  bootstrapUrl(target: BoardTarget = "project"): string {
    this.reap();
    if (this.#stopped || !this.#origin || this.#tickets.size >= MAX_TICKETS) throw new Error("Board access is unavailable");
    const ticket = randomBytes(32).toString("hex");
    this.#tickets.set(digest(ticket), this.clock() + TICKET_TTL_MS);
    const pathname = target === "usage" ? "/usage" : target === "index" || !this.ownerSlug
      ? "/rudder" : `/rudder/${encodeURIComponent(this.ownerSlug)}`;
    return `${this.#origin}${pathname}#rudder-board-ticket=${ticket}`;
  }

  matchesOrigin(req: IncomingMessage): boolean {
    const host = req.headers.host;
    const expected = this.#origin && new URL(this.#origin).host;
    return !this.#stopped && Boolean(this.#origin) && typeof host === "string"
      && (host.toLowerCase() === expected || expected === "127.0.0.1" && host.toLowerCase() === "127.0.0.1:80")
      && (req.headers.origin === undefined || req.headers.origin === this.#origin);
  }

  exchange(req: IncomingMessage, ticket: unknown, instance: unknown): { token: string; session: BoardSessionInfo } | null {
    this.reap();
    if (!this.matchesOrigin(req) || instance !== this.instanceId || !secret(ticket)) return null;
    const key = digest(ticket);
    const expires = this.#tickets.get(key);
    if (expires === undefined || expires <= this.clock() || this.#sessions.size >= MAX_SESSIONS) return null;
    // Consume before any asynchronous work or response; a second exchange loses.
    this.#tickets.delete(key);
    const token = randomBytes(32).toString("hex");
    const session: BoardSession = { expiresAt: this.clock() + SESSION_TTL_MS, streams: new Set() };
    this.#sessions.set(digest(token), session);
    return { token, session: this.info(session) };
  }

  authenticate(req: IncomingMessage): BoardSession | null {
    this.reap();
    if (!this.matchesOrigin(req)) return null;
    const header = req.headers.authorization;
    if (typeof header !== "string" || !/^Bearer [a-f0-9]{64}$/.test(header)) return null;
    return this.#sessions.get(digest(header.slice(7))) ?? null;
  }

  info(session: BoardSession): BoardSessionInfo {
    return { ...this.descriptor(), expires_at_ms: session.expiresAt, owner_slug: this.ownerSlug,
      control_mode: this.controlMode, scope: { registered_projects: true, usage: true, owner_project_control: true } };
  }

  renew(session: BoardSession): BoardSessionInfo {
    if (!this.valid(session)) throw new Error("Board session expired");
    session.expiresAt = this.clock() + SESSION_TTL_MS;
    return this.info(session);
  }

  valid(session: BoardSession): boolean {
    return !this.#stopped && session.expiresAt > this.clock()
      && [...this.#sessions.values()].includes(session);
  }

  registerStream(session: BoardSession, close: () => void): (() => void) | null {
    const count = [...this.#sessions.values()].reduce((n, value) => n + value.streams.size, 0);
    if (!this.valid(session) || session.streams.size >= 4 || count >= MAX_STREAMS) return null;
    session.streams.add(close);
    return () => { session.streams.delete(close); };
  }

  reap(): void {
    const now = this.clock();
    for (const [key, until] of this.#tickets) if (until <= now) this.#tickets.delete(key);
    for (const [key, session] of this.#sessions) {
      if (session.expiresAt <= now) {
        this.#sessions.delete(key);
        for (const close of session.streams) close();
        session.streams.clear();
      }
    }
  }

  close(): void {
    this.#stopped = true;
    clearInterval(this.#timer);
    this.#tickets.clear();
    for (const session of this.#sessions.values()) for (const close of session.streams) close();
    this.#sessions.clear();
  }
}
