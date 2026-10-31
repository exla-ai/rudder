import { spawn } from "node:child_process";
import path from "node:path";
import { resolveNativeBinaryPath } from "./native-binary.js";
import { validateFabricReference, type FabricObjectRef } from "./fabric-remote.js";

type ObjectRef = FabricObjectRef;
export type CloudBuildRequest = {
  format_version: 1;
  store: string;
  index: string;
  scratch: string;
  buck2: string;
  runnable: ObjectRef;
  expected_projection_policy: ObjectRef;
};
type Options = { nativeBinary?: string; timeoutMs?: number; signal?: AbortSignal };
const MAX_REQUEST = 64 * 1024;
const MAX_OUTPUT = 256 * 1024;

function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Invalid build-profile response");
  return value as Record<string, unknown>;
}
function reference(value: unknown, kinds: string[]): ObjectRef {
  const ref = validateFabricReference(value);
  if (!kinds.includes(ref.kind)) throw new Error("Invalid build-profile object reference kind");
  return ref;
}
function count(value: unknown): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0) throw new Error("Invalid build-profile action count");
  return Number(value);
}

export function parseCloudBuildResponse(raw: string, request: CloudBuildRequest) {
  const data = record(JSON.parse(raw));
  const runnable = reference(data.runnable, ["runnable_root"]);
  if (data.format_version !== 1 || data.kind !== "managed-build-data" || data.outputs_verified !== true
      || runnable.digest !== request.runnable.digest || runnable.size_bytes !== request.runnable.size_bytes
      || count(data.output_object_count) < 1 || typeof data.workspace !== "string"
      || path.dirname(data.workspace) !== request.scratch || !/^build-[a-f0-9-]+$/.test(path.basename(data.workspace))) {
    throw new Error("Build-profile response does not match the retained input or private scratch");
  }
  reference(data.profile, ["blob", "file"]);
  const build = record(data.build), stored = record(build.stored), observation = record(build.observation);
  if (build.format_version !== 1 || stored.requires_frontend_validation !== true
      || observation.frontend !== "buck2-rudder-linux-compile-v1" || typeof observation.trace_id !== "string" || !observation.trace_id) {
    throw new Error("Build-profile response lacks qualified frontend evidence");
  }
  reference(build.run_receipt, ["action_result"]);
  reference(stored.key, ["action_key"]);
  reference(stored.receipt, ["action_result"]);
  reference(stored.outputs, ["directory"]);
  reference(observation.event_log, ["blob"]);
  reference(observation.build_report, ["blob"]);
  const targets = count(data.target_count);
  if (targets < 1 || targets > 256 || count(observation.executed_actions) + count(observation.remote_cache_hits)
      + count(build.frontend_graph_reused_actions) !== targets) throw new Error("Incomplete build-profile action accounting");
  if (!Array.isArray(observation.native_action_digests) || (Number(observation.remote_cache_hits) > 0 && observation.native_action_digests.length === 0)) {
    throw new Error("Build-profile cache observation lacks native action digests");
  }
  for (const value of observation.native_action_digests) {
    const digest = record(value);
    reference({ kind: "blob", algorithm: "sha256", digest: digest.hash, size_bytes: digest.size_bytes }, ["blob"]);
  }
  return data;
}

/** Build DATA only. Callers must already own the retained runnable root and its
 * trusted source projection policy. This never creates launch authority. */
export async function runCloudBuildProfile(request: CloudBuildRequest, options: Options = {}) {
  if (request.format_version !== 1) throw new Error("Unsupported build-profile request");
  reference(request.runnable, ["runnable_root"]);
  reference(request.expected_projection_policy, ["blob"]);
  for (const name of ["store", "index", "scratch", "buck2"] as const) {
    if (typeof request[name] !== "string" || !path.isAbsolute(request[name]) || request[name].includes("\0")) throw new Error("Build-profile paths must be absolute");
  }
  const raw = JSON.stringify(request);
  if (Buffer.byteLength(raw) > MAX_REQUEST) throw new Error("Build-profile request exceeds 64 KiB");
  const timeout = options.timeoutMs ?? 300_000;
  if (!Number.isSafeInteger(timeout) || timeout < 1 || timeout > 1_200_000) throw new Error("Build-profile deadline must be 1..1200000 ms");
  options.signal?.throwIfAborted();
  const binary = options.nativeBinary ?? resolveNativeBinaryPath();
  if (!binary) throw new Error("rudder-native binary is not available. Reinstall or rebuild Rudder.");
  const stdout = await new Promise<string>((resolve, reject) => {
    // Do not forward provider credentials to the data-only build helper.
    const child = spawn(binary, ["fabric", "build-profile"], { detached: process.platform !== "win32",
      env: { PATH: process.env.PATH ?? "/usr/bin:/bin" }, stdio: ["pipe", "pipe", "pipe"] });
    const out: Buffer[] = [], err: Buffer[] = [];
    let bytes = 0, failure: Error | undefined, reap: NodeJS.Timeout | undefined;
    const stop = (error: Error) => {
      if (failure) return;
      failure = error;
      if (child.pid) {
        try { process.kill(process.platform === "win32" ? child.pid : -child.pid, "SIGKILL"); }
        catch { child.kill("SIGKILL"); }
      }
      // An unresponsive OS process can leave effects uncertain. Never retry or
      // translate the deadline/abort into a successful receipt or stopped fact.
      reap = setTimeout(() => { cleanup(); reject(error); }, 2_000);
    };
    const timer = setTimeout(() => stop(new Error("Build-profile deadline exceeded; build effects require reconciliation")), timeout);
    const abort = () => stop(new Error("Build-profile cancelled; build effects require reconciliation"));
    const cleanup = () => { clearTimeout(timer); clearTimeout(reap); options.signal?.removeEventListener("abort", abort); };
    for (const [stream, target] of [[child.stdout, out], [child.stderr, err]] as const) {
      stream.on("data", (chunk: Buffer) => {
        bytes += chunk.length;
        if (bytes > MAX_OUTPUT) stop(new Error("Build-profile output exceeds 256 KiB; build effects require reconciliation"));
        else target.push(chunk);
      });
    }
    child.stdin.on("error", (error) => stop(error));
    child.once("error", (error) => { cleanup(); reject(error); });
    child.once("close", (code) => {
      cleanup();
      if (failure) reject(failure);
      else if (code !== 0) reject(new Error(`Build-profile failed (${code}): ${Buffer.concat(err).toString("utf8").slice(-8_000)}`));
      else resolve(Buffer.concat(out).toString("utf8"));
    });
    options.signal?.addEventListener("abort", abort, { once: true });
    if (options.signal?.aborted) abort();
    child.stdin.end(raw);
  });
  // Canonical paths are returned by the helper. Resolve aliases before checking
  // private scratch binding, without touching build outputs ourselves.
  const { realpath } = await import("node:fs/promises");
  return parseCloudBuildResponse(stdout, { ...request, scratch: await realpath(request.scratch) });
}
