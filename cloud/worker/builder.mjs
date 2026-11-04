// A builder machine in a home's burst build pool (see cloud/src/builders.ts).
//
// It serves one home over the app's private network: rsync uploads into
// /workspace/build/<key>, commands run there with their output streamed back,
// and outputs can be pulled the same way. Build caches (target/, node_modules,
// package caches, toolchains) stay on the volume between leases. The process
// exits after it has been idle for RUDDER_BUILDER_IDLE_SECONDS, which stops the
// machine, so an idle pool costs only its volume.
//
// Trust: the bearer token is the boundary. Whoever holds it can already run
// any command here, so the rsync endpoint only checks that it runs rsync's
// server side; it is this home's own machine, never shared between accounts.

import { spawn } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

import { FRAME, RSYNC_UPGRADE, buildDir, encodeFrame, tokenMatches } from "./burst-protocol.mjs";

const MAX_EXEC_BODY = 256 * 1024;

function bearer(req) {
  const header = req.headers.authorization || "";
  return header.startsWith("Bearer ") ? header.slice(7).trim() : "";
}

function sendJson(res, status, body) {
  const text = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(text) });
  res.end(text);
}

async function readBody(req, max) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > max) throw new Error("request body too large");
    chunks.push(chunk);
  }
  return chunks.length ? JSON.parse(Buffer.concat(chunks).toString("utf8")) : {};
}

function killGroup(child, signal) {
  try {
    process.kill(-child.pid, signal);
  } catch {
    try {
      child.kill(signal);
    } catch {
      // already gone
    }
  }
}

/**
 * The builder's HTTP server. `toolchains` is a promise that settles once the
 * first-boot toolchain install is finished; commands wait for it.
 */
export function createBuilderServer({
  root,
  tokenSha256,
  idleSeconds = 300,
  env = process.env,
  toolchains = Promise.resolve(),
  builderId = "",
  onIdle = () => process.exit(0),
  log = (line) => console.error(`rudder builder: ${line}`),
  now = () => Date.now(),
}) {
  fs.mkdirSync(root, { recursive: true });
  let active = 0;
  let lastActivity = now();
  let toolchainsReady = false;
  void toolchains.finally(() => {
    toolchainsReady = true;
  });
  const startedAt = now();
  const begin = () => {
    active += 1;
    lastActivity = now();
  };
  const end = () => {
    active = Math.max(0, active - 1);
    lastActivity = now();
  };

  async function exec(req, res) {
    const input = await readBody(req, MAX_EXEC_BODY);
    const command = typeof input.command === "string" ? input.command : "";
    if (!command.trim()) return sendJson(res, 400, { error: "command is required" });
    let cwd;
    try {
      cwd = buildDir(root, input.key, typeof input.cwd === "string" ? input.cwd : "");
    } catch (error) {
      return sendJson(res, 400, { error: error.message });
    }
    if (!fs.existsSync(cwd)) return sendJson(res, 404, { error: `nothing synced at ${path.relative(root, cwd) || "."}; upload first` });
    const extra = {};
    if (input.env && typeof input.env === "object") {
      for (const [name, value] of Object.entries(input.env)) {
        // The home's PATH and HOME mean nothing here.
        if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(name) && typeof value === "string" && name !== "PATH" && name !== "HOME") extra[name] = value;
      }
    }
    res.writeHead(200, { "content-type": "application/octet-stream", "cache-control": "no-store" });
    if (!toolchainsReady) {
      res.write(encodeFrame(FRAME.stderr, "rudder builder: installing toolchains (first boot)...\n"));
      await toolchains.catch(() => undefined);
    }
    const started = now();
    // Not a login shell: Debian's /etc/profile resets PATH and would drop the
    // volume's cargo and go.
    const child = spawn("bash", ["-c", command], {
      cwd,
      env: { ...env, ...extra, RUDDER_BURST: "1" },
      detached: true,
      stdio: ["ignore", "pipe", "pipe"],
    });
    let finished = false;
    let killTimer = null;
    child.stdout.on("data", (chunk) => res.write(encodeFrame(FRAME.stdout, chunk)));
    child.stderr.on("data", (chunk) => res.write(encodeFrame(FRAME.stderr, chunk)));
    // The client went away (Ctrl-C, lost connection): stop the whole group.
    res.on("close", () => {
      if (finished) return;
      killGroup(child, "SIGTERM");
      killTimer = setTimeout(() => killGroup(child, "SIGKILL"), 10_000);
      killTimer.unref?.();
    });
    const timeout = Number(input.timeoutSeconds) > 0 ? setTimeout(() => killGroup(child, "SIGKILL"), Number(input.timeoutSeconds) * 1000) : null;
    await new Promise((resolve) => {
      child.on("error", (error) => {
        res.write(encodeFrame(FRAME.stderr, `rudder builder: ${error.message}\n`));
      });
      child.on("close", (code, signal) => {
        finished = true;
        if (timeout) clearTimeout(timeout);
        if (killTimer) clearTimeout(killTimer);
        res.end(encodeFrame(FRAME.exit, JSON.stringify({ code, signal, seconds: (now() - started) / 1000 })));
        resolve();
      });
    });
  }

  async function clean(req, res) {
    const input = await readBody(req, 4096);
    let dir;
    try {
      dir = buildDir(root, input.key);
    } catch (error) {
      return sendJson(res, 400, { error: error.message });
    }
    fs.rmSync(dir, { recursive: true, force: true });
    sendJson(res, 200, { ok: true });
  }

  const server = http.createServer(async (req, res) => {
    lastActivity = now();
    if (!tokenMatches(bearer(req), tokenSha256)) return sendJson(res, 401, { error: "unauthorized" });
    const url = new URL(req.url, "http://builder");
    begin();
    res.on("close", end);
    try {
      if (req.method === "GET" && url.pathname === "/v1/health") {
        return sendJson(res, 200, {
          ok: true,
          builderId,
          cpus: os.availableParallelism?.() ?? os.cpus().length,
          memoryBytes: os.totalmem(),
          active: active - 1,
          toolchains: toolchainsReady ? "ready" : "installing",
          uptimeSeconds: Math.round((now() - startedAt) / 1000),
        });
      }
      if (req.method === "POST" && url.pathname === "/v1/exec") return await exec(req, res);
      if (req.method === "POST" && url.pathname === "/v1/clean") return await clean(req, res);
      sendJson(res, 404, { error: "not found" });
    } catch (error) {
      if (!res.headersSent) sendJson(res, 400, { error: error instanceof Error ? error.message : String(error) });
      else res.destroy();
    }
  });

  // rsync's remote side, exactly as ssh would run it: the words rsync hands
  // its remote shell, joined and run by sh in the build root.
  server.on("upgrade", (req, socket, head) => {
    lastActivity = now();
    const refuse = (status, text) => {
      socket.end(`HTTP/1.1 ${status} ${text}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n`);
    };
    if (!tokenMatches(bearer(req), tokenSha256)) return refuse(401, "Unauthorized");
    if (new URL(req.url, "http://builder").pathname !== "/v1/rsync" || String(req.headers.upgrade).toLowerCase() !== RSYNC_UPGRADE) return refuse(404, "Not Found");
    let argv;
    try {
      argv = JSON.parse(Buffer.from(String(req.headers["x-rudder-rsync-argv"] || ""), "base64url").toString("utf8"));
    } catch {
      argv = null;
    }
    const command = Array.isArray(argv) && argv.every((arg) => typeof arg === "string" && !arg.includes("\0")) ? argv.join(" ") : "";
    if (!/^rsync --server( |$)/.test(command)) return refuse(400, "Bad Request");
    begin();
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nupgrade: ${RSYNC_UPGRADE}\r\nconnection: Upgrade\r\n\r\n`);
    const child = spawn("sh", ["-c", command], { cwd: root, env, stdio: ["pipe", "pipe", "pipe"] });
    let stderr = "";
    child.stderr.on("data", (chunk) => {
      stderr = (stderr + chunk).slice(-4000);
    });
    if (head?.length) child.stdin.write(head);
    socket.pipe(child.stdin);
    child.stdout.pipe(socket);
    child.stdin.on("error", () => undefined);
    socket.on("error", () => child.kill("SIGTERM"));
    socket.on("close", () => {
      if (child.exitCode === null) child.kill("SIGTERM");
    });
    child.on("close", (code) => {
      if (code !== 0 && stderr.trim()) log(`rsync exited ${code}: ${stderr.trim().split("\n").pop()}`);
      socket.end();
      end();
    });
  });

  const idleTimer = setInterval(() => {
    if (active === 0 && now() - lastActivity > idleSeconds * 1000) {
      clearInterval(idleTimer);
      log(`idle for ${idleSeconds}s; stopping`);
      server.close();
      onIdle();
    }
  }, 5_000);
  idleTimer.unref?.();

  return {
    server,
    get active() {
      return active;
    },
    close() {
      clearInterval(idleTimer);
      server.close();
    },
  };
}

// ---------------------------------------------------------------------------
// Machine boot: keep toolchains and caches on the volume, then serve.
// ---------------------------------------------------------------------------

function run(command, env) {
  return new Promise((resolve) => {
    const child = spawn("sh", ["-c", command], { env, stdio: ["ignore", "inherit", "inherit"] });
    child.on("close", (code) => resolve(code === 0));
    child.on("error", () => resolve(false));
  });
}

/** Link $HOME's toolchain dirs onto the volume, like the home machine does. */
function persistToolchainDirs(home, volume) {
  const stored = path.join(volume, "home");
  for (const name of [".cargo", ".rustup", "go", ".cache"]) {
    const live = path.join(home, name);
    const target = path.join(stored, name);
    fs.mkdirSync(target, { recursive: true });
    try {
      if (fs.lstatSync(live).isSymbolicLink()) continue;
      fs.cpSync(live, target, { recursive: true, force: false, errorOnExist: false });
      fs.rmSync(live, { recursive: true, force: true });
    } catch {
      // absent
    }
    fs.symlinkSync(target, live);
  }
}

// Rust and Go, the same way home.mjs installs them: once, onto the volume.
async function ensureToolchains(home, toolsDir, env) {
  const toolsBin = path.join(toolsDir, "bin");
  fs.mkdirSync(toolsBin, { recursive: true });
  if (!fs.existsSync(path.join(home, ".cargo", "bin", "cargo"))) {
    await run("curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path", env);
  }
  if (!fs.existsSync(path.join(toolsBin, "go"))) {
    const arch = process.arch === "arm64" ? "arm64" : "amd64";
    await run(`set -e
      version=$(curl -fsSL "https://go.dev/VERSION?m=text" | head -1)
      case "$version" in go1.*) ;; *) exit 1 ;; esac
      dir=$(mktemp -d)
      curl -fsSL "https://go.dev/dl/$version.linux-${arch}.tar.gz" -o "$dir/go.tgz"
      rm -rf "${toolsDir}/go"
      tar -xzf "$dir/go.tgz" -C "${toolsDir}"
      rm -rf "$dir"
      ln -sf "${toolsDir}/go/bin/go" "${toolsBin}/go"
      ln -sf "${toolsDir}/go/bin/gofmt" "${toolsBin}/gofmt"`, env);
  }
}

async function main() {
  const tokenSha256 = (process.env.RUDDER_BUILDER_TOKEN_SHA256 || "").trim();
  if (!/^[0-9a-f]{64}$/.test(tokenSha256)) {
    console.error("rudder builder: RUDDER_BUILDER_TOKEN_SHA256 is required");
    process.exit(64);
  }
  const volume = process.env.RUDDER_BUILDER_VOLUME || "/workspace";
  const home = os.homedir() || "/home/rudder";
  const toolsDir = path.join(volume, "tools");
  persistToolchainDirs(home, volume);
  const env = {
    ...process.env,
    HOME: home,
    PATH: `${path.join(toolsDir, "bin")}:${path.join(home, ".cargo", "bin")}:${path.join(home, "go", "bin")}:${path.join(home, ".local", "bin")}:${process.env.PATH || ""}`,
    CARGO_INCREMENTAL: process.env.CARGO_INCREMENTAL || "1",
  };
  for (const secret of ["RUDDER_BUILDER_TOKEN_SHA256"]) delete env[secret];
  const port = Number(process.env.RUDDER_BUILDER_PORT || 7420);
  const builder = createBuilderServer({
    root: path.join(volume, "build"),
    tokenSha256,
    idleSeconds: Number(process.env.RUDDER_BUILDER_IDLE_SECONDS || 300),
    env,
    builderId: process.env.RUDDER_BUILDER_ID || "",
    toolchains: process.env.RUDDER_BUILDER_TOOLCHAINS === "0" ? Promise.resolve() : ensureToolchains(home, toolsDir, env),
  });
  // Fly's private network is IPv6; nothing listens on a public address.
  builder.server.listen(port, "::", () => console.error(`rudder builder: ${process.env.RUDDER_BUILDER_ID || ""} listening on [::]:${port}`));
  for (const signal of ["SIGTERM", "SIGINT"]) process.on(signal, () => process.exit(0));
}

if (process.argv[1] && import.meta.url === pathToFileURL(fs.realpathSync(process.argv[1])).href) {
  await main();
}
