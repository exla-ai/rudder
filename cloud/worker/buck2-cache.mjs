import { execFile, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";

const run = promisify(execFile);
// Buck2's gRPC transport needs TCP. Use a separate loopback IP from localhost
// preview forwarding, and explicitly exclude this address from Rudder previews.
export const BUCK2_CACHE_HOST = "127.0.0.2";

function configFile(home) {
  return path.join(home, ".buckconfig.d", "50-rudder");
}

export async function removeBuck2CacheConfig(home) {
  await fs.rm(configFile(home), { force: true });
}

async function installConfig(home, address) {
  const file = configFile(home);
  const tmp = file + "." + randomUUID();
  await fs.mkdir(path.dirname(file), { recursive: true, mode: 0o700 });
  const endpoint = "grpc://" + address;
  try {
    await fs.writeFile(tmp, [
      "[buck2_re_client]", "instance_name = rudder", "tls = false",
      "cas_address = " + endpoint, "action_cache_address = " + endpoint,
      "engine_address = " + endpoint, "",
    ].join("\n"), { mode: 0o600, flag: "wx", flush: true });
    await fs.rename(tmp, file);
  } finally {
    await fs.rm(tmp, { force: true });
  }
}

// bazel-remote accepts port 0 but does not report the assigned port. Only
// accept a listening TCP socket actually owned by the child we started.
async function listeningAddress(pid, signal) {
  while (true) {
    signal.throwIfAborted();
    try {
      const { stdout } = await run("/usr/bin/lsof", ["-nP", "-a", "-p", String(pid), "-i4TCP", "-sTCP:LISTEN", "-Fn"], { signal, env: { LC_ALL: "C" } });
      const entry = stdout.split("\n").find((line) => line.startsWith("n" + BUCK2_CACHE_HOST + ":"));
      if (entry) return entry.slice(1);
    } catch (err) {
      if (err.code !== 1) throw err; // lsof exits 1 when there are no matches yet.
    }
    await delay(100, undefined, { signal });
  }
}

function launchDaemon({ binary, cacheDir, socket, backend, address }) {
  const child = spawn("/usr/bin/setpriv", [
    "--pdeathsig", "SIGKILL", "--", binary,
    "--dir", cacheDir, "--max_size", "1", "--max_size_hard_limit", "2",
    "--storage_mode", "uncompressed", "--max_blob_size", "268435456", "--max_proxy_blob_size", "268435456",
    "--grpc_address", address, "--http_address", "unix://" + socket,
    "--profile_address", "none", "--access_log_level", "none", "--num_uploaders", "4", "--max_queued_uploads", "1024",
  ], {
    // The backend URL is logged by upstream on some error paths.
    env: { BAZEL_REMOTE_HTTP_PROXY_URL: backend.href },
    stdio: "ignore",
  });
  const exited = new AbortController();
  let spawnError;
  child.once("error", (err) => { spawnError = err; });
  const stopped = new Promise((resolve) => child.once("close", (code, signal) => {
    const error = new Error("Buck2 cache daemon stopped (" + (spawnError?.code || signal || "exit " + code) + ")");
    exited.abort(error);
    resolve(error);
  }));
  return {
    stopped,
    async ready(closing) {
      const signal = AbortSignal.any([closing, exited.signal, AbortSignal.timeout(60_000)]);
      try {
        return await listeningAddress(child.pid, signal);
      } catch (err) {
        throw signal.reason ?? err;
      }
    },
    async close() {
      if (exited.signal.aborted) return;
      child.kill("SIGTERM");
      const kill = setTimeout(() => child.kill("SIGKILL"), 3000);
      try { await stopped; } finally { clearTimeout(kill); }
    },
  };
}

export async function startBuck2Cache({ home, cacheDir, proxy, binary = "/usr/local/bin/bazel-remote", log = () => {} }) {
  const backend = new URL("/buck2", proxy.url);
  if (backend.protocol !== "http:" || backend.hostname !== "127.0.0.1" || !proxy.token) {
    throw new Error("Buck2 requires an authenticated home cache proxy on loopback");
  }
  backend.username = "rudder";
  backend.password = proxy.token;
  await fs.mkdir(cacheDir, { recursive: true, mode: 0o700 });
  const sockets = await fs.mkdtemp(path.join(os.tmpdir(), "rudder-buck2-"));
  const socket = path.join(sockets, "http.sock");
  const options = { binary, cacheDir, socket, backend };
  const closing = new AbortController();
  let daemon = launchDaemon({ ...options, address: BUCK2_CACHE_HOST + ":0" });
  let address;
  try {
    address = await daemon.ready(closing.signal);
    await installConfig(home, address);
  } catch (err) {
    await daemon.close();
    await fs.rm(sockets, { recursive: true, force: true });
    throw err;
  }

  // Keep the endpoint published to existing shells across a daemon crash.
  // A failed restart is reported and backed off, without restarting dashboards.
  const supervise = async () => {
    let error = await daemon.stopped;
    let waitMs = 1000;
    while (!closing.signal.aborted) {
      log(error.message + "; restarting Buck2 cache in " + waitMs + "ms");
      try {
        await delay(waitMs, undefined, { signal: closing.signal });
        await fs.rm(socket, { force: true });
        closing.signal.throwIfAborted();
        daemon = launchDaemon({ ...options, address });
        await daemon.ready(closing.signal);
        log("Buck2 cache restored on " + address);
        waitMs = 1000;
        error = await daemon.stopped;
      } catch (err) {
        await daemon.close();
        error = err;
        waitMs = Math.min(waitMs * 2, 30_000);
      }
    }
  };
  const supervised = supervise();
  return {
    address,
    async close() {
      closing.abort();
      await daemon.close();
      await supervised;
      await fs.rm(sockets, { recursive: true, force: true });
    },
  };
}
