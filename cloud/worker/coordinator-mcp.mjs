#!/usr/bin/env node
// The `all_agents` MCP server of the "All agents" chat (coordinator.mjs): a
// stdio JSON-RPC proxy to the home supervisor's socket, which holds the tools.
//   node coordinator-mcp.mjs --socket <path>

import net from "node:net";
import path from "node:path";
import readline from "node:readline";

const at = process.argv.indexOf("--socket");
const socketPath = at > 0 ? process.argv[at + 1] : "";
if (!socketPath) {
  process.stderr.write("usage: coordinator-mcp.mjs --socket <path>\n");
  process.exit(2);
}
// Which agent is calling: Rudder gives each agent on a home its own scratch
// dir named after its run, and a wait ends when that chat gets a new message.
const caller = process.env.RUDDER_SCRATCH ? path.basename(process.env.RUDDER_SCRATCH) : null;
const calls = new Map(); // request id -> socket

function send(message) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);
}

function ask(request, id) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    let buffer = "";
    if (id !== undefined) calls.set(id, socket);
    socket.setEncoding("utf8");
    socket.on("connect", () => socket.write(`${JSON.stringify(request)}\n`));
    socket.on("data", (chunk) => { buffer += chunk; });
    socket.on("error", (error) => reject(new Error(`the Rudder home supervisor is not reachable (${error.code || error.message})`)));
    socket.on("close", () => {
      calls.delete(id);
      try {
        resolve(JSON.parse(buffer));
      } catch {
        reject(new Error("the call was cancelled or the supervisor closed it"));
      }
    });
  });
}

async function handle(message) {
  const { id, method, params } = message;
  if (method === "initialize") {
    send({ id, result: {
      protocolVersion: typeof params?.protocolVersion === "string" ? params.protocolVersion : "2025-06-18",
      capabilities: { tools: {} },
      serverInfo: { name: "rudder-all-agents", version: "1.0.0" },
    } });
    return;
  }
  if (method === "notifications/cancelled") {
    calls.get(params?.requestId)?.destroy();
    return;
  }
  if (id === undefined) return; // other notifications
  if (method === "ping") {
    send({ id, result: {} });
    return;
  }
  if (method === "tools/list") {
    const reply = await ask({ op: "tools" });
    if (!reply.ok) throw new Error(reply.error);
    send({ id, result: { tools: reply.tools } });
    return;
  }
  if (method === "tools/call") {
    const reply = await ask({ op: "call", name: params?.name, arguments: params?.arguments ?? {}, caller }, id);
    send({ id, result: { content: [{ type: "text", text: reply.ok ? reply.text : reply.error }], isError: !reply.ok } });
    return;
  }
  send({ id, error: { code: -32601, message: `method not found: ${method}` } });
}

readline.createInterface({ input: process.stdin }).on("line", (line) => {
  if (!line.trim()) return;
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    send({ id: null, error: { code: -32700, message: "parse error" } });
    return;
  }
  handle(message).catch((error) => {
    if (message.id !== undefined) send({ id: message.id, error: { code: -32603, message: error.message } });
  });
}).on("close", () => process.exit(0));
