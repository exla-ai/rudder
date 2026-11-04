// The home supervisor's side of the burst build pool. Agents never see the
// worker token (buildChildEnv strips it), so `rudder-burst` asks the
// supervisor over a local socket and the supervisor asks the control plane.
// What comes back is a lease on one builder and that builder's own token.
//
//   GET  /builders              this home's builders
//   POST /lease                 { seconds?, holder? }
//   POST /leases/<id>/renew     { seconds? }
//   POST /leases/<id>/release

import fs from "node:fs";
import http from "node:http";

const MAX_BODY = 16 * 1024;

export function startBurstBroker({ socketPath, cloudUrl, homeId, workerToken, log = () => {}, fetchImpl = fetch }) {
  const base = `${cloudUrl.replace(/\/$/, "")}/api/rudder/workspace/${encodeURIComponent(homeId)}/builders`;

  async function forward(method, suffix, body) {
    const response = await fetchImpl(`${base}${suffix}`, {
      method,
      headers: { authorization: `Bearer ${workerToken}`, ...(body === undefined ? {} : { "content-type": "application/json" }) },
      body: body === undefined ? undefined : JSON.stringify(body),
      // A cold builder takes a while to create and start.
      signal: AbortSignal.timeout(180_000),
    });
    const text = await response.text();
    let parsed;
    try {
      parsed = text ? JSON.parse(text) : {};
    } catch {
      parsed = { error: text.slice(0, 300) || `control plane answered ${response.status}` };
    }
    return { status: response.status, body: parsed };
  }

  const server = http.createServer(async (req, res) => {
    const reply = (status, body) => {
      const text = JSON.stringify(body);
      res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(text) });
      res.end(text);
    };
    try {
      const url = new URL(req.url, "http://burst");
      const chunks = [];
      let size = 0;
      for await (const chunk of req) {
        size += chunk.length;
        if (size > MAX_BODY) return reply(413, { error: "request too large" });
        chunks.push(chunk);
      }
      const input = chunks.length ? JSON.parse(Buffer.concat(chunks).toString("utf8")) : {};
      const pick = (keys) => Object.fromEntries(keys.filter((key) => input[key] !== undefined).map((key) => [key, input[key]]));
      let result;
      const leaseMatch = url.pathname.match(/^\/leases\/([A-Za-z0-9-]{1,64})\/(renew|release)$/);
      if (req.method === "GET" && url.pathname === "/builders") result = await forward("GET", "");
      else if (req.method === "POST" && url.pathname === "/lease") result = await forward("POST", "/lease", pick(["seconds", "holder"]));
      else if (req.method === "POST" && leaseMatch) result = await forward("POST", `/${leaseMatch[1]}/${leaseMatch[2]}`, pick(["seconds"]));
      else return reply(404, { error: "not found" });
      reply(result.status, result.body);
    } catch (error) {
      reply(502, { error: `the control plane did not answer: ${error instanceof Error ? error.message : String(error)}` });
    }
  });

  try {
    if (fs.lstatSync(socketPath).isSocket()) fs.unlinkSync(socketPath);
  } catch {
    // no stale socket
  }
  server.on("error", (error) => log(`burst broker: ${error.message}`));
  server.listen(socketPath, () => {
    // Same user as the agents; nobody else on the machine.
    try {
      fs.chmodSync(socketPath, 0o600);
    } catch {
      // best effort
    }
  });
  return server;
}
