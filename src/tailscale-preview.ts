import { execFile } from "node:child_process";
import net from "node:net";

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

export function previewAccess(value: unknown, dnsName: string): { ok: boolean; error?: string } {
  const status = object(value);
  const self = object(status.Self);
  if (status.BackendState !== "Running" || self.Online !== true) {
    return { ok: false, error: "Connect Tailscale on this computer to open the private preview." };
  }
  const target = dnsName.toLowerCase().replace(/\.$/, "");
  const peers = [self, ...Object.values(object(status.Peer)).map(object)];
  const peer = peers.find((entry) => typeof entry.DNSName === "string" && entry.DNSName.toLowerCase().replace(/\.$/, "") === target);
  if (!peer) return { ok: false, error: "This Tailscale connection cannot see the cloud machine. Use its tailnet or an existing device share." };
  if (peer.Online !== true) return { ok: false, error: "The cloud machine is offline on Tailscale. Retry once it reconnects." };
  return { ok: true };
}

export async function localTailnetStatus(): Promise<unknown> {
  const socket = process.env.RUDDER_TAILSCALE_SOCKET || (process.env.RUDDER_CLOUD_RUNTIME === "home" ? "/tmp/tailscaled.sock" : undefined);
  const args = [...(socket ? ["--socket", socket] : []), "status", "--json"];
  const commands = process.platform === "darwin"
    ? ["tailscale", "/Applications/Tailscale.app/Contents/MacOS/Tailscale"] : ["tailscale"];
  for (const command of commands) {
    const result = await new Promise<{ stdout: string; missing: boolean }>((resolve) => {
      execFile(command, args, { timeout: 5000, maxBuffer: 2 * 1024 * 1024, encoding: "utf8" }, (error, stdout) => {
        resolve({ stdout: error ? "" : stdout, missing: (error as NodeJS.ErrnoException | null)?.code === "ENOENT" });
      });
    });
    if (result.missing) continue;
    try { return JSON.parse(result.stdout); } catch { break; }
  }
  return null;
}

export function previewReachable(url: string, timeoutMs = 3000): Promise<boolean> {
  const target = new URL(url);
  return new Promise((resolve) => {
    const socket = net.createConnection({ host: target.hostname, port: Number(target.port || (target.protocol === "https:" ? 443 : 80)) });
    const finish = (ok: boolean) => { socket.destroy(); resolve(ok); };
    socket.setTimeout(timeoutMs, () => finish(false));
    socket.once("connect", () => finish(true));
    socket.once("error", () => finish(false));
  });
}
