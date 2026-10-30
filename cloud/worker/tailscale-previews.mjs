import http from "node:http";
import { execFile } from "node:child_process";

export function runTailnetCommand(args, { socket, timeoutMs = 10_000 } = {}) {
  return new Promise((resolve) => {
    execFile("tailscale", [...(socket ? ["--socket", socket] : []), ...args], {
      timeout: timeoutMs, maxBuffer: 2 * 1024 * 1024, encoding: "utf8",
    }, (error, stdout, stderr) => resolve({
      code: error ? (typeof error.code === "number" ? error.code : -1) : 0,
      stdout: stdout || "",
      // An execFile error's message includes argv, which can contain an auth
      // key. Never return it to a log or the control plane.
      stderr: stderr || (error ? String(error.code || "command failed") : ""),
    }));
  });
}

const validPort = (value) => Number.isInteger(value) && value >= 1024 && value <= 65535 && value !== 41641;

export function tailnetStatus(value, fallbackHostname = "rudder-home") {
  const self = value?.Self;
  const dnsName = typeof self?.DNSName === "string" ? self.DNSName.replace(/\.$/, "").toLowerCase() : "";
  const online = value?.BackendState === "Running" && self?.Online === true;
  return {
    hostname: typeof self?.HostName === "string" ? self.HostName : fallbackHostname,
    dnsName: /^[a-z0-9-]+\.[a-z0-9.-]+\.ts\.net$/.test(dnsName) ? dnsName : null,
    online,
    error: online ? null : `Tailscale ${typeof value?.BackendState === "string" ? value.BackendState : "status unavailable"}`,
  };
}

// Only advertise HTTP listeners belonging to a known project, reachable over
// loopback. Do not turn a database or a listener in an unrelated cwd into a URL.
export function previewOrigin(entry) {
  if (!validPort(entry?.port) || typeof entry?.workspaceId !== "string" || !entry.workspaceId) return null;
  const address = String(entry.address || "").toUpperCase();
  if (entry.family === 6) {
    if (address === "00000000000000000000000000000000" || address === "00000000000000000000000001000000") {
      return `http://[::1]:${entry.port}`;
    }
    return null;
  }
  if (address === "00000000") return `http://127.0.0.1:${entry.port}`;
  if (/^[0-9A-F]{8}$/.test(address) && address.endsWith("7F")) {
    const host = address.match(/../g).reverse().map((byte) => parseInt(byte, 16)).join(".");
    return `http://${host}:${entry.port}`;
  }
  return null;
}

export function probePreview(origin, timeoutMs = 1500) {
  return new Promise((resolve) => {
    let request;
    let settled = false;
    const finish = (ready) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      request?.destroy();
      resolve(ready);
    };
    // Bound total probe time, independently of partial-header/socket activity.
    const timer = setTimeout(() => finish(false), timeoutMs);
    try {
      request = http.request(origin, { method: "HEAD" }, (response) => {
        response.destroy();
        finish(true); // 401/404/405 are still a real HTTP server.
      });
      request.on("error", () => finish(false));
      request.end();
    } catch {
      finish(false);
    }
  });
}

/** Owns only the discovered project's ports on Rudder's dedicated tailscaled.
 * Commands are serialized and bounded by the caller; no Funnel/public route is
 * ever configured. A URL exists only after HTTP responds and Serve succeeds. */
export function createTailnetPreviews({ execute, hostname = "rudder-home", probe = probePreview, onChange = () => {},
  ownedPorts = [], onOwnedPorts = () => {}, now = Date.now }) {
  const state = { hostname, dnsName: null, online: false, https: null, error: null };
  const ports = new Map();
  let queue = Promise.resolve();
  let desired = new Map();
  const owned = new Set(Array.isArray(ownedPorts) ? ownedPorts.filter(validPort) : []);
  let httpsRetryAt = 0;

  function snapshot() {
    return Array.from(ports.values())
      .filter((entry) => state.online && state.dnsName && entry.host === state.dnsName
        && entry.url && desired.get(entry.port)?.identity === entry.identity)
      .map(({ port, workspaceId, url, https, label }) => ({ port, workspaceId, url, https, label }));
  }

  async function command(args) {
    try { return await execute(args); }
    catch (error) { return { code: -1, stdout: "", stderr: error.message }; }
  }

  async function remove(entry) {
    if (ports.delete(entry.port)) onChange();
    // A command may time out after applying its config. Remove both private
    // modes rather than leave an old proxy pointing at a reused local port.
    const secure = await command(["serve", `--https=${entry.port}`, "off"]);
    const plain = await command(["serve", `--http=${entry.port}`, "off"]);
    if (secure.code === 0 && plain.code === 0 && owned.delete(entry.port)) onOwnedPorts([...owned]);
  }

  async function update() {
    const before = JSON.stringify({ state, previews: snapshot() });
    const result = await command(["status", "--json"]);
    let status = null;
    if (result.code === 0) {
      try { status = JSON.parse(result.stdout); } catch { /* unavailable is offline */ }
    }
    const wasOnline = state.online;
    Object.assign(state, tailnetStatus(status, hostname));
    if (!state.online || !state.dnsName) {
      if (state.online && !state.dnsName) state.error = "Tailscale has no tailnet DNS name for previews";
      if (before !== JSON.stringify({ state, previews: snapshot() })) onChange();
      return;
    }
    if (!wasOnline) {
      httpsRetryAt = 0;
      for (const entry of ports.values()) entry.retryAt = 0;
    }

    // This ownership journal survives supervisor restarts; unrelated Serve
    // configuration is never swept just because its port was not discovered.
    for (const port of owned) {
      if (!desired.has(port) && !ports.has(port)) await remove({ port });
    }

    for (const entry of Array.from(ports.values())) {
      if (desired.get(entry.port)?.identity !== entry.identity || entry.host !== state.dnsName) await remove(entry);
    }
    for (const item of desired.values()) {
      let entry = ports.get(item.port);
      const needsProbe = !entry || entry.probeAt <= now();
      if (needsProbe) {
        if (!(await probe(item.origin))) {
          if (entry || owned.has(item.port)) await remove(entry || { port: item.port });
          continue;
        }
        if (entry) entry.probeAt = now() + 5000;
      }
      if (entry && entry.retryAt > now()) continue;
      entry ??= { ...item, host: state.dnsName, url: null, protocol: null, https: false, retryAt: 0, probeAt: now() + 5000 };
      ports.set(item.port, entry);
      if (!owned.has(item.port)) {
        onOwnedPorts([...owned, item.port]); // Persist before applying Serve.
        owned.add(item.port);
      }
      // Retry HTTPS periodically: a tailnet admin may enable certificates
      // after boot. A single refused port must not disable HTTPS forever.
      if (entry.protocol === "http") await command(["serve", `--http=${entry.port}`, "off"]);
      const attemptHTTPS = now() >= httpsRetryAt;
      const secure = attemptHTTPS
        ? await command(["serve", "--bg", `--https=${entry.port}`, item.origin]) : { code: -1 };
      if (secure.code === 0) {
        Object.assign(entry, { protocol: "https", https: true, url: `https://${state.dnsName}:${item.port}/`, retryAt: Infinity });
        state.https = true;
        state.error = null;
      } else {
        // One certificate-consent timeout is enough for this scan. Other
        // listeners can get their private HTTP route without waiting 10s each.
        if (attemptHTTPS) httpsRetryAt = now() + 60_000;
        const plain = await command(["serve", "--bg", `--http=${entry.port}`, item.origin]);
        if (plain.code === 0) {
          Object.assign(entry, { protocol: "http", https: false, url: `http://${state.dnsName}:${item.port}/`, retryAt: now() + 60_000 });
          state.https = false;
          state.error = "HTTPS previews unavailable; using private tailnet HTTP";
        } else {
          Object.assign(entry, { protocol: null, url: null, retryAt: now() + 10_000 });
          state.error = "Tailscale Serve is unavailable for previews";
        }
      }
      // Do not withhold a ready link while slower listeners are still probed.
      onChange();
    }
    if (before !== JSON.stringify({ state, previews: snapshot() })) onChange();
  }

  return {
    state,
    snapshot,
    reconcile(entries) {
      desired = new Map(entries.flatMap((entry) => {
        const origin = previewOrigin(entry);
        return origin ? [[entry.port, { ...entry, origin, identity: `${entry.workspaceId}:${entry.pid}:${origin}` }]] : [];
      }));
      const next = queue.then(update, update);
      queue = next.catch(() => {});
      return next;
    },
    offline(message) {
      state.online = false;
      state.error = message;
      onChange();
    },
  };
}
