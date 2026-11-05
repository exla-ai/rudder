#!/usr/bin/env node
// rudder-burst: run a heavy command (a release build, a full test suite) on a
// builder from this home's burst build pool instead of on the home itself.
//
// It leases a builder through the supervisor (RUDDER_BURST_SOCKET), uploads
// the repository with rsync (only what changed; .gitignored paths stay
// behind, and the builder keeps its own build outputs between runs), runs the
// command in the same relative directory with output streamed back, pulls any
// --pull paths home, and releases the builder.

import { execFileSync, spawn } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import { FRAME, RSYNC_UPGRADE, frameDecoder, safeKey, sha256Hex, shellJoin } from "./burst-protocol.mjs";

export const EXIT = { usage: 64, unavailable: 69, failed: 70, busy: 75 };

const USAGE = `usage: rudder-burst [options] [--] <command...>
       rudder-burst status
       rudder-burst clean

Runs <command> on a builder machine from this home's build pool, in the same
directory relative to the repository root, and exits with its exit code.

options:
  --pull <path>      copy <path> (relative to the repo root) back afterwards; repeatable
  --exclude <rule>   rsync exclude rule for the upload; repeatable
  --env NAME=VALUE   set an environment variable for the command; repeatable
  --git              upload .git too (left out by default)
  --no-sync          skip the upload and use what the builder already has
  --key <name>       build directory on the builder (default: from the repo)
  --minutes <n>      lease length, renewed while the command runs (default 10)
  --wait <minutes>   how long to wait when every builder is busy (default 10)

When this home has no build pool (it is off, or the home runs on a host
outside Fly), or every builder stays busy, the command runs here instead.`;

class BurstError extends Error {
  constructor(message, code = EXIT.failed) {
    super(message);
    this.code = code;
  }
}

export function parseArgs(argv) {
  const opts = { pull: [], exclude: [], env: {}, git: false, sync: true, key: "", minutes: 10, wait: 10, command: [], action: "run" };
  if (argv[0] === "status" || argv[0] === "clean") {
    opts.action = argv[0];
    argv = argv.slice(1);
  }
  const value = (i, flag) => {
    if (i >= argv.length) throw new BurstError(`${flag} needs a value\n${USAGE}`, EXIT.usage);
    return argv[i];
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--") {
      opts.command = argv.slice(i + 1);
      break;
    }
    if (arg === "--pull") opts.pull.push(value(++i, arg));
    else if (arg === "--exclude") opts.exclude.push(value(++i, arg));
    else if (arg === "--env") {
      const pair = value(++i, arg);
      const eq = pair.indexOf("=");
      if (eq <= 0) throw new BurstError(`--env wants NAME=VALUE, got ${pair}`, EXIT.usage);
      opts.env[pair.slice(0, eq)] = pair.slice(eq + 1);
    } else if (arg === "--git") opts.git = true;
    else if (arg === "--no-sync") opts.sync = false;
    else if (arg === "--key") opts.key = value(++i, arg);
    else if (arg === "--minutes" || arg === "--wait") {
      const n = Number(value(++i, arg));
      if (!Number.isFinite(n) || n < 0) throw new BurstError(`${arg} wants a number of minutes`, EXIT.usage);
      opts[arg.slice(2)] = n;
    } else if (arg === "--or-local") {
      // Older callers ask for the fallback; it is how rudder-burst always works now.
    }
    else if (arg === "-h" || arg === "--help") throw new BurstError(USAGE, 0);
    else if (arg.startsWith("-")) throw new BurstError(`unknown option ${arg}\n${USAGE}`, EXIT.usage);
    else {
      opts.command = argv.slice(i);
      break;
    }
  }
  for (const pulled of opts.pull) {
    if (path.isAbsolute(pulled) || pulled.split(/[\\/]/).includes("..")) throw new BurstError(`--pull paths are relative to the repo root: ${pulled}`, EXIT.usage);
  }
  if (opts.action === "run" && opts.command.length === 0) throw new BurstError(USAGE, EXIT.usage);
  return opts;
}

/** One word stays as typed (a shell line); several are quoted word by word. */
export function commandLine(words) {
  return words.length === 1 ? words[0] : shellJoin(words);
}

function quiet(command, args, cwd) {
  try {
    return execFileSync(command, args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"], timeout: 10_000 }).trim();
  } catch {
    return "";
  }
}

/**
 * The repo root, where cwd sits inside it, and the builder directory key.
 * Worktrees of one repo share a key (same origin), so they share the
 * builder's warm target directory; the builder runs one lease at a time.
 */
export function repoInfo(cwd, run = quiet) {
  const root = run("git", ["rev-parse", "--show-toplevel"], cwd) || run("jj", ["root"], cwd) || cwd;
  let remote = run("git", ["remote", "get-url", "origin"], root);
  if (!remote) {
    const line = run("jj", ["git", "remote", "list"], root).split("\n").find((entry) => entry.startsWith("origin "));
    remote = line ? line.slice("origin ".length).trim() : "";
  }
  const name = path.basename((remote || root).replace(/\/+$/, "").replace(/\.git$/, "")) || "repo";
  return { root, rel: path.relative(root, cwd), key: safeKey(`${name}-${sha256Hex(remote || root).slice(0, 8)}`) };
}

export function uploadArgs({ root, key, git, exclude }, rsh) {
  return [
    "-a",
    "--delete",
    // Ignored paths are neither sent nor deleted on the builder, so its own
    // target/, node_modules/ and dist/ survive from run to run.
    "--filter=:- .gitignore",
    "--exclude=/.jj",
    ...(git ? [] : ["--exclude=/.git"]),
    ...exclude.map((rule) => `--exclude=${rule}`),
    "-e",
    rsh,
    `${root.replace(/\/+$/, "")}/`,
    `burst:${key}/`,
  ];
}

/** rsync args to copy one builder path home, and the local directory it lands in. */
export function pullArgs({ root, key }, pulled, rsh) {
  const rel = pulled.replace(/^(\.\/)+/, "");
  // "dir/" copies the directory's contents into dir/; "dir" or "file" lands
  // beside its siblings, the way rsync treats a trailing slash.
  const dest = rel.endsWith("/") ? path.join(root, rel) : path.join(root, path.dirname(rel));
  return { dest, args: ["-a", "-e", rsh, `burst:${key}/${rel}`, `${dest.replace(/\/+$/, "")}/`] };
}

function brokerRequest(socketPath, method, pathname, body) {
  return new Promise((resolve, reject) => {
    const text = body === undefined ? "" : JSON.stringify(body);
    const req = http.request({ socketPath, method, path: pathname, headers: text ? { "content-type": "application/json", "content-length": Buffer.byteLength(text) } : {} }, (res) => {
      const chunks = [];
      res.on("data", (chunk) => chunks.push(chunk));
      res.on("end", () => {
        let parsed = {};
        try {
          parsed = chunks.length ? JSON.parse(Buffer.concat(chunks).toString("utf8")) : {};
        } catch {
          parsed = { error: Buffer.concat(chunks).toString("utf8").slice(0, 300) };
        }
        resolve({ status: res.statusCode || 0, body: parsed });
      });
    });
    req.on("error", reject);
    req.end(text);
  });
}

function builderJson(lease, method, pathname, body) {
  return new Promise((resolve, reject) => {
    const text = body === undefined ? "" : JSON.stringify(body);
    const req = http.request(new URL(pathname, lease.url), {
      method,
      headers: { authorization: `Bearer ${lease.token}`, ...(text ? { "content-type": "application/json", "content-length": Buffer.byteLength(text) } : {}) },
      timeout: 10_000,
    }, (res) => {
      const chunks = [];
      res.on("data", (chunk) => chunks.push(chunk));
      res.on("end", () => {
        let parsed = {};
        try {
          parsed = JSON.parse(Buffer.concat(chunks).toString("utf8"));
        } catch {
          // not JSON
        }
        resolve({ status: res.statusCode || 0, body: parsed });
      });
    });
    req.on("timeout", () => req.destroy(new Error("timed out")));
    req.on("error", reject);
    req.end(text);
  });
}

/** A freshly started machine needs a few seconds before its server listens. */
async function waitForBuilder(lease, timeoutMs = 120_000) {
  const deadline = Date.now() + timeoutMs;
  let last = "";
  while (Date.now() < deadline) {
    try {
      const reply = await builderJson(lease, "GET", "/v1/health");
      if (reply.status === 200) return reply.body;
      if (reply.status === 401) throw new BurstError("the builder rejected its token");
      last = `HTTP ${reply.status}`;
    } catch (error) {
      if (error instanceof BurstError) throw error;
      last = error.message;
    }
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
  throw new BurstError(`the builder did not come up (${last})`);
}

function spawnExit(program, args, { cwd = process.cwd(), env = process.env, stdio = "inherit" } = {}) {
  return new Promise((resolve) => {
    const child = spawn(program, args, { cwd, env, stdio });
    child.on("close", (code, signal) => resolve(code ?? (signal ? 128 + 15 : 1)));
    child.on("error", () => resolve(127));
  });
}

function runLocal(command, cwd) {
  return spawnExit("bash", ["-c", command], { cwd });
}

/** No builder to use: the command still runs, on this machine. */
function runHere(reason, command, cwd) {
  process.stderr.write(`rudder-burst: ${reason}; running here instead\n`);
  return runLocal(command, cwd);
}

function rsync(args, env) {
  return spawnExit("rsync", args, { env, stdio: ["ignore", "inherit", "inherit"] });
}

/** Stream one command's output; resolves with its exit status. */
export function execRemote(lease, body, { stdout = process.stdout, stderr = process.stderr, signal } = {}) {
  return new Promise((resolve, reject) => {
    const text = JSON.stringify(body);
    const req = http.request(new URL("/v1/exec", lease.url), {
      method: "POST",
      headers: { authorization: `Bearer ${lease.token}`, "content-type": "application/json", "content-length": Buffer.byteLength(text) },
      signal,
    }, (res) => {
      if (res.statusCode !== 200) {
        const chunks = [];
        res.on("data", (chunk) => chunks.push(chunk));
        res.on("end", () => {
          let error = `HTTP ${res.statusCode}`;
          try {
            error = JSON.parse(Buffer.concat(chunks).toString("utf8")).error || error;
          } catch {
            // keep the status
          }
          reject(new BurstError(`the builder refused the command: ${error}`));
        });
        return;
      }
      let exit = null;
      const push = frameDecoder((type, payload) => {
        if (type === FRAME.stdout) stdout.write(payload);
        else if (type === FRAME.stderr) stderr.write(payload);
        else if (type === FRAME.exit) exit = JSON.parse(payload.toString("utf8"));
      });
      res.on("data", (chunk) => {
        try {
          push(chunk);
        } catch (error) {
          req.destroy(error);
        }
      });
      res.on("end", () => (exit ? resolve(exit) : reject(new BurstError("the connection to the builder dropped before the command finished"))));
      res.on("error", reject);
    });
    req.on("error", (error) => reject(error instanceof BurstError ? error : new BurstError(`lost the builder: ${error.message}`)));
    req.end(text);
  });
}

async function lease(socketPath, opts, holder) {
  const deadline = Date.now() + opts.wait * 60_000;
  for (;;) {
    let reply;
    try {
      reply = await brokerRequest(socketPath, "POST", "/lease", { seconds: Math.round(Math.max(1, opts.minutes) * 60), holder });
    } catch (error) {
      throw new BurstError(`the home supervisor did not answer (${error.message})`, EXIT.unavailable);
    }
    if (reply.status === 200) return reply.body;
    if (reply.status === 404) throw new BurstError(reply.body.error || "the build pool is not enabled", EXIT.unavailable);
    if (reply.status !== 429) throw new BurstError(reply.body.error || `lease failed (HTTP ${reply.status})`, reply.status >= 500 ? EXIT.unavailable : EXIT.failed);
    const retry = Math.max(5, Number(reply.body.retryAfterSeconds) || 30);
    if (Date.now() + retry * 1000 > deadline) throw new BurstError(reply.body.error || "every builder is busy", EXIT.busy);
    process.stderr.write(`rudder-burst: ${reply.body.error || "every builder is busy"}; retrying in ${retry}s\n`);
    await new Promise((resolve) => setTimeout(resolve, retry * 1000));
  }
}

async function status(socketPath) {
  const reply = await brokerRequest(socketPath, "GET", "/builders");
  if (reply.status !== 200) throw new BurstError(reply.body.error || `HTTP ${reply.status}`, reply.status === 404 ? EXIT.unavailable : EXIT.failed);
  const { policy, builders } = reply.body;
  process.stdout.write(`build pool: up to ${policy.maxPerHome} builders, ${policy.cpus} CPU / ${Math.round(policy.memoryMb / 1024)} GB each, stop after ${policy.idleSeconds}s idle\n`);
  if (!builders.length) process.stdout.write("no builders yet; the first rudder-burst creates one\n");
  for (const b of builders) {
    const state = b.leased ? `leased by ${b.leaseHolder || "?"} until ${b.leaseExpiresAt}` : b.status === "ready" ? `free, machine ${b.machineState || "unknown"}` : b.status;
    process.stdout.write(`  ${b.id} (slot ${b.slot}): ${state}${b.error ? ` [last error: ${b.error}]` : ""}\n`);
  }
  return 0;
}

export async function main(argv = process.argv.slice(2), env = process.env) {
  const opts = parseArgs(argv);
  const socketPath = env.RUDDER_BURST_SOCKET || "";
  const command = commandLine(opts.command);
  const cwd = process.cwd();
  if (!socketPath) {
    if (opts.action === "run") return await runHere("no build pool here (RUDDER_BURST_SOCKET is not set)", command, cwd);
    throw new BurstError("no build pool here: rudder-burst runs on Rudder Cloud homes (RUDDER_BURST_SOCKET is not set)", EXIT.unavailable);
  }
  if (opts.action === "status") return await status(socketPath);
  const repo = repoInfo(cwd);
  const key = opts.key ? safeKey(opts.key) : repo.key;
  let granted;
  try {
    granted = await lease(socketPath, opts, `${path.basename(repo.root)}: ${opts.action === "clean" ? "clean" : command}`.slice(0, 120));
  } catch (error) {
    if (opts.action === "run" && error instanceof BurstError && (error.code === EXIT.unavailable || error.code === EXIT.busy)) {
      return await runHere(error.message, command, cwd);
    }
    throw error;
  }
  const abort = new AbortController();
  let interrupted = false;
  const onSignal = () => {
    interrupted = true;
    abort.abort();
  };
  process.on("SIGINT", onSignal);
  process.on("SIGTERM", onSignal);
  const renew = setInterval(() => {
    void brokerRequest(socketPath, "POST", `/leases/${granted.leaseId}/renew`, { seconds: Math.round(Math.max(1, opts.minutes) * 60) }).catch(() => undefined);
  }, Math.max(30_000, Math.min(120_000, opts.minutes * 20_000)));
  try {
    const health = await waitForBuilder(granted);
    process.stderr.write(`rudder-burst: ${granted.builderId} (${health.cpus ?? granted.cpus} CPU, ${Math.round((health.memoryBytes ?? granted.memoryMb * 1048576) / 1073741824)} GB, ${granted.warm ? "warm" : "cold start"})\n`);
    if (opts.action === "clean") {
      const reply = await builderJson(granted, "POST", "/v1/clean", { key });
      if (reply.status !== 200) throw new BurstError(reply.body.error || `clean failed (HTTP ${reply.status})`);
      process.stderr.write(`rudder-burst: removed ${key} on ${granted.builderId}\n`);
      return 0;
    }
    const rsh = `${process.execPath} ${fileURLToPath(import.meta.url)} --rsh`;
    const rsyncEnv = { ...process.env, RUDDER_BURST_URL: granted.url, RUDDER_BURST_TOKEN: granted.token };
    if (opts.sync) {
      const started = Date.now();
      const code = await rsync(uploadArgs({ root: repo.root, key, git: opts.git, exclude: opts.exclude }, rsh), rsyncEnv);
      if (code !== 0) throw new BurstError(`upload failed (rsync exited ${code})`);
      process.stderr.write(`rudder-burst: uploaded in ${((Date.now() - started) / 1000).toFixed(1)}s\n`);
    }
    let result;
    try {
      result = await execRemote(granted, { key, cwd: repo.rel, command, env: opts.env }, { signal: abort.signal });
    } catch (error) {
      if (interrupted) return 130;
      throw error;
    }
    for (const pulled of opts.pull) {
      const { dest, args } = pullArgs({ root: repo.root, key }, pulled, rsh);
      fs.mkdirSync(dest, { recursive: true });
      const code = await rsync(args, rsyncEnv);
      if (code !== 0) process.stderr.write(`rudder-burst: could not pull ${pulled} (rsync exited ${code})\n`);
    }
    process.stderr.write(`rudder-burst: finished in ${Number(result.seconds || 0).toFixed(1)}s\n`);
    return result.code ?? (result.signal ? 128 + 9 : 1);
  } finally {
    clearInterval(renew);
    process.off("SIGINT", onSignal);
    process.off("SIGTERM", onSignal);
    await brokerRequest(socketPath, "POST", `/leases/${granted.leaseId}/release`).catch(() => undefined);
  }
}

/** rsync's remote shell: carry its stdio to the builder's rsync endpoint. */
function rsh(args, env = process.env) {
  let rest = args;
  if (rest[0] === "-l") rest = rest.slice(2);
  const remote = rest.slice(1);
  const req = http.request(new URL("/v1/rsync", env.RUDDER_BURST_URL), {
    headers: {
      connection: "Upgrade",
      upgrade: RSYNC_UPGRADE,
      authorization: `Bearer ${env.RUDDER_BURST_TOKEN || ""}`,
      "x-rudder-rsync-argv": Buffer.from(JSON.stringify(remote)).toString("base64url"),
    },
  });
  req.on("upgrade", (_res, socket, head) => {
    if (head.length) process.stdout.write(head);
    process.stdin.pipe(socket);
    socket.pipe(process.stdout);
    socket.on("close", () => process.stdout.write("", () => process.exit(0)));
    socket.on("error", () => process.exit(1));
  });
  req.on("response", (res) => {
    process.stderr.write(`rudder-burst: the builder refused rsync (HTTP ${res.statusCode})\n`);
    process.exit(1);
  });
  req.on("error", (error) => {
    process.stderr.write(`rudder-burst: ${error.message}\n`);
    process.exit(1);
  });
  req.end();
}

if (process.argv[1] && import.meta.url === pathToFileURL(fs.realpathSync(process.argv[1])).href) {
  const argv = process.argv.slice(2);
  if (argv[0] === "--rsh") {
    rsh(argv.slice(1));
  } else {
    main(argv).then((code) => process.exit(code), (error) => {
      process.stderr.write(error.code === 0 ? `${error.message}\n` : `rudder-burst: ${error.message}\n`);
      process.exit(error instanceof BurstError ? error.code : EXIT.failed);
    });
  }
}
