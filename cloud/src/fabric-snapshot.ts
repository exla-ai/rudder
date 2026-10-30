/** Ancestry-free launch metadata. History stays in explicit history authority;
 * old Source/Environment/Checkpoint formats keep all their existing edges. */
import { createHash } from "node:crypto";
import { ContentFormatError, unsigned, validateContentReference,
  type CanonicalMetadataCodec, type ContentObjectKind, type ContentObjectRef } from "./fabric-content.js";

export const MAX_RUNNABLE_MANIFEST_BYTES = 64 * 1024;
const MAX_POLICY_BYTES = 1024n * 1024n;
const MAX_TOPOLOGY_BYTES = 64n * 1024n * 1024n;
export interface SourceV2 { format_version: 2; consistency: "quiesced"; directory: ContentObjectRef; projection_policy: ContentObjectRef }
export interface EnvironmentV2 { format_version: 2; directory: ContentObjectRef; platform: Record<string, string>; installer_inputs: ContentObjectRef[]; provenance: Record<string, string> }
export interface RunnableRoot { format_version: 1; consistency: "quiesced"; source: ContentObjectRef; environment: ContentObjectRef | null; view_root: ContentObjectRef; profile: ContentObjectRef; hardlinks: ContentObjectRef; knowledge_cursor: bigint }
function fail(message: string): never { throw new ContentFormatError(message); }
function exact(value: unknown, keys: readonly string[]): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) fail("Expected a runnable metadata object");
  const object = value as Record<string, unknown>;
  if (Object.keys(object).length !== keys.length || keys.some((key) => !Object.hasOwn(object, key))) fail("Unsupported or missing runnable metadata field");
  return object;
}
function reference(value: unknown, kind: ContentObjectKind): ContentObjectRef {
  const object = validateContentReference(value);
  if (object.kind !== kind) fail("Incorrect runnable-state reference kind");
  return object;
}
function blob(value: unknown, limit: bigint): ContentObjectRef {
  const object = reference(value, "blob");
  if (object.size_bytes > limit) fail("Runnable-state blob exceeds its format bound");
  return object;
}
function stringMap(value: unknown): Record<string, string> {
  if (!value || typeof value !== "object" || Array.isArray(value) || Object.values(value).some((item) => typeof item !== "string")) fail("Expected a runnable string map");
  return value as Record<string, string>;
}
function bounded<T>(value: T, codec: CanonicalMetadataCodec): T {
  if (codec.encodeCanonical(value).byteLength > MAX_RUNNABLE_MANIFEST_BYTES) fail("Runnable manifest exceeds64KiB");
  return value;
}
export function validateSourceV2(value: unknown, codec: CanonicalMetadataCodec): SourceV2 {
  const object = exact(value, ["format_version", "consistency", "directory", "projection_policy"]);
  if (unsigned(object.format_version) !== 2n || object.consistency !== "quiesced") fail("Runnable source requires quiesced schema2 without ancestry");
  return bounded({ format_version: 2, consistency: "quiesced", directory: reference(object.directory, "directory"), projection_policy: blob(object.projection_policy, MAX_POLICY_BYTES) }, codec);
}
export function validateEnvironmentV2(value: unknown, codec: CanonicalMetadataCodec): EnvironmentV2 {
  const object = exact(value, ["format_version", "directory", "platform", "installer_inputs", "provenance"]);
  if (unsigned(object.format_version) !== 2n || !Array.isArray(object.installer_inputs)) fail("Runnable environment requires schema2");
  const platform = stringMap(object.platform);
  if (!Object.keys(platform).length) fail("Environment platform/ABI identity is required");
  const installer_inputs = (object.installer_inputs as unknown[]).map(validateContentReference);
  if (installer_inputs.some((ref) => !["blob", "file", "directory"].includes(ref.kind))) fail("Installer inputs must be content roots, without history/evidence edges");
  return bounded({ format_version: 2, directory: reference(object.directory, "directory"), platform, installer_inputs, provenance: stringMap(object.provenance) }, codec);
}
export function validateRunnableRoot(value: unknown, codec: CanonicalMetadataCodec): RunnableRoot {
  const object = exact(value, ["format_version", "consistency", "source", "environment", "view_root", "profile", "hardlinks", "knowledge_cursor"]);
  if (unsigned(object.format_version) !== 1n || object.consistency !== "quiesced") fail("Unsupported runnable-root schema/consistency");
  return bounded({ format_version: 1, consistency: "quiesced", source: reference(object.source, "source_root"), environment: object.environment === null ? null : reference(object.environment, "environment_root"), view_root: reference(object.view_root, "directory"), profile: blob(object.profile, MAX_POLICY_BYTES), hardlinks: blob(object.hardlinks, MAX_TOPOLOGY_BYTES), knowledge_cursor: unsigned(object.knowledge_cursor) }, codec);
}
function decode<T>(bytes: Buffer, codec: CanonicalMetadataCodec, validate: (value: unknown, codec: CanonicalMetadataCodec) => T): T {
  if (bytes.byteLength > MAX_RUNNABLE_MANIFEST_BYTES) fail("Runnable manifest exceeds64KiB");
  const value = validate(codec.parseCanonical(bytes), codec);
  if (!codec.encodeCanonical(value).equals(bytes)) fail("Runnable metadata must use canonical compact JSON");
  return value;
}
export const decodeSourceV2 = (bytes: Buffer, codec: CanonicalMetadataCodec): SourceV2 => decode(bytes, codec, validateSourceV2);
export const decodeEnvironmentV2 = (bytes: Buffer, codec: CanonicalMetadataCodec): EnvironmentV2 => decode(bytes, codec, validateEnvironmentV2);
export const decodeRunnableRoot = (bytes: Buffer, codec: CanonicalMetadataCodec): RunnableRoot => decode(bytes, codec, validateRunnableRoot);
export function snapshotReferences(kind: "source_root" | "environment_root" | "runnable_root", value: unknown, codec: CanonicalMetadataCodec): ContentObjectRef[] {
  if (kind === "source_root") { const source = validateSourceV2(value, codec); return [source.directory, source.projection_policy]; }
  if (kind === "environment_root") { const environment = validateEnvironmentV2(value, codec); return [environment.directory, ...environment.installer_inputs]; }
  const root = validateRunnableRoot(value, codec);
  return [root.source, root.view_root, root.profile, root.hardlinks, ...(root.environment ? [root.environment] : [])];
}
export function validateRunnableChild(parentValue: unknown, childValue: unknown, childBytes: Buffer, codec: CanonicalMetadataCodec): void {
  const parent = validateRunnableRoot(parentValue, codec), child = validateContentReference(childValue);
  if (BigInt(childBytes.byteLength) !== child.size_bytes || createHash("sha256").update(childBytes).digest("hex") !== child.digest) fail("Runnable child differs from its typed reference");
  const same = (ref: ContentObjectRef | null) => ref && ref.kind === child.kind && ref.digest === child.digest && ref.size_bytes === child.size_bytes;
  if (same(parent.source)) { decodeSourceV2(childBytes, codec); return; }
  if (same(parent.environment)) { decodeEnvironmentV2(childBytes, codec); return; }
  fail("Child is not the runnable source or environment");
}
