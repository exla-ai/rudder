import { execFile } from "node:child_process";
import { readFile, readlink, readdir, realpath } from "node:fs/promises";
import path from "node:path";
import { createHash } from "node:crypto";
import { canonicalPreviewUrl, previewAccess, previewReachable } from "../tailscale-preview.js";

export type PreviewBinding = {
  tenantId: string;
  projectId: string;
  workspaceId: string;
  attemptId: string;
  workspacePath: string;
};
export type PreviewListener = PreviewBinding & {
  pid: number;
  processStart: string;
  port: number;
  protocol: "http" | "https";
  origin: string;
  role: "preview";
};
export type PrivatePreview = PreviewListener & {
  id: string;
  deviceId: string;
  dnsName: string;
  url: string;
  ready: true;
  observedAt: number;
};
export type TailnetCommandResult = { code: number; stdout: string };
export type TailnetCommand = (args: string[], signal: AbortSignal) => Promise<TailnetCommandResult>;
export type PreviewTrace = { correlationId: string; stage: string; at: number };
const sameBinding = (a: PreviewBinding, b: PreviewBinding) => a.tenantId === b.tenantId && a.projectId === b.projectId
  && a.workspaceId === b.workspaceId && a.attemptId === b.attemptId;
const nonempty = (value: unknown): value is string => typeof value === "string" && value.length > 0 && !value.includes("\0");
const identity = (item: PreviewListener, deviceId: string) => createHash("sha256").update(JSON.stringify([
  item.tenantId, item.projectId, item.workspaceId, item.attemptId, item.workspacePath,
  item.pid, item.processStart, item.port, item.protocol, item.origin, deviceId,
])).digest("hex");
const object = (value: unknown): Record<string, unknown> => value && typeof value === "object" && !Array.isArray(value)
  ? value as Record<string, unknown> : {};
// Serve's TCP/Web keys name configured services; Foreground configs nest the
// same structures. Treat unfamiliar nesting conservatively, without deleting
// or overwriting a port that was not journaled by this directory.
function configuredPorts(value: unknown, depth = 0): Set<number> {
  const ports = new Set<number>();
  if (depth > 8) return ports;
  for (const [key, nested] of Object.entries(object(value))) {
    const match = /(?:^|:)(\d+)$/.exec(key);
    if (match) ports.add(Number(match[1]));
    for (const port of configuredPorts(nested, depth + 1)) ports.add(port);
  }
  return ports;
}
function funnelPorts(value: unknown, depth = 0): Set<number> {
  const ports = new Set<number>();
  if (depth > 8) return ports;
  for (const [key, nested] of Object.entries(object(value))) {
    if (key === "AllowFunnel") {
      for (const [service, enabled] of Object.entries(object(nested))) {
        if (enabled === true) for (const port of configuredPorts({ [service]: true })) ports.add(port);
      }
    } else for (const port of funnelPorts(nested, depth + 1)) ports.add(port);
  }
  return ports;
}
function forwardsToListener(value: unknown, item: PreviewListener, dnsName: string): boolean {
  let matches = 0;
  let mismatch = false;
  const visit = (config: unknown, depth: number) => {
    if (depth > 8) { mismatch = true; return; }
    const record = object(config);
    const tcp = object(object(record.TCP)[String(item.port)]);
    if (Object.keys(tcp).some((key) => !["HTTP", "HTTPS"].includes(key))) mismatch = true;
    for (const [service, entry] of Object.entries(object(record.Web))) {
      if (!service.endsWith(`:${item.port}`)) continue;
      const handlers = object(object(entry).Handlers);
      const target = object(handlers["/"]).Proxy;
      let sameOrigin = false;
      try { sameOrigin = typeof target === "string" && new URL(target).href === item.origin; } catch { /* mismatch */ }
      if (service !== `${dnsName}:${item.port}` || Object.keys(handlers).length !== 1 || !sameOrigin) mismatch = true;
      else matches++;
    }
    for (const [key, nested] of Object.entries(record)) {
      if (!["TCP", "Web", "AllowFunnel"].includes(key) && nested && typeof nested === "object") visit(nested, depth + 1);
    }
  };
  visit(value, 0);
  return matches > 0 && !mismatch;
}

/** No login/up/auth-key command: persisted tailscaled identity remains authoritative. */
export const executeTailnetCommand: TailnetCommand = (args, signal) => new Promise((resolve) => {
  const socket = process.env.RUDDER_TAILSCALE_SOCKET;
  execFile("tailscale", [...(socket ? ["--socket", socket] : []), ...args], {
    signal, killSignal: "SIGKILL", timeout: 5000, maxBuffer: 2 * 1024 * 1024, encoding: "utf8",
  }, (error, stdout) => resolve({ code: error ? -1 : 0, stdout: error ? "" : stdout }));
});

/** Bounds dependencies too, including a fake/hung backend that ignores abort. */
async function bounded<T>(run: (signal: AbortSignal) => Promise<T>, timeoutMs: number): Promise<T | undefined> {
  const controller = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      Promise.resolve().then(() => run(controller.signal)).catch(() => undefined),
      new Promise<undefined>((resolve) => { timer = setTimeout(() => { controller.abort(); resolve(undefined); }, timeoutMs); }),
    ]);
  } finally { if (timer) clearTimeout(timer); }
}

export function loopbackPreviewOrigin(host: string, port: number, protocol: "http" | "https" = "http"): string | null {
  if (!Number.isInteger(port) || port < 1024 || port > 65535 || port === 41641) return null;
  if (host === "::" || host === "::1" || host === "[::1]") return `${protocol}://[::1]:${port}/`;
  if (host === "0.0.0.0") host = "127.0.0.1";
  if (!/^127(?:\.(?:0|[1-9]\d{0,2})){3}$/.test(host) || host.split(".").some((part) => Number(part) > 255)) return null;
  return `${protocol}://${host}:${port}/`;
}

function validListener(item: PreviewListener): boolean {
  try {
    const origin = new URL(item.origin);
    return item.role === "preview" && [item.tenantId, item.projectId, item.workspaceId, item.attemptId, item.processStart].every(nonempty)
      && path.isAbsolute(item.workspacePath) && !item.workspacePath.includes("\0")
      && Number.isSafeInteger(item.pid) && item.pid > 0 && ["http", "https"].includes(item.protocol)
      && loopbackPreviewOrigin(origin.hostname, item.port, item.protocol) === item.origin;
  } catch { return false; }
}

export async function processStartToken(pid: number, procRoot = "/proc"): Promise<string | null> {
  try {
    const stat = await readFile(path.join(procRoot, String(pid), "stat"), "utf8");
    // comm can contain spaces and parentheses. Field 22 follows the last ')'.
    const end = stat.lastIndexOf(")");
    const token = stat.slice(end + 2).trim().split(/\s+/)[19];
    return end > 0 && /^\d+$/.test(token ?? "") ? token : null;
  } catch { return null; }
}

export async function previewProcessMatches(item: PreviewListener, procRoot = "/proc"): Promise<boolean> {
  const observed = await observeLinuxPreviewListeners([item], { procRoot });
  return observed.some((listener) => listener.port === item.port && listener.origin === item.origin
    && listener.protocol === item.protocol && listener.processStart === item.processStart);
}

/** Read only engine-registered preview processes, never all machine listeners.
 * The spawn record supplies the owner attempt and process-start token. */
export async function observeLinuxPreviewListeners(processes: Array<PreviewBinding & {
  pid: number; processStart: string; role: string; protocol?: "http" | "https";
}>, options: { procRoot?: string; maxProcesses?: number; maxListeners?: number } = {}): Promise<PreviewListener[]> {
  const procRoot = options.procRoot ?? "/proc";
  if (processes.length > (options.maxProcesses ?? 128)) return [];
  const output: PreviewListener[] = [];
  for (const process of processes.slice(0, options.maxProcesses ?? 128)) {
    if (process.role !== "preview" || await processStartToken(process.pid, procRoot) !== process.processStart) continue;
    try {
      const root = await realpath(process.workspacePath);
      const cwd = await realpath(path.join(procRoot, String(process.pid), "cwd"));
      const relative = path.relative(root, cwd);
      if (relative === ".." || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) continue;
      const fdRoot = path.join(procRoot, String(process.pid), "fd");
      const inodes = new Set<string>();
      for (const fd of (await readdir(fdRoot)).slice(0, 4096)) {
        const link = await readlink(path.join(fdRoot, fd)).catch(() => "");
        const match = /^socket:\[(\d+)\]$/.exec(link);
        if (match) inodes.add(match[1]);
      }
      for (const family of [4, 6]) {
        const table = await readFile(path.join(procRoot, String(process.pid), "net", family === 4 ? "tcp" : "tcp6"), "utf8").catch(() => "");
        for (const row of table.trim().split("\n").slice(1)) {
          const fields = row.trim().split(/\s+/);
          if (fields[3] !== "0A" || !inodes.has(fields[9])) continue;
          const [address, hexPort] = fields[1].split(":");
          let host = "";
          if (family === 4 && /^[0-9A-Fa-f]{8}$/.test(address)) host = address.match(/../g)!.reverse().map((byte) => parseInt(byte, 16)).join(".");
          if (family === 6 && /^000000000000000000000000(?:00000000|01000000)$/.test(address)) host = "::1";
          const port = parseInt(hexPort, 16);
          const protocol = process.protocol ?? "http";
          const origin = loopbackPreviewOrigin(host, port, protocol);
          if (origin && await processStartToken(process.pid, procRoot) === process.processStart) {
            output.push({ ...process, role: "preview", protocol, origin, port });
            if (output.length > (options.maxListeners ?? 64)) return [];
          }
        }
      }
    } catch { /* An exited process or inaccessible cwd is not an observation. */ }
  }
  return output;
}

/** Project ownership and route readiness are independent from network reachability.
 * This directory is ephemeral; persist only its owned-route journal through the
 * existing JSON store, and repopulate from fresh process observations on boot. */
export class PrivatePreviewDirectory {
  private readonly routes = new Map<number, PrivatePreview>();
  private readonly owned: Set<number>;
  private readonly claimedTargets = new Map<number, { item: PreviewListener; dnsName: string }>();
  private readonly uncertain = new Map<number, Promise<unknown>>();
  private readonly unsettled = new Set<number>();
  private desired = new Map<number, PreviewListener>();
  private revision = 0;
  private queue: Promise<void> = Promise.resolve();
  private online = false;
  readonly timeoutMs: number;
  constructor(private readonly options: {
    execute?: TailnetCommand;
    probe?: (url: string, timeoutMs: number) => Promise<boolean>;
    processMatches?: (item: PreviewListener) => Promise<boolean>;
    now?: () => number;
    timeoutMs?: number;
    maxAgeMs?: number;
    maxListeners?: number;
    allowPrivateHttp?: boolean;
    ownedPorts?: number[];
    onOwnedPorts?: (ports: number[]) => void;
    trace?: (event: PreviewTrace) => void;
  } = {}) {
    this.timeoutMs = Math.max(1, options.timeoutMs ?? 1500);
    this.owned = new Set((options.ownedPorts ?? []).filter((port) => Number.isInteger(port) && port >= 1024 && port <= 65535));
  }
  private now() { return (this.options.now ?? (() => performance.now()))(); }
  private trace(stage: string, correlationId: string) { this.options.trace?.({ stage, correlationId, at: this.now() }); }
  private journal() { this.options.onOwnedPorts?.([...this.owned]); }
  private current(item: PreviewListener, revision: number) { return this.revision === revision && this.desired.get(item.port) === item; }
  private async processMatches(item: PreviewListener) {
    return await bounded(() => this.options.processMatches ? this.options.processMatches(item)
      : previewProcessMatches(item), this.timeoutMs) === true;
  }
  private async command(args: string[], port?: number): Promise<TailnetCommandResult | undefined> {
    if (port !== undefined && this.unsettled.has(port)) return undefined;
    let underlying: Promise<TailnetCommandResult> | undefined;
    const result = await bounded((signal) => {
      underlying = Promise.resolve().then(() => (this.options.execute ?? executeTailnetCommand)(args, signal));
      if (port !== undefined) {
        this.unsettled.add(port);
        const pending = underlying.finally(() => { this.unsettled.delete(port); }).catch(() => {});
        this.uncertain.set(port, pending);
      }
      return underlying;
    }, this.timeoutMs);
    if (port !== undefined && result) this.uncertain.delete(port);
    return result;
  }
  private async retire(port: number): Promise<boolean> {
    this.routes.delete(port);
    if (this.unsettled.has(port)) return false;
    const secure = await this.command(["serve", `--https=${port}`, "off"], port);
    if (!secure || secure.code !== 0) return false;
    const plain = await this.command(["serve", `--http=${port}`, "off"], port);
    if (!plain || plain.code !== 0) return false;
    this.owned.delete(port); this.claimedTargets.delete(port); this.uncertain.delete(port); this.journal();
    return true;
  }
  snapshot(binding?: PreviewBinding): PrivatePreview[] {
    if (!this.online) return [];
    const now = this.now();
    return [...this.routes.values()].filter((item) => now >= item.observedAt && now - item.observedAt <= (this.options.maxAgeMs ?? 10_000)
      && this.desired.has(item.port) && (!binding || sameBinding(item, binding))).map((item) => ({ ...item }));
  }
  reconcile(listeners: PreviewListener[]): Promise<void> {
    const next = new Map<number, PreviewListener>();
    const collisions = new Set<number>();
    for (const item of listeners.length <= (this.options.maxListeners ?? 64) ? listeners : []) {
      if (!validListener(item)) continue;
      // A private Serve port has one target. Ambiguous targets fail closed.
      if (next.has(item.port) && identity(next.get(item.port)!, "") !== identity(item, "")) collisions.add(item.port);
      else next.set(item.port, { ...item });
    }
    for (const port of collisions) next.delete(port);
    this.desired = next;
    const revision = ++this.revision;
    for (const [port, item] of this.routes) {
      const target = next.get(port);
      if (!target || identity(target, item.deviceId) !== item.id) this.routes.delete(port);
    }
    const update = () => this.update(revision);
    const result = this.queue.then(update, update);
    this.queue = result.catch(() => {});
    return result;
  }
  private async update(revision: number) {
    const status = await this.command(["status", "--json"]);
    let value: Record<string, unknown> = {};
    try { value = object(JSON.parse(status?.stdout ?? "")); } catch { /* offline */ }
    if (this.revision !== revision) return;
    const self = object(value.Self);
    const dnsName = typeof self.DNSName === "string" ? self.DNSName.toLowerCase().replace(/\.$/, "") : "";
    const deviceId = nonempty(self.ID) ? self.ID : "";
    this.online = value.BackendState === "Running" && self.Online === true && !!deviceId
      && canonicalPreviewUrl(`https://${dnsName}:3000/`, dnsName, 3000) !== null;
    if (!this.online) { this.routes.clear(); return; }
    const configuration = await this.command(["serve", "status", "--json"]);
    let serve: Record<string, unknown>;
    try {
      const decoded: unknown = JSON.parse(configuration?.code === 0 ? configuration.stdout : "");
      if (decoded === null || typeof decoded !== "object" || Array.isArray(decoded)) throw new Error("invalid config");
      serve = object(decoded);
    } catch { this.routes.clear(); return; }
    if (this.revision !== revision) return;
    const existing = configuredPorts(serve);
    const publicPorts = funnelPorts(serve);
    // Retire old journal entries and uncertain effects before any publication.
    const blocked = new Set<number>();
    for (const port of new Set([...existing, ...publicPorts])) {
      const previous = this.routes.get(port);
      const target = previous ? { item: previous, dnsName: previous.dnsName } : this.claimedTargets.get(port);
      if (!this.owned.has(port) || publicPorts.has(port) || (existing.has(port)
        && (!target || !forwardsToListener(serve, target.item, target.dnsName)))) {
        blocked.add(port); this.routes.delete(port);
        // A journaled number alone cannot authorize deleting a service that
        // somebody else has repointed. Relinquish it and preserve that config.
        this.claimedTargets.delete(port);
        if (this.owned.delete(port)) this.journal();
      }
    }
    for (const port of this.routes.keys()) if (!existing.has(port)) this.routes.delete(port);
    for (const port of this.owned) {
      const route = this.routes.get(port);
      const desired = this.desired.get(port);
      if (!route || !desired || identity(desired, deviceId) !== route.id || this.uncertain.has(port)) {
        if (publicPorts.has(port)) { this.routes.delete(port); continue; }
        if (!(await this.retire(port))) blocked.add(port);
      }
    }
    for (const item of this.desired.values()) {
      const id = identity(item, deviceId);
      if (!this.current(item, revision)) return;
      if (blocked.has(item.port)) continue;
      this.trace("preview-probe", id);
      const alive = await this.processMatches(item);
      const ready = alive && await bounded(() => (this.options.probe ?? previewReachable)(item.origin, this.timeoutMs), this.timeoutMs) === true;
      if (!this.current(item, revision)) return;
      if (!ready) { if (this.owned.has(item.port)) await this.retire(item.port); continue; }
      if (this.unsettled.has(item.port) || this.uncertain.has(item.port)) continue;
      let url = this.routes.get(item.port)?.id === id ? this.routes.get(item.port)!.url : undefined;
      if (!url) {
        // Journal before the effect. A timeout can have installed a route.
        this.owned.add(item.port); this.journal();
        this.claimedTargets.set(item.port, { item, dnsName });
        let result = await this.command(["serve", "--bg", `--https=${item.port}`, item.origin], item.port);
        if (result?.code === 0) url = `https://${dnsName}:${item.port}/`;
        else if (result && this.options.allowPrivateHttp === true) {
          result = await this.command(["serve", "--bg", `--http=${item.port}`, item.origin], item.port);
          if (result?.code === 0) url = `http://${dnsName}:${item.port}/`;
        }
        if (url) {
          const confirmed = await this.command(["serve", "status", "--json"]);
          let config: unknown;
          try { config = confirmed?.code === 0 ? JSON.parse(confirmed.stdout) : undefined; } catch { /* not confirmed */ }
          if (!forwardsToListener(config, item, dnsName) || funnelPorts(config).has(item.port)) {
            this.routes.delete(item.port);
            this.owned.delete(item.port); this.claimedTargets.delete(item.port); this.journal();
            continue;
          }
        }
      }
      const privateReady = url && await bounded(() => (this.options.probe ?? previewReachable)(url!, this.timeoutMs), this.timeoutMs) === true;
      if (!this.current(item, revision) || !(await this.processMatches(item)) || !privateReady) {
        await this.retire(item.port); continue;
      }
      if (url) {
        this.routes.set(item.port, { ...item, id, deviceId, dnsName, url, ready: true, observedAt: this.now() });
        this.trace("preview-ready", id);
      }
    }
  }
  /** Resolve from the live directory after tenant/project authorization. Caller
   * invokes its local browser only with this returned URL. Never accepts a URL
   * supplied by a user or a terminal byte stream. */
  async open(id: string, binding: PreviewBinding, options: {
    authorize: (binding: PreviewBinding) => Promise<boolean>;
    localStatus: () => Promise<unknown>;
    probe?: (url: string, timeoutMs: number) => Promise<boolean>;
    relay?: (preview: PrivatePreview) => Promise<string | null>;
  }): Promise<{ url: string; access: "tailnet" | "relay" }> {
    if (await bounded(() => options.authorize(binding), this.timeoutMs) !== true) throw new Error("Private preview access denied.");
    const find = () => this.snapshot(binding).find((item) => item.id === id);
    const preview = find();
    if (!preview) throw new Error("The project preview is no longer ready. Refresh discovery.");
    const status = await bounded(() => options.localStatus(), this.timeoutMs);
    const direct = previewAccess(status, preview.dnsName, preview.deviceId).ok
      && await bounded(() => (options.probe ?? previewReachable)(preview.url, this.timeoutMs), this.timeoutMs) === true;
    let url: string | undefined = direct ? preview.url : undefined;
    if (!url && options.relay) {
      const relay = await bounded(() => options.relay!(preview), this.timeoutMs);
      if (relay) {
        const parsed = new URL(relay);
        if (parsed.protocol === "https:" && !parsed.username && !parsed.password) url = parsed.href;
      }
    }
    if (!url || !find() || !(await this.processMatches(preview))) throw new Error("The authorized project preview is unreachable. Retry discovery.");
    // Revalidate ACL after probes: revocation during a slow connection must not
    // turn a formerly authorized reference into a browser launch.
    if (await bounded(() => options.authorize(binding), this.timeoutMs) !== true || !find()) throw new Error("Private preview access denied.");
    this.trace("preview-open", id);
    return { url, access: direct ? "tailnet" : "relay" };
  }
}
