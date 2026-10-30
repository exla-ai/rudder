#!/usr/bin/env node
import { createHash, randomUUID } from "node:crypto";
import { execFileSync } from "node:child_process";
import fs from "node:fs/promises";
import { constants, realpathSync, openSync, closeSync, fstatSync, readSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { isDeepStrictEqual, parseArgs } from "node:util";

export const NATIVE_PLATFORMS = Object.freeze(["darwin-arm64", "darwin-x64", "linux-x64"]);
export const NATIVE_BINARIES = Object.freeze(["rudder-native", "rudder-jj"]);
export const MANIFEST_NAME = "native-manifest.json";
export const NATIVE_FEATURES = Object.freeze(["rudder-vcs-bridge/cli"]);
const NATIVE_INPUTS = ["Cargo.toml", "Cargo.lock", ".cargo/config", ".cargo/config.toml", "rust-toolchain", "rust-toolchain.toml", "native", "engine", "vcs-bridge", "assets"];
const MAX_BINARY_BYTES = 512 * 1024 * 1024;
const MAX_MANIFEST_BYTES = 16 * 1024;
const MAX_PACKAGE_BYTES = 256 * 1024;

function requireValue(condition, message) {
  if (!condition) throw new Error(message);
}
function exact(value, keys, label) {
  requireValue(value && typeof value === "object" && !Array.isArray(value)
    && Object.keys(value).sort().join("\0") === [...keys].sort().join("\0"), `Invalid ${label} fields`);
}
function git(cwd, args) {
  return execFileSync("git", ["--no-optional-locks", "-C", cwd, ...args], { encoding: "utf8", maxBuffer: 2 * 1024 * 1024, stdio: ["ignore", "pipe", "pipe"] }).trim();
}
function checkTrackedSourceBytes(cwd, revision) {
  const entries = git(cwd, ["ls-tree", "-rz", revision, "--", ...NATIVE_INPUTS]).split("\0").filter(Boolean);
  requireValue(entries.length > 0 && entries.length <= 10_000, "Native source file inventory exceeds its bound");
  const buffer = Buffer.alloc(64 * 1024);
  let total = 0;
  for (const entry of entries) {
    const separator = entry.indexOf("\t");
    const [mode, kind, expected] = entry.slice(0, separator).split(" ");
    const relative = entry.slice(separator + 1);
    requireValue(separator > 0 && ["100644", "100755"].includes(mode) && kind === "blob" && /^[0-9a-f]{40}$/.test(expected), "Native inputs require regular tracked source files");
    const fd = openSync(path.join(cwd, relative), constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
    try {
      const before = fstatSync(fd, { bigint: true });
      total += Number(before.size);
      requireValue(before.isFile() && before.size <= BigInt(MAX_BINARY_BYTES) && total <= 2 * 1024 ** 3, "Native source bytes exceed their bound");
      // Match Git's blob identity directly, independent of index stat caches,
      // assume-unchanged/skip-worktree flags or line-ending conversion settings.
      const digest = createHash("sha1").update(`blob ${before.size}\0`);
      let size = 0;
      for (;;) {
        const count = readSync(fd, buffer, 0, buffer.length, size);
        if (!count) break;
        size += count;
        requireValue(size <= Number(before.size), "Native source changed during verification");
        digest.update(buffer.subarray(0, count));
      }
      const after = fstatSync(fd, { bigint: true });
      requireValue(size === Number(before.size) && before.size === after.size
        && before.mtimeNs === after.mtimeNs && before.ctimeNs === after.ctimeNs
        && digest.digest("hex") === expected, "Native source bytes do not match the source revision");
    } finally { closeSync(fd); }
  }
}
function checkPackageMetadata(cwd, revision) {
  const fromGit = git(cwd, ["show", `${revision}:package.json`]);
  requireValue(Buffer.byteLength(fromGit) <= MAX_PACKAGE_BYTES, "Native package metadata exceeds its bound");
  const fd = openSync(path.join(cwd, "package.json"), constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  let current, expected;
  try {
    const stat = fstatSync(fd);
    requireValue(stat.isFile() && stat.size <= MAX_PACKAGE_BYTES, "Native package metadata exceeds its bound");
    const buffer = Buffer.alloc(MAX_PACKAGE_BYTES + 1);
    let size = 0;
    while (size < buffer.length) {
      const count = readSync(fd, buffer, size, buffer.length - size, size);
      if (!count) break;
      size += count;
    }
    requireValue(size === stat.size && size <= MAX_PACKAGE_BYTES, "Native package metadata changed during read");
    try { current = JSON.parse(buffer.subarray(0, size).toString("utf8")); expected = JSON.parse(fromGit); }
    catch { throw new Error("Invalid native package metadata JSON"); }
  } finally { closeSync(fd); }
  requireValue(current && expected && !Array.isArray(current) && !Array.isArray(expected), "Invalid native package metadata");
  const permittedName = current.name === expected.name || (expected.name === "@exla/rudder" && current.name === "@viraatdas/rudder");
  requireValue(permittedName && isDeepStrictEqual({ ...current, name: expected.name }, expected), "Package metadata must match the source revision except for the approved npm name alias");
}

/** Release prebuilt provenance requires a real clean Git checkout. Ordinary
 * local Cargo builds do not call this, including builds from source archives. */
export function checkedSourceRevision({ cwd = process.cwd(), expectedRevision } = {}) {
  requireValue(realpathSync(git(cwd, ["rev-parse", "--show-toplevel"])) === realpathSync(cwd), "Release prebuilts require the Git checkout root");
  const revision = git(cwd, ["rev-parse", "--verify", "HEAD^{commit}"]);
  requireValue(/^[0-9a-f]{40}$/.test(revision), "Invalid native source revision");
  if (expectedRevision !== undefined) {
    requireValue(/^[0-9a-f]{40}$/.test(expectedRevision) && expectedRevision === revision, "Native source revision does not match the expected revision");
  }
  const changed = git(cwd, ["status", "--porcelain=v1", "--untracked-files=all", "--", ...NATIVE_INPUTS]);
  requireValue(!changed, "Native build inputs must match the source revision before using release prebuilts");
  checkTrackedSourceBytes(cwd, revision);
  checkPackageMetadata(cwd, revision);
  return revision;
}

async function directory(file) {
  const stat = await fs.lstat(file);
  requireValue(stat.isDirectory() && !stat.isSymbolicLink(), "Native artifact directory must be a real directory");
}

/** Hash/copy at most 64 KiB at a time, check bounded size and reject changes
 * during the read. Rechecking while staging prevents verify/copy substitution. */
export async function readNativeBinary(file, expected, destination) {
  const source = await fs.open(file, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  let output;
  try {
    const before = await source.stat({ bigint: true });
    requireValue(before.isFile() && before.size > 0n && before.size <= BigInt(MAX_BINARY_BYTES), "Invalid native executable size or type");
    if (expected) requireValue(Number(before.size) === expected.size, "Native executable size mismatch");
    if (destination) output = await fs.open(destination, "wx", 0o755);
    const buffer = Buffer.alloc(64 * 1024);
    const hash = createHash("sha256");
    let size = 0;
    for (;;) {
      const { bytesRead } = await source.read(buffer, 0, buffer.length, size);
      if (!bytesRead) break;
      size += bytesRead;
      requireValue(size <= MAX_BINARY_BYTES && size <= Number(before.size), "Native executable changed during read");
      hash.update(buffer.subarray(0, bytesRead));
      if (output) {
        let written = 0;
        while (written < bytesRead) {
          const result = await output.write(buffer, written, bytesRead - written);
          requireValue(result.bytesWritten > 0, "Native executable copy made no progress");
          written += result.bytesWritten;
        }
      }
    }
    const after = await source.stat({ bigint: true });
    requireValue(size === Number(before.size) && before.size === after.size
      && before.mtimeNs === after.mtimeNs && before.ctimeNs === after.ctimeNs, "Native executable changed during read");
    const result = { sha256: hash.digest("hex"), size };
    if (expected) requireValue(result.sha256 === expected.sha256, "Native executable SHA256 mismatch");
    if (output) await output.chmod(0o755);
    return result;
  } finally {
    try { await output?.close(); }
    finally { await source.close(); }
  }
}

async function readManifest(file) {
  const handle = await fs.open(file, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  try {
    const stat = await handle.stat();
    requireValue(stat.isFile() && stat.size > 0 && stat.size <= MAX_MANIFEST_BYTES, "Invalid native manifest size or type");
    const buffer = Buffer.alloc(MAX_MANIFEST_BYTES + 1);
    let size = 0;
    while (size < buffer.length) {
      const { bytesRead } = await handle.read(buffer, size, buffer.length - size, size);
      if (!bytesRead) break;
      size += bytesRead;
    }
    requireValue(size === stat.size && size <= MAX_MANIFEST_BYTES, "Native manifest changed during read");
    try { return JSON.parse(buffer.subarray(0, size).toString("utf8")); }
    catch { throw new Error("Invalid native manifest JSON"); }
  } finally { await handle.close(); }
}

function validateManifest(manifest, platform, revision) {
  exact(manifest, ["formatVersion", "sourceRevision", "platform", "buildProfile", "features", "files"], "native manifest");
  requireValue(manifest.formatVersion === 1 && manifest.sourceRevision === revision && manifest.platform === platform
    && manifest.buildProfile === "release" && JSON.stringify(manifest.features) === JSON.stringify(NATIVE_FEATURES), "Native manifest source, platform or build graph mismatch");
  exact(manifest.files, NATIVE_BINARIES, "native manifest executable");
  for (const name of NATIVE_BINARIES) {
    const file = manifest.files[name];
    exact(file, ["sha256", "size"], "native executable manifest");
    requireValue(typeof file.sha256 === "string" && /^[0-9a-f]{64}$/.test(file.sha256)
      && Number.isSafeInteger(file.size) && file.size > 0 && file.size <= MAX_BINARY_BYTES, "Invalid native executable manifest");
  }
}

async function artifactDirectory(dir, allowMissingManifest = false) {
  await directory(dir);
  const entries = (await fs.readdir(dir)).sort();
  const allowed = [...NATIVE_BINARIES, MANIFEST_NAME].sort();
  requireValue(entries.includes("rudder-native"), "Native artifact is missing rudder-native");
  requireValue(entries.includes("rudder-jj"), "Native artifact is missing its rudder-jj companion");
  requireValue(JSON.stringify(entries) === JSON.stringify(allowed)
    || (allowMissingManifest && JSON.stringify(entries) === JSON.stringify([...NATIVE_BINARIES].sort())), "Native artifact has missing or unexpected files");
  for (const name of entries) requireValue((await fs.lstat(path.join(dir, name))).isFile(), "Native artifacts must be regular files, not symlinks");
}

export async function createNativeManifest({ directory: dir, platform, cwd = process.cwd(), expectedRevision } = {}) {
  requireValue(NATIVE_PLATFORMS.includes(platform), "Unsupported native artifact platform");
  const revision = checkedSourceRevision({ cwd, expectedRevision });
  await artifactDirectory(dir, true);
  const files = {};
  for (const name of NATIVE_BINARIES) files[name] = await readNativeBinary(path.join(dir, name));
  requireValue(checkedSourceRevision({ cwd, expectedRevision }) === revision, "Native source changed during manifest creation");
  const manifest = { formatVersion: 1, sourceRevision: revision, platform, buildProfile: "release", features: [...NATIVE_FEATURES], files };
  const temporary = path.join(dir, `.native-manifest-${randomUUID()}.tmp`);
  try {
    await fs.writeFile(temporary, JSON.stringify(manifest, null, 2) + "\n", { flag: "wx", mode: 0o644 });
    await fs.rename(temporary, path.join(dir, MANIFEST_NAME));
  } finally { await fs.rm(temporary, { force: true }); }
  return manifest;
}

export async function verifyNativeArtifacts({ root, cwd = process.cwd(), expectedRevision } = {}) {
  const revision = checkedSourceRevision({ cwd, expectedRevision });
  await directory(root);
  const names = NATIVE_PLATFORMS.map(key => `rudder-native-${key}`).sort();
  requireValue(JSON.stringify((await fs.readdir(root)).sort()) === JSON.stringify(names), "Release prebuilts require exactly all three supported platform artifacts");
  const artifacts = [];
  for (const platform of NATIVE_PLATFORMS) {
    const dir = path.join(root, `rudder-native-${platform}`);
    await artifactDirectory(dir);
    const manifest = await readManifest(path.join(dir, MANIFEST_NAME));
    validateManifest(manifest, platform, revision);
    const files = [];
    for (const name of NATIVE_BINARIES) {
      const file = path.join(dir, name);
      await readNativeBinary(file, manifest.files[name]);
      files.push({ name, path: file, ...manifest.files[name] });
    }
    artifacts.push({ platform, manifest, files });
  }
  requireValue(checkedSourceRevision({ cwd, expectedRevision }) === revision, "Native source changed during artifact verification");
  return { sourceRevision: revision, artifacts };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const { values, positionals } = parseArgs({ allowPositionals: true, options: {
      root: { type: "string" }, directory: { type: "string" }, platform: { type: "string" }, revision: { type: "string" },
    } });
    requireValue(positionals.length === 1 && values.revision, "Native artifact command requires an explicit revision");
    if (positionals[0] === "create") {
      requireValue(values.directory && values.platform === `${process.platform}-${process.arch}`, "Native artifact producer must match its build host platform");
      const manifest = await createNativeManifest({ directory: values.directory, platform: values.platform, expectedRevision: values.revision });
      console.log(`Created ${manifest.platform} native manifest for ${manifest.sourceRevision}`);
    } else if (positionals[0] === "verify") {
      requireValue(values.root, "Native artifact verification requires a root");
      const verified = await verifyNativeArtifacts({ root: values.root, expectedRevision: values.revision });
      console.log(`Verified ${verified.artifacts.length} native pairs for ${verified.sourceRevision}`);
    } else throw new Error("Unknown native artifact command");
  } catch (error) {
    console.error(`Native artifacts: ${error.message}`);
    process.exitCode = 1;
  }
}
