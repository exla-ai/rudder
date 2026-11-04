import type { Readable, Writable } from "node:stream";
import { captureLegacyProject, prepareLegacyProjectRecovery } from "./capture.js";

/** Native owns the real migration lock for this call. No credentials are read,
 * no scheduler is started, and the recovery bundle stays private on the host. */
export async function runMigrationCaptureCommand(input: Readable = process.stdin, output: Writable = process.stdout): Promise<void> {
  const chunks: Buffer[] = []; let size = 0;
  for await (const chunk of input) {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    size += bytes.length;
    if (size > 256 * 1024) throw new Error("Migration capture input exceeds bound");
    chunks.push(bytes);
  }
  const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));
  if (request?.operation === "export") {
    if (typeof request !== "object" || Array.isArray(request)
      || Object.keys(request).some(key=>!["operation","bundle","destination","projectId","migrationId","rollbackCheckpoint"].includes(key))
      || ![request.bundle, request.destination, request.projectId, request.migrationId].every(value=>typeof value === "string" && value.length>0 && value.length<=4096)
      || (request.rollbackCheckpoint !== undefined && typeof request.rollbackCheckpoint !== "string")) throw new Error("Invalid migration export request");
    const {operation: _, ...options} = request;
    const exported = await prepareLegacyProjectRecovery(options);
    output.write(JSON.stringify({format_version:1,exported:true,integrationPath:exported.integrationPath})+"\n");
    return;
  }
  if (!request || typeof request !== "object" || Array.isArray(request)
    || Object.keys(request).some(key => !["operation", "repository", "bundle", "projectId", "migrationId", "sessions", "rollbackCheckpoint"].includes(key))
    || (request.operation !== undefined && request.operation !== "capture")
    || ![request.repository, request.bundle, request.projectId, request.migrationId].every(value => typeof value === "string" && value.length > 0)
    || !Array.isArray(request.sessions) || request.sessions.length > 1024
    || request.sessions.some((session: { runId?: unknown; files?: unknown }) => !session || typeof session.runId !== "string"
      || !Array.isArray(session.files) || session.files.length > 64 || session.files.some(file => typeof file !== "string"))) throw new Error("Invalid migration capture request");
  const {operation: _, ...options} = request;
  await captureLegacyProject(options);
  output.write('{"format_version":1,"captured":true}\n');
}
