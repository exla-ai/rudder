type Descriptor = { format_version: 1; instance_id: string; origin: string };
export type BoardSessionInfo = Descriptor & {
  expires_at_ms: number;
  owner_slug: string;
  control_mode: "projector" | "scheduler";
  scope: { registered_projects: true; usage: true; owner_project_control: true };
};
type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem" | "key" | "length">;
type Environment = {
  origin: string;
  href: string;
  replaceUrl: (value: string) => void;
  storage?: StorageLike;
  fetch: typeof fetch;
  now?: () => number;
};
const PREFIX = "rudder.board.session.v1:";
const TOKEN = /^[a-f0-9]{64}$/;
const UUID = /^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/;

export class BoardAccessExpired extends Error {
  constructor() { super("Open this board from Rudder to connect"); this.name = "BoardAccessExpired"; }
}

/** The bearer is private to this module and this browser origin/daemon instance. */
export class BrowserBoardAccess {
  #token = "";
  #info: BoardSessionInfo | null = null;
  #key = "";
  #timer: ReturnType<typeof setTimeout> | undefined;
  #renewing: { epoch: number; promise: Promise<void> } | undefined;
  #listeners = new Set<() => void>();
  #epoch = 0;
  #now: () => number;
  #env: Omit<Environment, "href" | "now">;
  #href: string;

  constructor(env: Environment) {
    const { href, now, ...rest } = env;
    this.#href = href; this.#env = rest; this.#now = now ?? Date.now;
  }

  onLost(callback: () => void): () => void { this.#listeners.add(callback); return () => { this.#listeners.delete(callback); }; }

  #clear(notify = true): void {
    this.#epoch++;
    this.#token = ""; this.#info = null;
    this.#renewing = undefined;
    clearTimeout(this.#timer);
    try { if (this.#key) this.#env.storage?.removeItem(this.#key); } catch { /* In-memory sessions still work. */ }
    if (notify) for (const listener of this.#listeners) listener();
  }

  #session(value: unknown, descriptor: Descriptor): BoardSessionInfo {
    const info = value as BoardSessionInfo | null;
    if (!info || info.format_version !== 1 || info.origin !== this.#env.origin || info.instance_id !== descriptor.instance_id
      || !Number.isSafeInteger(info.expires_at_ms) || info.expires_at_ms <= this.#now()
      || typeof info.owner_slug !== "string" || !["projector", "scheduler"].includes(info.control_mode)
      || info.scope?.registered_projects !== true || info.scope.usage !== true || info.scope.owner_project_control !== true) throw new BoardAccessExpired();
    return info;
  }

  #save(): void {
    try { this.#env.storage?.setItem(this.#key, JSON.stringify({ token: this.#token, session: this.#info })); } catch { /* Memory remains authoritative. */ }
    clearTimeout(this.#timer);
    const epoch = this.#epoch;
    if (this.#info) this.#timer = setTimeout(() => { void this.#renew().catch(() => { if (epoch === this.#epoch) this.#clear(); }); },
      Math.max(1000, this.#info.expires_at_ms - this.#now() - 60_000));
  }

  #request(input: string, init?: RequestInit): Promise<Response> {
    const url = new URL(input, this.#env.origin);
    if (url.origin !== this.#env.origin) return Promise.reject(new BoardAccessExpired());
    return this.#env.fetch(url.href, { ...init, credentials: "omit", cache: "no-store", redirect: "error", referrerPolicy: "no-referrer" });
  }

  async initialize(): Promise<BoardSessionInfo> {
    this.#clear(false);
    const epoch = this.#epoch;
    const location = new URL(this.#href);
    const ticket = /^#rudder-board-ticket=([a-f0-9]{64})$/.exec(location.hash)?.[1];
    // Erase before the first await, including malformed/expired ticket fragments.
    if (location.hash.startsWith("#rudder-board-ticket")) {
      this.#env.replaceUrl(location.pathname + location.search);
      this.#href = location.origin + location.pathname + location.search;
    }
    try {
      const response = await this.#request("/api/board/descriptor");
      const descriptor = await response.json() as Descriptor;
      if (epoch !== this.#epoch) throw new BoardAccessExpired();
      if (!response.ok || descriptor.format_version !== 1 || descriptor.origin !== this.#env.origin || !UUID.test(descriptor.instance_id)) throw new BoardAccessExpired();
      this.#key = `${PREFIX}${this.#env.origin}:${descriptor.instance_id}`;
      try {
        const storage = this.#env.storage;
        for (let i = (storage?.length ?? 0) - 1; i >= 0; i--) {
          const key = storage!.key(i);
          if (key?.startsWith(`${PREFIX}${this.#env.origin}:`) && key !== this.#key) storage!.removeItem(key);
        }
      } catch { /* Private browsing may deny storage. */ }
      if (ticket) {
        const exchanged = await this.#request("/api/board/session", { method: "POST", headers: { "content-type": "application/json" },
          body: JSON.stringify({ ticket, instance_id: descriptor.instance_id }) });
        if (!exchanged.ok) throw new BoardAccessExpired();
        const value = await exchanged.json();
        if (epoch !== this.#epoch) throw new BoardAccessExpired();
        if (!TOKEN.test(value.token)) throw new BoardAccessExpired();
        this.#token = value.token;
        this.#info = this.#session(value.session, descriptor);
      } else {
        let stored: { token?: unknown; session?: unknown } | null = null;
        try { stored = JSON.parse(this.#env.storage?.getItem(this.#key) ?? "null"); } catch { /* Ignore corrupt saved state. */ }
        if (!stored || typeof stored.token !== "string" || !TOKEN.test(stored.token)) throw new BoardAccessExpired();
        this.#token = stored.token;
        this.#session(stored.session, descriptor);
        const checked = await this.#request("/api/board/session", { headers: { Authorization: `Bearer ${this.#token}` } });
        if (!checked.ok) throw new BoardAccessExpired();
        const info = await checked.json();
        if (epoch !== this.#epoch) throw new BoardAccessExpired();
        this.#info = this.#session(info, descriptor);
      }
      this.#save();
      return this.#info;
    } catch { if (epoch === this.#epoch) this.#clear(); throw new BoardAccessExpired(); }
  }

  async #renew(): Promise<void> {
    if (this.#renewing?.epoch === this.#epoch) return this.#renewing.promise;
    const epoch = this.#epoch;
    const renewal = { epoch, promise: Promise.resolve() };
    renewal.promise = (async () => {
      if (!this.#token || !this.#info || this.#info.expires_at_ms <= this.#now()) throw new BoardAccessExpired();
      const response = await this.#request("/api/board/session/renew", { method: "POST", headers: { Authorization: `Bearer ${this.#token}` } });
      if (!response.ok) throw new BoardAccessExpired();
      const info = await response.json();
      if (epoch !== this.#epoch || !this.#info) throw new BoardAccessExpired();
      this.#info = this.#session(info, this.#info);
      this.#save();
    })().finally(() => { if (this.#renewing === renewal) this.#renewing = undefined; });
    this.#renewing = renewal;
    return renewal.promise;
  }

  async fetch(input: string, init?: RequestInit): Promise<Response> {
    if (!this.#token || !this.#info) throw new BoardAccessExpired();
    const epoch = this.#epoch;
    if (this.#info.expires_at_ms <= this.#now() + 60_000) {
      try { await this.#renew(); } catch { if (epoch === this.#epoch) this.#clear(); throw new BoardAccessExpired(); }
    }
    if (epoch !== this.#epoch) throw new BoardAccessExpired();
    const headers = new Headers(init?.headers);
    headers.set("Authorization", `Bearer ${this.#token}`);
    const response = await this.#request(input, { ...init, headers });
    if (epoch !== this.#epoch) { await response.body?.cancel(); throw new BoardAccessExpired(); }
    if (response.status === 401) { this.#clear(); throw new BoardAccessExpired(); }
    return response;
  }

  close(): void { this.#clear(false); this.#listeners.clear(); }
}

let browser: BrowserBoardAccess | undefined;
export function boardAccess(): BrowserBoardAccess {
  if (!browser) {
    let storage: Storage | undefined;
    try { storage = window.sessionStorage; } catch { /* Memory-only session. */ }
    browser = new BrowserBoardAccess({ origin: window.location.origin, href: window.location.href, storage,
      replaceUrl: (value) => window.history.replaceState(null, "", value), fetch: window.fetch.bind(window) });
  }
  return browser;
}
export function boardFetch(input: string, init?: RequestInit): Promise<Response> { return boardAccess().fetch(input, init); }
