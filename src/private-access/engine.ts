import { execFile } from "node:child_process";
import { realpath } from "node:fs/promises";
import path from "node:path";
import { parseEngineStatus } from "../engine.js";
import { resolveNativeBinaryPath } from "../native-binary.js";
import { validateFileScope } from "../project-files/service.js";
import type { FileScope } from "../project-files/types.js";

export type RegisteredPreviewProcess = {
  version: 1; role: "preview"; action: "register"; scope: FileScope;
  owner: { project_id: string; owner_id: string; epoch: number };
  workspacePath: string; rootPid: number; pid: number; processStart: string; protocol: "http" | "https";
};
/** Private owner socket supplies current process custody, never projected state,
 * arbitrary cwd, or an unreceipted Home process launch. */
export async function queryPreviewOwner(repo: string, operation: "previews" | "preview-register", request?: unknown,
  signal = AbortSignal.timeout(3000), nativeBinary = resolveNativeBinaryPath()): Promise<RegisteredPreviewProcess[] | unknown> {
  signal.throwIfAborted();
  if (!nativeBinary) throw new Error("Current preview authority is unavailable");
  const root = await realpath(repo);
  const args = ["engine", operation, "--repo", root, "--json"];
  if (operation === "preview-register") {
    const encoded = JSON.stringify(request);
    if (!encoded || Buffer.byteLength(encoded) > 4096) throw new Error("Preview registration exceeds bound");
    args.push("--preview-json", encoded);
  }
  const raw = await new Promise<string>((resolve, reject) => execFile(nativeBinary, args,
    { cwd: root, signal, timeout: 3000, maxBuffer: 128 * 1024 },
    (error, stdout) => error ? reject(new Error("Current preview authority is unavailable")) : resolve(stdout)));
  const status = parseEngineStatus(raw);
  if (!status.running || status.authority !== "fabric" || !status.owner || await realpath(status.repo) !== root) {
    throw new Error("Current preview authority is unavailable");
  }
  const output = (JSON.parse(raw) as { previews?: unknown; preview?: unknown });
  signal.throwIfAborted();
  if (operation === "preview-register") return output.preview;
  const values = output.previews;
  if (!Array.isArray(values) || values.length > 64) throw new Error("Current preview bindings exceed bound");
  return values.map((raw: unknown) => {
    const value = raw as RegisteredPreviewProcess;
    const scope = validateFileScope(value?.scope);
    if (value.version !== 1 || value.role !== "preview" || value.action !== "register"
      || value.owner?.project_id !== status.owner!.project_id || value.owner?.owner_id !== status.owner!.owner_id
      || value.owner?.epoch !== status.owner!.epoch || scope.projectId !== status.owner!.project_id || scope.ownerEpoch !== status.owner!.epoch
      || typeof value.workspacePath !== "string" || !path.isAbsolute(value.workspacePath) || value.workspacePath.length > 4096
      || !Number.isSafeInteger(value.pid) || value.pid <= 1 || !Number.isSafeInteger(value.rootPid) || value.rootPid <= 1
      || typeof value.processStart !== "string" || !/^\d{1,32}$/.test(value.processStart) || !["http", "https"].includes(value.protocol)) {
      throw new Error("Current preview binding changed");
    }
    return { ...value, scope };
  });
}
