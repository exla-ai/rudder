import http, { type IncomingMessage, type ServerResponse } from "node:http";
import type { Duplex } from "node:stream";
import { WebSocketServer, createWebSocketStream } from "ws";

type ResolveGrant = (builderId: string, leaseId: string, token: string) => { url: string; token: string } | null;
const prefix = "/api/rudder/builder-relay/";
const route = /^\/api\/rudder\/builder-relay\/([\w-]{1,128})\/([\w-]{1,128})\/v1\/(health|exec|clean|rsync)$/;

// Only the pool can choose a private destination. The client's bearer and
// arbitrary HTTP headers never pass through to the builder.
export function createBuilderRelay(resolveGrant: ResolveGrant) {
  const wss = new WebSocketServer({ noServer: true, maxPayload: 256 * 1024, perMessageDeflate: false, clientTracking: false });
  function target(req: IncomingMessage, upgrade: boolean) {
    const match = route.exec(req.url ?? "");
    if (!match) return null;
    const [, builderId, leaseId, operation] = match;
    const method = operation === "health" || operation === "rsync" ? "GET" : "POST";
    if (req.method !== method || (operation === "rsync") !== upgrade) return null;
    const bearer = req.headers.authorization;
    if (!bearer?.startsWith("Bearer ") || !bearer.slice(7)) return null;
    const resolve = () => resolveGrant(builderId, leaseId, bearer.slice(7));
    const grant = resolve();
    return grant && {
      url: new URL("/v1/" + operation, grant.url), authorization: "Bearer " + grant.token,
      guard(stream: ServerResponse | Duplex) {
        // Commands and rsync can outlive a released or unrenewed lease.
        const timer = setInterval(() => {
          try { if (!resolve()) stream.destroy(); } catch { stream.destroy(); }
        }, 5000);
        timer.unref();
        stream.once("close", () => clearInterval(timer));
      },
    };
  }

  return {
    http(req: IncomingMessage, res: ServerResponse): boolean {
      if (!req.url?.startsWith(prefix)) return false;
      const dest = target(req, false);
      if (!dest) { res.writeHead(403).end(); return true; }
      dest.guard(res);
      const upstream = http.request(dest.url, { method: req.method, headers: {
        authorization: dest.authorization, "content-type": "application/json",
      } }, (reply) => {
        res.writeHead(reply.statusCode ?? 502, {
          "content-type": reply.headers["content-type"] ?? "application/octet-stream",
          "cache-control": "no-store",
        });
        reply.on("error", () => res.destroy());
        reply.pipe(res);
      });
      upstream.on("error", () => {
        if (!res.headersSent) res.writeHead(502).end();
        else res.destroy();
      });
      res.on("close", () => upstream.destroy());
      req.on("error", () => upstream.destroy());
      req.pipe(upstream);
      return true;
    },
    upgrade(req: IncomingMessage, client: Duplex, head: Buffer): boolean {
      if (!req.url?.startsWith(prefix)) return false;
      const refuse = (status: "403 Forbidden" | "502 Bad Gateway") => {
        client.end("HTTP/1.1 " + status + "\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
      };
      const dest = target(req, true);
      const argv = req.headers["x-rudder-rsync-argv"];
      if (!dest || typeof argv !== "string" || req.headers.upgrade?.toLowerCase() !== "websocket") {
        refuse("403 Forbidden");
        return true;
      }
      dest.guard(client);
      const upstream = http.request(dest.url, { headers: {
        authorization: dest.authorization, connection: "Upgrade", upgrade: "rudder-rsync", "x-rudder-rsync-argv": argv,
      } });
      upstream.setTimeout(10_000, () => upstream.destroy());
      client.on("close", () => upstream.destroy());
      client.on("error", () => upstream.destroy());
      upstream.on("error", () => refuse("502 Bad Gateway"));
      upstream.on("response", (reply) => { reply.resume(); refuse("502 Bad Gateway"); });
      upstream.on("upgrade", (reply, builder, first) => {
        if (reply.headers.upgrade !== "rudder-rsync" || client.destroyed) {
          builder.destroy(); refuse("502 Bad Gateway"); return;
        }
        builder.setTimeout(0);
        builder.on("error", () => client.destroy());
        client.on("close", () => builder.destroy());
        wss.handleUpgrade(req, client, head, (ws) => {
          const stream = createWebSocketStream(ws, { readableObjectMode: true });
          stream.on("error", () => builder.destroy());
          ws.on("close", () => builder.destroy());
          if (first.length) stream.write(first);
          builder.pipe(stream);
          // A WebSocket close shuts both directions. A text EOF lets rsync
          // finish writing its reply after its remote stdin closes.
          stream.on("data", (message: Buffer | string) => {
            if (message === "eof" && !builder.writableEnded) { builder.end(); return; }
            if (!Buffer.isBuffer(message) || builder.writableEnded) { ws.terminate(); return; }
            if (!builder.write(message)) { stream.pause(); builder.once("drain", () => stream.resume()); }
          });
        });
      });
      upstream.end();
      return true;
    },
  };
}
