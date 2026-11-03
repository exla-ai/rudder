import { execFile } from "node:child_process";
import http from "node:http";
import https from "node:https";

type RecordValue = Record<string, unknown>;
const object = (value: unknown): RecordValue => value !== null && typeof value === "object" && !Array.isArray(value)
  ? value as RecordValue : {};

export type CloudPreview = {
  workspaceId: string;
  project: string;
  repo: string | null;
  port: number;
  label: string;
  url: string;
  access: "tailnet";
};

export function canonicalPreviewUrl(value: unknown, dnsName: unknown, port: unknown): string | null {
  if (typeof value !== "string" || typeof dnsName !== "string" || !Number.isInteger(port) || Number(port) < 1024 || Number(port) > 65535) return null;
  const host = dnsName.toLowerCase().replace(/\.$/, "");
  if (!/^[a-z0-9-]+\.[a-z0-9.-]+\.ts\.net$/.test(host)) return null;
  try {
    const url = new URL(value);
    if (!["http:", "https:"].includes(url.protocol) || url.hostname !== host || url.username || url.password
      || Number(url.port || (url.protocol === "https:" ? 443 : 80)) !== port
      || url.pathname !== "/" || url.search || url.hash) return null;
    return url.href;
  } catch { return null; }
}

/** Account-scoped home state is authoritative only while its worker is live.
 * A port in another project, or an arbitrary URL in an old/malformed worker
 * report, must never become a browser launch. */
export function cloudPreviews(value: unknown, project?: { repo?: string | null; name?: string }): CloudPreview[] {
  const response = object(value);
  const home = object(response.home);
  const tailnet = object(home.tailnet);
  if (home.connected !== true || home.stateLive === false || tailnet.online !== true) return [];
  const rows = Array.isArray(response.projects) ? response.projects : [];
  const previews: CloudPreview[] = [];
  for (const raw of rows) {
    const row = object(raw);
    if (row.live === false || typeof row.workspaceId !== "string" || typeof row.name !== "string") continue;
    const repo = typeof row.repo === "string" ? row.repo : null;
    if (project?.repo ? repo?.toLowerCase() !== project.repo.toLowerCase() : project?.name && row.name !== project.name) continue;
    for (const rawPort of Array.isArray(row.ports) ? row.ports : []) {
      const entry = object(rawPort);
      const url = canonicalPreviewUrl(entry.url, tailnet.dnsName, entry.port);
      if (!url) continue;
      previews.push({ workspaceId: row.workspaceId, project: row.name, repo, port: Number(entry.port),
        label: typeof entry.label === "string" ? entry.label.slice(0, 80) : "Preview", url, access: "tailnet" });
    }
  }
  return previews.sort((a, b) => a.port - b.port || a.project.localeCompare(b.project));
}

export function previewAccess(value: unknown, dnsName: string, deviceId?: string): { ok: boolean; error?: string } {
  const status = object(value);
  const self = object(status.Self);
  if (status.BackendState !== "Running" || self.Online !== true) {
    return { ok: false, error: "Connect Tailscale on this computer to open the private preview." };
  }
  const target = dnsName.toLowerCase().replace(/\.$/, "");
  const peers = [self, ...Object.values(object(status.Peer)).map(object)];
  const peer = peers.find((entry) => typeof entry.DNSName === "string" && entry.DNSName.toLowerCase().replace(/\.$/, "") === target);
  if (!peer) return { ok: false, error: "This Tailscale connection cannot see the cloud machine. Use its tailnet or an existing device share." };
  if (deviceId && peer.ID !== deviceId) return { ok: false, error: "The private preview belongs to a different device. Refresh project discovery." };
  if (peer.Online !== true) return { ok: false, error: "The cloud machine is offline on Tailscale. Retry once it reconnects." };
  return { ok: true };
}

export async function localTailnetStatus(options: { timeoutMs?: number; command?: string; socket?: string } = {}): Promise<unknown> {
  const deadline = performance.now() + Math.max(1, options.timeoutMs ?? 5000);
  const socket = options.socket ?? process.env.RUDDER_TAILSCALE_SOCKET ?? (process.env.RUDDER_CLOUD_RUNTIME === "home" ? "/tmp/tailscaled.sock" : undefined);
  const args = [...(socket ? ["--socket", socket] : []), "status", "--json"];
  const commands = options.command ? [options.command] : process.platform === "darwin"
    ? ["tailscale", "/Applications/Tailscale.app/Contents/MacOS/Tailscale"] : ["tailscale"];
  for (const command of commands) {
    const remaining = Math.floor(deadline - performance.now());
    if (remaining < 1) break;
    const result = await new Promise<{ stdout: string; missing: boolean }>((resolve) => {
      let settled = false;
      const finish = (stdout: string, missing = false) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve({ stdout, missing });
      };
      // A forked descendant can keep execFile's output pipes open after its
      // parent exits. Bound the promise independently of the close callback.
      const timer = setTimeout(() => {
        child.kill("SIGKILL");
        child.stdin?.destroy(); child.stdout?.destroy(); child.stderr?.destroy();
        finish("");
      }, remaining);
      const child = execFile(command, args, { timeout: remaining, killSignal: "SIGKILL", maxBuffer: 2 * 1024 * 1024, encoding: "utf8" }, (error, stdout) => {
        finish(error ? "" : stdout, (error as NodeJS.ErrnoException | null)?.code === "ENOENT");
      });
    });
    if (result.missing) continue;
    try { return JSON.parse(result.stdout); } catch { break; }
  }
  return null;
}

export function previewReachable(url: string, timeoutMs = 3000): Promise<boolean> {
  return new Promise((resolve) => {
    let request: http.ClientRequest | undefined;
    let settled = false;
    const finish = (ok: boolean) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      request?.destroy();
      resolve(ok);
    };
    // A TCP accept does not establish an ACL-capable HTTP route. A total
    // deadline also bounds DNS, TLS and peers that dribble partial headers.
    const timer = setTimeout(() => finish(false), Math.max(1, timeoutMs));
    try {
      const target = new URL(url);
      if (!["http:", "https:"].includes(target.protocol) || target.username || target.password) return finish(false);
      request = (target.protocol === "https:" ? https : http).request(target, { method: "HEAD" }, (response) => {
        response.destroy();
        finish(true);
      });
      request.once("error", () => finish(false));
      request.end();
    } catch { finish(false); }
  });
}
