import { randomUUID } from "node:crypto";
import { performance } from "node:perf_hooks";

export interface HomeRpcTiming {
  now(): number;
  setTimeout(callback: () => void, timeoutMs: number): unknown;
  clearTimeout(timer: unknown): void;
}
type UnknownReason = "replaced" | "closed" | "timeout" | "send_failed";
export class HomeRpcEffectUnknownError extends Error {
  readonly code = "HOME_RPC_EFFECT_UNKNOWN";
  readonly effect = "unknown";
  readonly status: number;
  constructor(readonly reason: UnknownReason, method: string) {
    super(reason === "timeout"
      ? `home machine did not answer ${method} in time; effect unknown; request was not replayed`
      : `home control ${reason}; ${method} effect unknown; request was not replayed`);
    this.name = "HomeRpcEffectUnknownError";
    this.status = reason === "timeout" ? 504 : 503;
  }
}
class Session<S extends object, V> {
  active = true;
  readonly pending = new Map<string, Pending<S, V>>();
  constructor(readonly homeId: string, readonly socket: S) {}
}
type Pending<S extends object, V> = {
  session: Session<S, V>;
  id: string;
  method: string;
  deadline: number;
  timer?: unknown;
  resolve: (value: V) => void;
  reject: (error: Error) => void;
};
type Transport<S> = { isOpen(socket: S): boolean; send(socket: S, data: string): boolean };
const timing: HomeRpcTiming = {
  now: () => performance.now(),
  setTimeout: (callback, ms) => setTimeout(callback, ms),
  clearTimeout: (timer) => clearTimeout(timer as ReturnType<typeof setTimeout>),
};

/** Process-local communication ownership only. attach() follows the existing
 * worker-bearer check; it authenticates no Machine, boot, NET or native grant.
 * Each actual socket attachment gets a fresh non-wire session identity. */
export class HomeControlSessions<S extends object, V> {
  readonly #current = new Map<string, Session<S, V>>();
  readonly #sockets = new WeakMap<S, Session<S, V>>();
  readonly #timing: HomeRpcTiming;
  readonly #id: () => string;
  constructor(readonly transport: Transport<S>, options: { timing?: HomeRpcTiming; id?: () => string } = {}) {
    this.#timing = options.timing ?? timing;
    this.#id = options.id ?? randomUUID;
  }
  attach(homeId: string, socket: S): Session<S, V> {
    const previous = this.#current.get(homeId);
    if (previous) this.close(previous, "replaced");
    const priorSocket = this.#sockets.get(socket);
    if (priorSocket) this.close(priorSocket, "replaced");
    const session = new Session<S, V>(homeId, socket);
    this.#current.set(homeId, session);
    this.#sockets.set(socket, session);
    return session;
  }
  isCurrent(session: Session<S, V>): boolean {
    return session.active && this.#current.get(session.homeId) === session
      && this.#sockets.get(session.socket) === session && this.transport.isOpen(session.socket);
  }
  close(session: Session<S, V>, reason: "closed" | "replaced" = "closed"): void {
    session.active = false;
    if (this.#current.get(session.homeId) === session) this.#current.delete(session.homeId);
    if (this.#sockets.get(session.socket) === session) this.#sockets.delete(session.socket);
    for (const pending of session.pending.values()) this.#fail(pending, reason);
  }
  closeCurrent(homeId: string, socket: S): void {
    const session = this.#current.get(homeId);
    if (session?.socket === socket) this.close(session);
  }
  request(homeId: string, socket: S, method: string, params: unknown, timeoutMs: number): Promise<V> {
    const session = this.#current.get(homeId);
    if (!session || session.socket !== socket || !this.isCurrent(session)) {
      return Promise.reject(Object.assign(new Error("the cloud machine is not connected yet; it may still be starting"), { status: 503 }));
    }
    const id = this.#id();
    if (session.pending.has(id)) {
      return Promise.reject(Object.assign(new Error("home RPC request identity is already active"), { status: 503 }));
    }
    // Capture before scheduling/sending, retaining the original caller timeout.
    const deadline = this.#timing.now() + timeoutMs;
    return new Promise<V>((resolve, reject) => {
      const pending: Pending<S, V> = { session, id, method, deadline, resolve, reject };
      session.pending.set(id, pending);
      try {
        pending.timer = this.#timing.setTimeout(() => this.#fail(pending, "timeout"), timeoutMs);
        // A synchronous test timer/transport may already have retired this entry.
        if (session.pending.get(id) !== pending) {
          this.#timing.clearTimeout(pending.timer);
          return;
        }
        const wire = JSON.stringify({ type: "rpc", id, method, params });
        // Serialization can consume the remaining budget or synchronously
        // close/replace this session. Recheck its original owner and deadline
        // after that work, immediately before dispatch; never renew the timer.
        if (this.#timing.now() >= pending.deadline) {
          this.#fail(pending, "timeout");
          return;
        }
        if (!this.isCurrent(session) || session.pending.get(id) !== pending
          || !this.transport.send(socket, wire)) {
          this.#fail(pending, "send_failed");
        }
      } catch {
        this.#fail(pending, "send_failed");
      }
    });
  }
  reply(session: Session<S, V>, id: string, result: V, error?: string): boolean {
    if (!this.isCurrent(session)) return false;
    const pending = session.pending.get(id);
    if (!pending || pending.session !== session) return false;
    // A queued timer can run late. The reply cannot acquire a new deadline
    // merely by arriving before that delayed timer callback is dispatched.
    if (this.#timing.now() >= pending.deadline) { this.#fail(pending, "timeout"); return false; }
    if (!this.#take(pending)) return false;
    if (error) pending.reject(Object.assign(new Error(error), { status: 400 }));
    else pending.resolve(result);
    return true;
  }
  #take(pending: Pending<S, V>): boolean {
    // Identity, not ID alone: an already-queued old timeout must not remove a
    // colliding ID on a new socket generation or another authenticated home.
    if (pending.session.pending.get(pending.id) !== pending) return false;
    pending.session.pending.delete(pending.id);
    if (pending.timer !== undefined) this.#timing.clearTimeout(pending.timer);
    return true;
  }
  #fail(pending: Pending<S, V>, reason: UnknownReason): boolean {
    if (!this.#take(pending)) return false;
    pending.reject(new HomeRpcEffectUnknownError(reason, pending.method));
    return true;
  }
}
