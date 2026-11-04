import { createHash } from "node:crypto";
import fs from "node:fs";
import fsp from "node:fs/promises";
import net from "node:net";
import path from "node:path";
import { spawn } from "node:child_process";
import { rudderHome, writeJson } from "../util.js";
import { BOARD_ACCESS_VERSION, type BoardAuthority, type BoardControlMode, type BoardTarget } from "./authority.js";

type HostDescriptor = {
  format_version: 1;
  instance_id: string;
  origin: string;
  repo_root: string;
  process_id: number;
  control_mode: BoardControlMode;
};
const MAX_FRAME = 4096;

function paths(repoRoot: string): { directory: string; descriptor: string; socket: string; repo: string } {
  if (process.platform === "win32" || !process.getuid) throw new Error("Private board access requires Unix");
  const repo = fs.realpathSync(repoRoot);
  const key = createHash("sha256").update(repo).digest("hex").slice(0, 32);
  let directory = path.join(path.resolve(rudderHome()), "board");
  if (Buffer.byteLength(path.join(directory, `${key}.sock`)) >= 100) directory = `/tmp/rudder-board-${process.getuid()}`;
  return { directory, descriptor: path.join(directory, `${key}.json`), socket: path.join(directory, `${key}.sock`), repo };
}

function privateNode(file: string, kind: "directory" | "file" | "socket"): fs.Stats {
  const meta = fs.lstatSync(file);
  if (meta.isSymbolicLink() || meta.uid !== process.getuid!() || (meta.mode & 0o077)
    || !(kind === "directory" ? meta.isDirectory() : kind === "file" ? meta.isFile() && meta.nlink === 1 : meta.isSocket())) {
    throw new Error("Private board endpoint is unsafe");
  }
  return meta;
}

async function readDescriptor(file: string): Promise<HostDescriptor> {
  privateNode(path.dirname(file), "directory");
  const handle = await fsp.open(file, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW | fs.constants.O_NONBLOCK);
  try {
    const m = await handle.stat();
    if (!m.isFile() || m.uid !== process.getuid!() || m.mode & 0o077 || m.nlink !== 1 || m.size > MAX_FRAME) throw new Error("Invalid board descriptor");
    // Cap the actual read as well as the initial stat: a changed regular file
    // must not grow into an unbounded allocation between those operations.
    const bytes = Buffer.alloc(MAX_FRAME + 1);
    let length = 0;
    while (length < bytes.length) {
      const read = await handle.read(bytes, length, bytes.length - length, length);
      if (!read.bytesRead) break;
      length += read.bytesRead;
    }
    if (length > MAX_FRAME) throw new Error("Invalid board descriptor");
    const value = JSON.parse(bytes.subarray(0, length).toString("utf8")) as HostDescriptor;
    if (value.format_version !== BOARD_ACCESS_VERSION || typeof value.instance_id !== "string"
      || !/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(value.instance_id)
      || !/^http:\/\/127\.0\.0\.1(?::[1-9][0-9]{0,4})?$/.test(value.origin)
      || !Number.isSafeInteger(value.process_id) || value.process_id < 1 || typeof value.repo_root !== "string"
      || !["projector", "scheduler"].includes(value.control_mode)) throw new Error("Invalid board descriptor");
    const origin = new URL(value.origin);
    const port = Number(origin.port || 80);
    if (port < 1 || port > 65535 || origin.origin !== value.origin) throw new Error("Invalid board descriptor");
    return value;
  } finally { await handle.close(); }
}

export function openBoardBrowser(url: string): Promise<void> {
  // The fragment is passed only to the host opener. No output/error includes it.
  const command = process.platform === "darwin" ? "open" : "xdg-open";
  return new Promise((resolve, reject) => {
    const child = spawn(command, [url], { detached: true, stdio: "ignore" });
    child.once("error", () => reject(new Error("Browser opener is unavailable")));
    child.once("spawn", () => { child.unref(); resolve(); });
  });
}

export async function serveBoardHostAccess(repoRoot: string, authority: BoardAuthority,
  open: (url: string) => void | Promise<void> = openBoardBrowser): Promise<{ close(): Promise<void> }> {
  const p = paths(repoRoot);
  await fsp.mkdir(p.directory, { recursive: true, mode: 0o700 });
  privateNode(p.directory, "directory");
  try {
    privateNode(p.socket, "socket");
    const live = await new Promise<boolean>((resolve) => {
      const socket = net.connect(p.socket);
      socket.setTimeout(500);
      socket.once("connect", () => { socket.destroy(); resolve(true); });
      socket.once("timeout", () => { socket.destroy(); resolve(true); });
      socket.once("error", (error: NodeJS.ErrnoException) => resolve(!["ECONNREFUSED", "ENOENT"].includes(error.code ?? "")));
    });
    if (live) throw new Error("A board already owns its private endpoint");
    await fsp.unlink(p.socket);
  } catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
  let connections = 0;
  const sockets = new Set<net.Socket>();
  const server = net.createServer((socket) => {
    if (connections >= 8) { socket.destroy(); return; }
    connections++; sockets.add(socket);
    const deadline = setTimeout(() => socket.destroy(), 1000);
    deadline.unref();
    socket.once("close", () => { clearTimeout(deadline); connections--; sockets.delete(socket); });
    let bytes = "";
    let replied = false;
    socket.on("error", () => undefined);
    socket.on("data", (chunk) => {
      if (replied) return;
      bytes += chunk.toString("utf8");
      if (Buffer.byteLength(bytes) > MAX_FRAME) { socket.destroy(); return; }
      if (!bytes.includes("\n")) return;
      replied = true;
      try {
        const request = JSON.parse(bytes.slice(0, bytes.indexOf("\n")));
        if (!request || Object.keys(request).some((key) => !["format_version", "instance_id", "action", "target"].includes(key))
          || request.format_version !== 1 || request.instance_id !== authority.instanceId
          || !["info", "bootstrap", "open"].includes(request.action)
          || request.action !== "info" && !["project", "index", "usage"].includes(request.target)) throw new Error();
        if (request.action === "info") socket.end(`${JSON.stringify({ ...authority.descriptor(), control_mode: authority.controlMode })}\n`);
        else {
          const url = authority.bootstrapUrl(request.target);
          if (request.action === "open") {
            void Promise.resolve().then(() => open(url)).then(() => {
              socket.end('{"format_version":1,"ok":true}\n');
            }, () => socket.end('{"format_version":1,"error":"Browser opener is unavailable"}\n'));
          }
          else socket.end(`${JSON.stringify({ ...authority.descriptor(), url })}\n`);
        }
      } catch { socket.end('{"format_version":1,"error":"Board access is unavailable"}\n'); }
    });
  });
  try {
    await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(p.socket, resolve); });
    await fsp.chmod(p.socket, 0o600);
    await writeJson(p.descriptor, { ...authority.descriptor(), repo_root: p.repo,
      process_id: process.pid, control_mode: authority.controlMode }, { mode: 0o600 });
  } catch (error) {
    for (const socket of sockets) socket.destroy();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    throw error;
  }
  let closing: Promise<void> | undefined;
  return { close() { return closing ??= (async () => {
    for (const socket of sockets) socket.destroy();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    const value = await readDescriptor(p.descriptor).catch(() => null);
    if (value?.instance_id === authority.instanceId) await fsp.unlink(p.descriptor).catch(() => undefined);
  })(); } };
}

async function request(repoRoot: string, action: "info" | "bootstrap" | "open", target: BoardTarget = "project"): Promise<Record<string, unknown>> {
  const p = paths(repoRoot);
  const descriptor = await readDescriptor(p.descriptor);
  if (descriptor.repo_root !== p.repo) throw new Error("Board endpoint belongs to another project");
  privateNode(p.socket, "socket");
  return new Promise((resolve, reject) => {
    const socket = net.connect(p.socket);
    const failed = () => { socket.destroy(); reject(new Error("Private board access is unavailable")); };
    const deadline = setTimeout(failed, 1500);
    deadline.unref();
    socket.once("close", () => clearTimeout(deadline));
    socket.on("error", failed);
    socket.once("connect", () => socket.write(`${JSON.stringify({ format_version: 1,
      instance_id: descriptor.instance_id, action, target })}\n`));
    let body = "";
    socket.on("data", (chunk) => {
      body += chunk.toString("utf8");
      if (Buffer.byteLength(body) > MAX_FRAME) { failed(); return; }
      if (!body.includes("\n")) return;
      socket.destroy();
      try {
        const value = JSON.parse(body.slice(0, body.indexOf("\n")));
        if (value.format_version !== 1 || value.error || action === "open" && value.ok !== true) throw new Error();
        if (action !== "open" && (value.instance_id !== descriptor.instance_id || value.origin !== descriptor.origin)) throw new Error();
        if (action === "bootstrap") {
          const url = new URL(value.url);
          if (url.origin !== descriptor.origin || url.search || !/^#rudder-board-ticket=[a-f0-9]{64}$/.test(url.hash)) throw new Error();
        }
        resolve(value);
      } catch { failed(); }
    });
    socket.once("end", () => { if (!body.includes("\n")) failed(); });
  });
}

export async function inspectBoardHostAccess(repoRoot: string): Promise<Record<string, unknown>> { return request(repoRoot, "info"); }
export async function requestBoardBootstrap(repoRoot: string, target: BoardTarget = "project"): Promise<string> {
  return (await request(repoRoot, "bootstrap", target)).url as string;
}
export async function requestBoardOpen(repoRoot: string, target: BoardTarget = "project"): Promise<void> {
  await request(repoRoot, "open", target);
}
