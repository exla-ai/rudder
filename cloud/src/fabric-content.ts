/** Pure file-format parity with engine::content. Object references describe
 * encoded bytes; logical lengths stay bigint and are not Cloud quota checks.
 * Callers SHA-bind metadata to its reference before validating graph edges.
 * Codec injection avoids a runtime dependency on the CAS/HTTP implementation.
 */

export const FILE_CHUNK_BYTES = 4 * 1024 * 1024;
export const FILE_FANOUT = 256;
export const MAX_FILE_NODE_BYTES = 64 * 1024;
export const MAX_FILE_LOGICAL_BYTES = (1n << 63n) - 1n;
export const FILE_LAYOUT = "fixed_4mib_fanout256_v1";
const MAX_LEVEL = 5;
const MAX_U64 = (1n << 64n) - 1n;
const kinds = new Set(["blob", "file", "directory", "directory_shard", "source_root", "environment_root", "checkpoint", "runnable_root", "action_key", "action_result"]);

export type ContentObjectKind = "blob" | "file" | "directory" | "directory_shard" | "source_root" | "environment_root" | "checkpoint" | "runnable_root" | "action_key" | "action_result";
export interface ContentObjectRef {
  kind: ContentObjectKind;
  algorithm: "sha256";
  digest: string;
  size_bytes: bigint;
}
export interface CanonicalMetadataCodec {
  parseCanonical(bytes: Buffer): unknown;
  encodeCanonical(value: unknown): Buffer;
}
export interface FileNode {
  format_version: 1;
  layout: typeof FILE_LAYOUT;
  level: number;
  logical_size: bigint;
  children: ContentObjectRef[];
}
export class ContentFormatError extends Error {
  readonly code = "invalid_metadata";
  readonly status = 400;
  constructor(message: string) { super(message); this.name = "ContentFormatError"; }
}
function fail(message: string): never { throw new ContentFormatError(message); }
function exact(value: unknown, keys: readonly string[]): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) fail("Expected a content metadata object");
  const object = value as Record<string, unknown>;
  if (Object.keys(object).length !== keys.length || keys.some((key) => !Object.hasOwn(object, key))) fail("Unsupported or missing content metadata field");
  return object;
}

/** Format integer conversion, independent of HTTP/storage quotas. Unsafe JS
 * numbers are rejected instead of preserving an already-rounded value. */
export function unsigned(value: unknown, max: bigint | number = MAX_U64): bigint {
  if (typeof max === "number" && (!Number.isSafeInteger(max) || max < 0)) fail("Invalid unsigned integer bound");
  const limit = BigInt(max);
  if (limit < 0n || limit > MAX_U64) fail("Invalid unsigned integer bound");
  let integer: bigint;
  if (typeof value === "bigint") integer = value;
  else if (typeof value === "number" && Number.isSafeInteger(value) && !Object.is(value, -0)) integer = BigInt(value);
  else return fail("Content integers require lossless unsigned values");
  if (integer < 0n || integer > limit) fail("Content integer exceeds its format bound");
  return integer;
}

export function validateContentReference(value: unknown): ContentObjectRef {
  const object = exact(value, ["kind", "algorithm", "digest", "size_bytes"]);
  if (typeof object.kind !== "string" || !kinds.has(object.kind) || object.algorithm !== "sha256" || typeof object.digest !== "string" || !/^[a-f0-9]{64}$/.test(object.digest)) fail("Invalid typed content reference");
  return { kind: object.kind as ContentObjectKind, algorithm: "sha256", digest: object.digest, size_bytes: unsigned(object.size_bytes) };
}

function capacity(level: number): bigint {
  if (!Number.isInteger(level) || level < 0 || level > MAX_LEVEL) fail("File tree level exceeds supported logical size");
  const size = BigInt(FILE_CHUNK_BYTES) * BigInt(FILE_FANOUT) ** BigInt(level + 1);
  return size < MAX_FILE_LOGICAL_BYTES ? size : MAX_FILE_LOGICAL_BYTES;
}
function minimalLevel(size: bigint): number {
  if (size <= 0n || size > MAX_FILE_LOGICAL_BYTES) fail("Invalid chunked file logical size");
  for (let level = 0; level <= MAX_LEVEL; level++) if (size <= capacity(level)) return level;
  return fail("File exceeds supported logical size");
}

/** Parent validity is a separate precondition, as in Rust FileNode::child_span. */
export function fileChildSpan(node: FileNode, index: number): bigint {
  if (!Number.isSafeInteger(index) || index < 0 || index >= node.children.length) fail("File child index is out of range");
  const childCapacity = node.level === 0 ? BigInt(FILE_CHUNK_BYTES) : capacity(node.level - 1);
  const start = childCapacity * BigInt(index);
  if (start >= node.logical_size) fail("File child exceeds logical span");
  const remaining = node.logical_size - start;
  return remaining < childCapacity ? remaining : childCapacity;
}

export function validateFileNode(value: unknown): FileNode {
  const object = exact(value, ["format_version", "layout", "level", "logical_size", "children"]);
  if (unsigned(object.format_version) !== 1n || object.layout !== FILE_LAYOUT) fail("Unsupported file layout");
  const level = Number(unsigned(object.level, MAX_LEVEL));
  const logical_size = unsigned(object.logical_size, MAX_FILE_LOGICAL_BYTES);
  if (level !== minimalLevel(logical_size)) fail("File tree is not minimally shaped");
  if (!Array.isArray(object.children) || object.children.length === 0 || object.children.length > FILE_FANOUT) fail("Invalid file child count");
  const childCapacity = level === 0 ? BigInt(FILE_CHUNK_BYTES) : capacity(level - 1);
  const expected = (logical_size - 1n) / childCapacity + 1n;
  if (BigInt(object.children.length) !== expected) fail("File children do not cover logical span");
  const node: FileNode = { format_version: 1, layout: FILE_LAYOUT, level, logical_size, children: object.children.map(validateContentReference) };
  for (const [index, child] of node.children.entries()) {
    if (level === 0) {
      if (child.kind !== "blob" || child.size_bytes !== fileChildSpan(node, index)) fail("Invalid file chunk kind or length");
    } else if (child.kind !== "file" || child.size_bytes <= 0n || child.size_bytes > BigInt(MAX_FILE_NODE_BYTES)) fail("Invalid file descriptor child");
  }
  return node;
}

export function decodeFileNode(bytes: Buffer, codec: CanonicalMetadataCodec): FileNode {
  if (bytes.byteLength > MAX_FILE_NODE_BYTES) fail("File descriptor exceeds metadata limit");
  const node = validateFileNode(codec.parseCanonical(bytes));
  const encoded = codec.encodeCanonical(node);
  if (encoded.byteLength > MAX_FILE_NODE_BYTES || !encoded.equals(bytes)) fail("File descriptor must use canonical compact JSON");
  return node;
}

export function fileNodeReferences(bytes: Buffer, codec: CanonicalMetadataCodec): ContentObjectRef[] {
  return decodeFileNode(bytes, codec).children;
}

/** Pure cross-object semantics. Caller must bind the decoded child bytes to
 * parent.children[index] with the ordinary typed SHA256/size verification. */
export function validateFileChild(parentValue: unknown, index: number, childValue: unknown): void {
  const parent = validateFileNode(parentValue), child = validateFileNode(childValue);
  if (parent.level === 0) fail("Raw chunks have no file descriptor children");
  const span = fileChildSpan(parent, index);
  if (child.logical_size !== span || child.level !== minimalLevel(span)) fail("File child logical span or level differs from parent");
}
