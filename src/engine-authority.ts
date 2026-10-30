import fs from "node:fs";
import path from "node:path";

/** A projection selects the project's migration mode. It is not a lease or
 * proof that the owner is alive; native verifies the lock, epoch and socket.
 */
export function projectEngineAuthority(repo: string): "fabric" | "legacy-native" | "legacy-ts" | undefined {
  let raw: string;
  try { raw = fs.readFileSync(path.join(repo, ".rudder", "engine", "owner.json"), "utf8"); }
  catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
    throw error;
  }
  let value: unknown;
  try { value = JSON.parse(raw); } catch { throw new Error("Project engine ownership record is unreadable; recover the engine before starting a writer."); }
  const data = value as { format_version?: unknown; authority?: unknown } | null;
  if (!data || data.format_version !== 1 || !["fabric", "legacy-native", "legacy-ts"].includes(String(data.authority))) {
    throw new Error("Unsupported project engine authority; use a compatible Rudder engine.");
  }
  return data.authority as "fabric" | "legacy-native" | "legacy-ts";
}

export function assertLegacyWriterAllowed(repo: string): void {
  if (projectEngineAuthority(repo) === "fabric") {
    throw new Error("This project uses the persistent engine. Use its dashboard or machine API so commands retain the current owner epoch.");
  }
}
