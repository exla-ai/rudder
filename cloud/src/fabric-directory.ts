/** Pure parity with engine::directory (Directory v2 / shard layout v1).
 * Codec callbacks keep this module independent of CAS, S3 and HTTP. Counts,
 * logical lengths and reference lengths are lossless format integers; deployment
 * quotas are a separate admission policy owned by the caller.
 */
import { createHash } from "node:crypto";
import {
  ContentFormatError, MAX_FILE_LOGICAL_BYTES, unsigned,
  validateContentReference, validateFileNode,
  type CanonicalMetadataCodec, type ContentObjectRef,
} from "./fabric-content.js";

export const DIRECTORY_FORMAT_VERSION = 2;
export const DIRECTORY_SHARD_FORMAT_VERSION = 1;
export const DIRECTORY_MAX_PAGE_BYTES = 64 * 1024;
export const DIRECTORY_MAX_ROOT_BYTES = 4096;
export const DIRECTORY_LEAF_ENTRIES_BYTES = 60n * 1024n;
export const DIRECTORY_LEAF_MAX_ENTRIES = 1024n;
export const DIRECTORY_MAX_NAME_BYTES = 255;
export const DIRECTORY_MAX_SYMLINK_BYTES = 4096;
const U64_MAX = (1n << 64n) - 1n;

export interface DirectoryRoot {
  format_version: 2;
  mode: number;
  entry_count: bigint;
  subdirectory_count: bigint;
  directory_shard: ContentObjectRef;
}
export interface DirectorySummary {
  entry_count: bigint;
  subdirectory_count: bigint;
  /** Sum of canonical entry encodings, excluding array commas. */
  entries_bytes: bigint;
}
export type DirectoryEntryNode =
  | { type: "file"; object: ContentObjectRef; logical_size: bigint; mode: number }
  | { type: "directory"; object: ContentObjectRef }
  | { type: "symlink"; target_hex: string };
export interface DirectoryEntry { name_hex: string; node: DirectoryEntryNode }
export interface DirectoryShardLink {
  digest: string;
  size_bytes: bigint;
  summary: DirectorySummary;
}
export interface DirectoryShardChild { edge: number; shard: DirectoryShardLink }
export interface DirectoryShard {
  format_version: 1;
  prefix_hex: string;
  summary: DirectorySummary;
  node: { type: "leaf"; entries: DirectoryEntry[] } | { type: "branch"; children: DirectoryShardChild[] };
}
export class DirectoryFormatError extends ContentFormatError {
  constructor(message: string) { super(message); this.name = "DirectoryFormatError"; }
}
function fail(message: string): never { throw new DirectoryFormatError(message); }
function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) fail("Expected a directory metadata object");
  return value as Record<string, unknown>;
}
function exact(value: unknown, keys: readonly string[]): Record<string, unknown> {
  const object = record(value);
  if (Object.keys(object).length !== keys.length || keys.some((key) => !Object.hasOwn(object, key))) fail("Unsupported or missing directory metadata field");
  return object;
}
function array(value: unknown): unknown[] {
  if (!Array.isArray(value)) fail("Expected a directory metadata array");
  return value;
}
function bounded(value: unknown, codec: CanonicalMetadataCodec, limit: number): Buffer {
  const bytes = codec.encodeCanonical(value);
  if (bytes.byteLength > limit) fail("Directory metadata exceeds its format byte bound");
  return bytes;
}
function rawHex(value: unknown, max: number): Buffer {
  if (typeof value !== "string" || value.length > max * 2 || !/^(?:[0-9a-f]{2})*$/.test(value)) fail("Noncanonical or oversized directory byte field");
  return Buffer.from(value, "hex");
}
function nameBytes(value: unknown): Buffer {
  const bytes = rawHex(value, DIRECTORY_MAX_NAME_BYTES);
  if (!bytes.length || bytes.includes(0) || bytes.includes(47) || bytes.equals(Buffer.from(".")) || bytes.equals(Buffer.from(".."))) fail("Unsafe directory entry name");
  return bytes;
}
function mode(value: unknown): number { return Number(unsigned(value, 0o7777)); }
function validateSummary(value: unknown): DirectorySummary {
  const object = exact(value, ["entry_count", "subdirectory_count", "entries_bytes"]);
  const result = {
    entry_count: unsigned(object.entry_count),
    subdirectory_count: unsigned(object.subdirectory_count),
    entries_bytes: unsigned(object.entries_bytes),
  };
  if (result.subdirectory_count > result.entry_count || (result.entry_count === 0n) !== (result.entries_bytes === 0n)) fail("Invalid directory summary");
  return result;
}
function add(a: DirectorySummary, b: DirectorySummary): DirectorySummary {
  const sum = {
    entry_count: a.entry_count + b.entry_count,
    subdirectory_count: a.subdirectory_count + b.subdirectory_count,
    entries_bytes: a.entries_bytes + b.entries_bytes,
  };
  if (sum.entry_count > U64_MAX || sum.subdirectory_count > U64_MAX || sum.entries_bytes > U64_MAX) fail("Directory summary arithmetic overflow");
  return sum;
}
function sameSummary(a: DirectorySummary, b: DirectorySummary): boolean {
  return a.entry_count === b.entry_count && a.subdirectory_count === b.subdirectory_count && a.entries_bytes === b.entries_bytes;
}
function fitsLeaf(summary: DirectorySummary): boolean {
  return summary.entry_count <= DIRECTORY_LEAF_MAX_ENTRIES && summary.entries_bytes <= DIRECTORY_LEAF_ENTRIES_BYTES;
}
function shardRef(value: unknown): ContentObjectRef {
  const object = validateContentReference(value);
  if (object.kind !== "directory_shard" || object.size_bytes <= 0n || object.size_bytes > BigInt(DIRECTORY_MAX_PAGE_BYTES)) fail("Invalid or oversized directory shard reference");
  return object;
}
export function directoryShardReference(link: DirectoryShardLink): ContentObjectRef {
  return { kind: "directory_shard", algorithm: "sha256", digest: link.digest, size_bytes: link.size_bytes };
}
function sameRef(a: ContentObjectRef, b: ContentObjectRef): boolean {
  return a.kind === b.kind && a.algorithm === b.algorithm && a.digest === b.digest && a.size_bytes === b.size_bytes;
}
function refKey(object: ContentObjectRef): string { return `${object.kind}:${object.algorithm}:${object.digest}:${object.size_bytes}`; }
function validateLink(value: unknown): DirectoryShardLink {
  const object = exact(value, ["digest", "size_bytes", "summary"]);
  const reference = shardRef({ kind: "directory_shard", algorithm: "sha256", digest: object.digest, size_bytes: object.size_bytes });
  const summary = validateSummary(object.summary);
  if (summary.entry_count === 0n) fail("A radix branch cannot point to an empty child");
  return { digest: reference.digest, size_bytes: reference.size_bytes, summary };
}

export function validateDirectoryEntry(value: unknown): DirectoryEntry {
  const entry = exact(value, ["name_hex", "node"]);
  nameBytes(entry.name_hex);
  const node = record(entry.node);
  let parsed: DirectoryEntryNode;
  if (node.type === "file") {
    exact(node, ["type", "object", "logical_size", "mode"]);
    const object = validateContentReference(node.object);
    if (object.kind !== "blob" && object.kind !== "file") fail("Directory file entry requires Blob or File");
    const logical_size = unsigned(node.logical_size, MAX_FILE_LOGICAL_BYTES);
    if (object.kind === "blob" && logical_size !== object.size_bytes) fail("Blob logical size differs from referenced bytes");
    if (object.kind === "file" && (object.size_bytes <= 0n || object.size_bytes > BigInt(DIRECTORY_MAX_PAGE_BYTES))) fail("File descriptor exceeds its format bound");
    parsed = { type: "file", object, logical_size, mode: mode(node.mode) };
  } else if (node.type === "directory") {
    exact(node, ["type", "object"]);
    const object = validateContentReference(node.object);
    if (object.kind !== "directory") fail("Directory entry has the wrong reference kind");
    parsed = { type: "directory", object };
  } else if (node.type === "symlink") {
    exact(node, ["type", "target_hex"]);
    const target = rawHex(node.target_hex, DIRECTORY_MAX_SYMLINK_BYTES);
    if (!target.length || target.includes(0)) fail("Invalid symlink text");
    parsed = { type: "symlink", target_hex: node.target_hex as string };
  } else return fail("Unsupported directory entry type");
  return { name_hex: entry.name_hex as string, node: parsed };
}

/** Parsed values must come from a lossless canonical codec. Safe JS integers
 * are accepted for callers constructing fixtures; unsafe numbers fail closed. */
export function validateDirectoryRoot(value: unknown, codec: CanonicalMetadataCodec): DirectoryRoot {
  const object = exact(value, ["format_version", "mode", "entry_count", "subdirectory_count", "directory_shard"]);
  if (unsigned(object.format_version) !== 2n) fail("Directory root requires schema 2");
  const entry_count = unsigned(object.entry_count), subdirectory_count = unsigned(object.subdirectory_count);
  if (subdirectory_count > entry_count) fail("Root subdirectory count exceeds entries");
  const root: DirectoryRoot = { format_version: 2, mode: mode(object.mode), entry_count, subdirectory_count, directory_shard: shardRef(object.directory_shard) };
  bounded(root, codec, DIRECTORY_MAX_ROOT_BYTES);
  return root;
}

export function validateDirectoryShard(value: unknown, codec: CanonicalMetadataCodec): DirectoryShard {
  const object = exact(value, ["format_version", "prefix_hex", "summary", "node"]);
  if (unsigned(object.format_version) !== 1n) fail("Unsupported directory shard layout");
  const prefix = rawHex(object.prefix_hex, DIRECTORY_MAX_NAME_BYTES);
  if (prefix.includes(0) || prefix.includes(47)) fail("Unsafe shard prefix");
  const summary = validateSummary(object.summary), node = record(object.node);
  let parsed: DirectoryShard["node"];
  if (node.type === "leaf") {
    exact(node, ["type", "entries"]);
    const values = array(node.entries);
    if (BigInt(values.length) > DIRECTORY_LEAF_MAX_ENTRIES) fail("Directory leaf entry count exceeds layout bound");
    const entries = values.map(validateDirectoryEntry);
    let actual: DirectorySummary = { entry_count: 0n, subdirectory_count: 0n, entries_bytes: 0n };
    let previous: string | null = null;
    for (const entry of entries) {
      if (previous !== null && previous >= entry.name_hex) fail("Directory names must be unique and byte-sorted");
      previous = entry.name_hex;
      actual = add(actual, { entry_count: 1n, subdirectory_count: entry.node.type === "directory" ? 1n : 0n, entries_bytes: BigInt(codec.encodeCanonical(entry).byteLength) });
    }
    if (!sameSummary(summary, actual) || !fitsLeaf(actual)) fail("Directory leaf summary or packing mismatch");
    let common: Buffer = Buffer.alloc(0);
    if (entries.length) {
      const first = nameBytes(entries[0].name_hex), last = nameBytes(entries[entries.length - 1].name_hex);
      let length = 0; while (length < first.length && length < last.length && first[length] === last[length]) length++;
      common = first.subarray(0, length);
    }
    if (!prefix.equals(common)) fail("Leaf prefix is not its maximal common prefix");
    parsed = { type: "leaf", entries };
  } else if (node.type === "branch") {
    exact(node, ["type", "children"]);
    const values = array(node.children);
    if (values.length < 2 || values.length > 257 || prefix.length >= DIRECTORY_MAX_NAME_BYTES) fail("Invalid radix branch fanout or prefix");
    if (fitsLeaf(summary)) fail("Noncanonical radix branch fits a leaf");
    const children: DirectoryShardChild[] = [];
    let total: DirectorySummary = { entry_count: 0n, subdirectory_count: 0n, entries_bytes: 0n }, previous = -1;
    const seen = new Set<string>();
    for (const value of values) {
      const child = exact(value, ["edge", "shard"]);
      const edge = Number(unsigned(child.edge, 256));
      if (edge <= previous || edge === 1 || edge === 48) fail("Radix edges must be sorted, unique and safe");
      previous = edge;
      const shard = validateLink(child.shard), key = refKey(directoryShardReference(shard));
      if (seen.has(key)) fail("Branch repeats a directory shard");
      seen.add(key);
      if (edge === 0 && shard.summary.entry_count !== 1n) fail("Terminator child must contain exactly one name");
      total = add(total, shard.summary);
      children.push({ edge, shard });
    }
    if (!sameSummary(total, summary)) fail("Radix branch summaries do not add up");
    parsed = { type: "branch", children };
  } else return fail("Unsupported directory shard node type");
  const shard: DirectoryShard = { format_version: 1, prefix_hex: object.prefix_hex as string, summary, node: parsed };
  bounded(shard, codec, DIRECTORY_MAX_PAGE_BYTES);
  return shard;
}

export function decodeDirectoryRoot(bytes: Buffer, codec: CanonicalMetadataCodec): DirectoryRoot {
  if (bytes.byteLength > DIRECTORY_MAX_ROOT_BYTES) fail("Directory root exceeds its format byte bound");
  const root = validateDirectoryRoot(codec.parseCanonical(bytes), codec);
  if (!codec.encodeCanonical(root).equals(bytes)) fail("Directory root must use canonical compact JSON");
  return root;
}
export function decodeDirectoryShard(bytes: Buffer, codec: CanonicalMetadataCodec): DirectoryShard {
  if (bytes.byteLength > DIRECTORY_MAX_PAGE_BYTES) fail("Directory shard exceeds its format byte bound");
  const shard = validateDirectoryShard(codec.parseCanonical(bytes), codec);
  if (!codec.encodeCanonical(shard).equals(bytes)) fail("Directory shard must use canonical compact JSON");
  return shard;
}

/** Complete typed direct edges. The shard page itself is never an opaque Blob.
 * Receive can precede children; publication additionally checks actual edges. */
export function directoryReferences(kind: "directory" | "directory_shard", value: unknown, codec: CanonicalMetadataCodec): ContentObjectRef[] {
  if (kind === "directory") return [validateDirectoryRoot(value, codec).directory_shard];
  if (kind !== "directory_shard") return fail("Object is not a directory root or shard");
  const shard = validateDirectoryShard(value, codec);
  if (shard.node.type === "branch") return shard.node.children.map((child) => directoryShardReference(child.shard));
  return shard.node.entries.flatMap((entry) => entry.node.type === "file" || entry.node.type === "directory" ? [entry.node.object] : []);
}

function bindMetadata(child: ContentObjectRef, value: unknown, codec: CanonicalMetadataCodec): Buffer {
  const bytes = bounded(value, codec, DIRECTORY_MAX_PAGE_BYTES);
  if (BigInt(bytes.byteLength) !== child.size_bytes || createHash("sha256").update(bytes).digest("hex") !== child.digest) fail("Child metadata differs from its typed reference");
  return bytes;
}
function validateRoute(parent: DirectoryShard, child: DirectoryShardChild, actual: DirectoryShard): void {
  if (!sameSummary(child.shard.summary, actual.summary)) fail("Actual shard summary differs from parent");
  const prefix = rawHex(parent.prefix_hex, DIRECTORY_MAX_NAME_BYTES), actualPrefix = rawHex(actual.prefix_hex, DIRECTORY_MAX_NAME_BYTES);
  if (child.edge === 0) {
    if (!actualPrefix.equals(prefix) || actual.node.type !== "leaf" || actual.node.entries.length !== 1 || !nameBytes(actual.node.entries[0].name_hex).equals(prefix)) fail("Terminator child is not the exact prefix name");
  } else if (actualPrefix.length <= prefix.length || !actualPrefix.subarray(0, prefix.length).equals(prefix) || actualPrefix[prefix.length] + 1 !== child.edge) fail("Child prefix lies outside parent radix range");
}

/** Pure graph-edge validation, including SHA/length binding for bounded child
 * metadata. For Blob and nested Directory refs no body is needed: entry shape
 * already validates Blob length and the outer closure owns data availability.
 * A File child is a descriptor; every same-ref entry's logical length is checked.
 */
export function validateDirectoryChild(
  parentKind: "directory" | "directory_shard", parentValue: unknown,
  childReference: unknown, childValue: unknown, codec: CanonicalMetadataCodec,
): void {
  const child = validateContentReference(childReference);
  if (parentKind === "directory") {
    const parent = validateDirectoryRoot(parentValue, codec);
    if (!sameRef(parent.directory_shard, child)) fail("Root does not reference this shard");
    const bytes = bindMetadata(child, childValue, codec), actual = decodeDirectoryShard(bytes, codec);
    if (parent.entry_count !== actual.summary.entry_count || parent.subdirectory_count !== actual.summary.subdirectory_count) fail("Actual root shard counts differ from parent");
    return;
  }
  if (parentKind !== "directory_shard") fail("Parent is not a directory root or shard");
  const parent = validateDirectoryShard(parentValue, codec);
  if (parent.node.type === "branch") {
    const link = parent.node.children.find((link) => sameRef(directoryShardReference(link.shard), child));
    if (!link) fail("Branch does not reference this shard");
    const actual = decodeDirectoryShard(bindMetadata(child, childValue, codec), codec);
    validateRoute(parent, link, actual);
    return;
  }
  const entries = parent.node.entries.filter((entry) => (entry.node.type === "file" || entry.node.type === "directory") && sameRef(entry.node.object, child));
  if (!entries.length) fail("Leaf does not reference this child");
  if (child.kind === "file") {
    const bytes = bindMetadata(child, childValue, codec);
    const file = validateFileNode(codec.parseCanonical(bytes));
    for (const entry of entries) if (entry.node.type === "file" && entry.node.logical_size !== file.logical_size) fail("File logical size differs from directory entry");
  }
}
