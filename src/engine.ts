import { execFile, spawn } from "node:child_process";
import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";

import { codexLaunchEnv } from "./codex-binary.js";
import { resolveNativeBinaryPath } from "./native-binary.js";
import { projectEngineAuthority } from "./engine-authority.js";

export type EngineStatus = {
  format_version: 1;
  running: boolean;
  repo: string;
  authority: "fabric" | "legacy-native" | "legacy-ts";
  implementation?: string;
  state: string;
  pid?: number | null;
  owner?: { project_id: string; owner_id: string; epoch: number } | null;
  instance_id?: string | null;
  attach_socket?: string;
  control_socket?: string;
  started_at?: string;
};

type EngineOptions = {
  nativeBinary?: string;
  env?: NodeJS.ProcessEnv;
  timeoutMs?: number;
};

function binaryPath(options: EngineOptions): string {
  const binary = options.nativeBinary ?? resolveNativeBinaryPath();
  if (!binary) throw new Error("rudder-native binary is not available. Reinstall or rebuild Rudder.");
  return binary;
}

export function parseEngineStatus(raw: string): EngineStatus {
  const status: unknown = JSON.parse(raw);
  if (!status || typeof status !== "object") throw new Error("Invalid engine status response");
  const data = status as Record<string, unknown>;
  if (data.format_version !== 1 || typeof data.running !== "boolean" || typeof data.repo !== "string"
      || !["fabric", "legacy-native", "legacy-ts"].includes(String(data.authority)) || typeof data.state !== "string") {
    throw new Error("Unsupported or invalid engine status response");
  }
  if (data.running && data.authority === "fabric") {
    const owner = data.owner as Record<string, unknown> | null;
    if (typeof data.instance_id !== "string" || !data.instance_id || typeof data.attach_socket !== "string"
        || !owner || typeof owner.project_id !== "string" || typeof owner.owner_id !== "string"
        || !Number.isSafeInteger(owner.epoch) || Number(owner.epoch) < 1) {
      throw new Error("Engine status is missing its ownership identity");
    }
  }
  return status as EngineStatus;
}

async function queryEngine(action: "status" | "shutdown", repo: string, options: EngineOptions): Promise<EngineStatus> {
  const binary = binaryPath(options);
  const raw = await new Promise<string>((resolve, reject) => {
    execFile(binary, ["engine", action, "--repo", repo, "--json"], {
      cwd: repo,
      env: options.env ?? process.env,
      timeout: options.timeoutMs ?? 3_000,
      maxBuffer: 64 * 1024,
    }, (error, stdout, stderr) => {
      if (error) reject(new Error(stderr.trim() || `Engine ${action} failed: ${error.message}`));
      else resolve(stdout.trim());
    });
  });
  const status = parseEngineStatus(raw);
  if (await fsp.realpath(status.repo) !== await fsp.realpath(repo)) throw new Error("Engine returned a different checkout identity");
  return status;
}

export async function readEngineStatus(repo: string, options: EngineOptions = {}): Promise<EngineStatus> {
  return queryEngine("status", await fsp.realpath(repo), options);
}

export async function shutdownEngine(repo: string, options: EngineOptions = {}): Promise<EngineStatus> {
  const root = await fsp.realpath(repo);
  const status = await queryEngine("shutdown", root, options);
  if (!status.running) return status;
  const deadline = Date.now() + (options.timeoutMs ?? 12_000);
  do {
    const current = await readEngineStatus(root, { ...options, timeoutMs: 2_000 });
    if (!current.running) return current;
    if (current.instance_id !== status.instance_id) throw new Error("Engine ownership changed while waiting for shutdown");
    await new Promise((resolve) => setTimeout(resolve, 75));
  } while (Date.now() < deadline);
  throw new Error("Engine accepted shutdown but has not yet confirmed it stopped");
}

/** The child owns the lifecycle. Closing the caller's terminal only closes a view. */
export async function ensureEngineRunning(repo: string, options: EngineOptions = {}): Promise<EngineStatus> {
  const root = await fsp.realpath(repo);
  const current = await readEngineStatus(root, options);
  if (current.running) {
    if (current.authority !== "fabric") throw new Error(`The ${current.authority} owner is still running; close it before transferring this project to the persistent engine.`);
    return current;
  }

  const stateDir = path.join(root, ".rudder", "engine");
  await fsp.mkdir(stateDir, { recursive: true, mode: 0o700 });
  const logFile = path.join(stateDir, "engine.log");
  // Bound historical startup diagnostics without truncating a live owner's log.
  const oldLog = await fsp.stat(logFile).catch(() => null);
  if (oldLog && oldLog.size > 8 * 1024 * 1024) {
    await fsp.rename(logFile, `${logFile}.previous`).catch(() => undefined);
  }
  const fd = fs.openSync(logFile, "a", 0o600);
  let startupError: Error | undefined;
  const env = options.env ?? await codexLaunchEnv(process.env);
  try {
    const child = spawn(binaryPath(options), ["engine", "--repo", root], {
      cwd: root,
      detached: true,
      stdio: ["ignore", fd, fd],
      env: { ...env, RUDDER_NATIVE_TUI: "1" },
    });
    child.once("error", (error) => { startupError = error; });
    child.unref();
  } finally {
    fs.closeSync(fd);
  }

  const deadline = Date.now() + (options.timeoutMs ?? 12_000);
  do {
    if (startupError) throw startupError;
    const status = await readEngineStatus(root, { ...options, timeoutMs: 2_000 });
    if (status.running) {
      if (status.authority !== "fabric") throw new Error("A legacy owner acquired the project during engine startup");
      return status;
    }
    await new Promise((resolve) => setTimeout(resolve, 75));
  } while (Date.now() < deadline);
  throw new Error(`Persistent engine did not become ready. Startup diagnostics: ${logFile}`);
}

/** Explicit migration opt-in until the remaining legacy ingress paths are fenced. */
export function persistentEngineRequested(env: NodeJS.ProcessEnv = process.env): boolean {
  if (env.RUDDER_ENGINE === undefined || env.RUDDER_ENGINE === "legacy-native") return false;
  if (env.RUDDER_ENGINE === "fabric") return true;
  throw new Error("RUDDER_ENGINE must be fabric or legacy-native");
}

export function persistentProjectEngineRequested(repo: string, env: NodeJS.ProcessEnv = process.env): boolean {
  return env.RUDDER_ENGINE === undefined ? projectEngineAuthority(repo) === "fabric" : persistentEngineRequested(env);
}

export async function runEngineCommand(action: string | undefined, repo: string, json = false): Promise<void> {
  const op = action ?? "status";
  if (op === "api") {
    const code = await new Promise<number | null>((resolve, reject) => {
      const child = spawn(binaryPath({}), ["fabric"], { stdio: "inherit", env: process.env });
      child.once("error", reject);
      child.once("exit", resolve);
    });
    process.exitCode = code ?? 1;
    return;
  }
  let status: EngineStatus;
  if (op === "start") status = await ensureEngineRunning(repo);
  else if (op === "status") status = await readEngineStatus(repo);
  else if (op === "shutdown" || op === "stop") status = await shutdownEngine(repo);
  else throw new Error("Usage: rudder engine [start|status|shutdown] [--repo PATH] [--json]");
  process.stdout.write(json ? `${JSON.stringify(status)}\n` : status.running
    ? `Engine ready in ${status.repo} (owner epoch ${status.owner?.epoch ?? "legacy"})\n`
    : `Engine stopped in ${status.repo}\n`);
}
