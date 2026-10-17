import { spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";
import { commandExists, ensureDir, rudderHome } from "./util.js";

// jj (Jujutsu) is Rudder's workspace engine, and nobody should have to install
// it as a separate step. When `jj` is not on PATH, Rudder fetches jj's own
// prebuilt release for this platform (checksum-pinned below) into
// ~/.rudder/bin/jj/<version>/ and puts that directory on PATH for itself and
// everything it starts: the native dashboard, workers and agents all inherit
// it. A jj the user installed (brew, cargo, a distro package) always wins.
//
// Not newer on purpose: from 0.46 `jj workspace add` needs git >= 2.42
// (`git worktree add --orphan`), and stock macOS and Debian 12 ship git 2.39,
// so every isolated agent would fail to start on exactly the machines that have
// nothing installed. 0.40.0 is also what the cloud worker image runs.
export const MANAGED_JJ_VERSION = "0.40.0";

// sha256 of each release archive, from github.com/jj-vcs/jj/releases/tag/v0.40.0.
export const MANAGED_JJ_ASSETS: Record<string, { target: string; sha256: string }> = {
  "darwin-arm64": { target: "aarch64-apple-darwin", sha256: "8a1d713103bb968c771617c9b2c48b0b5982193090ee74dec935bff710af2082" },
  "darwin-x64": { target: "x86_64-apple-darwin", sha256: "ce62cf26e3c6c72a295f5917056e33cfa972874f882a2d15b5a3687b3ddce1e5" },
  "linux-x64": { target: "x86_64-unknown-linux-musl", sha256: "5c8979f46873e052f59bdd9535636dca6e6f9f70571b73f6d63c3b92acfaa037" },
  "linux-arm64": { target: "aarch64-unknown-linux-musl", sha256: "b26f24ff7a34838fbafe8788e6a94a9cdcf51601ef8c9af8fab4fa22c06ddbee" },
};

const DOWNLOAD_TIMEOUT_MS = 120_000;

export type JjSource = "system" | "managed";

let lastInstallError: string | null = null;

/** Why the last automatic install failed, for the "jj is missing" error. */
export function managedJjInstallError(): string | null {
  return lastInstallError;
}

export function managedJjDir(version = MANAGED_JJ_VERSION): string {
  return path.join(rudderHome(), "bin", "jj", version);
}

export function managedJjPath(version = MANAGED_JJ_VERSION): string {
  return path.join(managedJjDir(version), "jj");
}

export function managedJjAsset(platform = process.platform, arch = process.arch): { url: string; sha256: string } | null {
  const asset = MANAGED_JJ_ASSETS[`${platform}-${arch}`];
  if (!asset) return null;
  const tag = `v${MANAGED_JJ_VERSION}`;
  return {
    url: `https://github.com/jj-vcs/jj/releases/download/${tag}/jj-${tag}-${asset.target}.tar.gz`,
    sha256: asset.sha256,
  };
}

/** True when the managed jj's directory is already on this process's PATH. */
export function managedJjOnPath(): boolean {
  const dir = managedJjDir();
  return (process.env.PATH ?? "").split(path.delimiter).includes(dir);
}

/**
 * Make `jj` resolve for this process and its children. Returns where it comes
 * from, or null when there is none and it could not be installed. Never throws:
 * commands that need jj fail later with MissingToolError, which carries the
 * reason recorded here.
 */
export async function ensureJjAvailable(options: { onInstall?: (message: string) => void } = {}): Promise<JjSource | null> {
  if (managedJjOnPath() && isExecutable(managedJjPath())) return "managed";
  if (commandExists("jj")) return "system";
  if (isExecutable(managedJjPath())) {
    prependPath(managedJjDir());
    return "managed";
  }
  const asset = managedJjAsset();
  if (!asset) {
    lastInstallError = `no prebuilt jj for ${process.platform}/${process.arch}`;
    return null;
  }
  options.onInstall?.(`Setting up jj ${MANAGED_JJ_VERSION}, Rudder's workspace engine (one time, about 10 MB)...`);
  try {
    await installManagedJj({ url: asset.url, sha256: asset.sha256, dest: managedJjPath() });
  } catch (error) {
    lastInstallError = error instanceof Error ? error.message : String(error);
    return null;
  }
  lastInstallError = null;
  prependPath(managedJjDir());
  return "managed";
}

/**
 * Download a jj release archive, check it against the pinned sha256, unpack the
 * `jj` binary, make sure it runs, then move it into place. Nothing is left at
 * `dest` unless every step passed.
 */
export async function installManagedJj(params: { url: string; sha256: string; dest: string; timeoutMs?: number }): Promise<void> {
  const response = await fetch(params.url, {
    redirect: "follow",
    signal: AbortSignal.timeout(params.timeoutMs ?? DOWNLOAD_TIMEOUT_MS),
  });
  if (!response.ok) {
    throw new Error(`could not download jj from ${params.url}: HTTP ${response.status}`);
  }
  const archive = Buffer.from(await response.arrayBuffer());
  const actual = createHash("sha256").update(archive).digest("hex");
  if (actual !== params.sha256) {
    throw new Error(`jj download failed its checksum (expected ${params.sha256}, got ${actual})`);
  }

  const destDir = path.dirname(params.dest);
  await ensureDir(path.dirname(destDir));
  const staging = path.join(path.dirname(destDir), `.install-${process.pid}-${randomUUID().slice(0, 8)}`);
  await ensureDir(staging);
  try {
    const archivePath = path.join(staging, "jj.tar.gz");
    await fsp.writeFile(archivePath, archive);
    const untar = spawnSync("tar", ["-xzf", archivePath, "-C", staging], { encoding: "utf8" });
    if (untar.status !== 0) {
      throw new Error(`could not unpack jj: ${(untar.stderr || untar.error?.message || "tar failed").trim()}`);
    }
    const binary = path.join(staging, "jj");
    if (!fs.existsSync(binary)) {
      throw new Error("the jj archive had no jj binary in it");
    }
    await fsp.chmod(binary, 0o755);
    const probe = spawnSync(binary, ["--version"], { encoding: "utf8" });
    if (probe.status !== 0) {
      throw new Error(`the downloaded jj does not run here: ${(probe.stderr || probe.error?.message || "").trim()}`);
    }
    await ensureDir(destDir);
    await fsp.rename(binary, params.dest);
  } finally {
    await fsp.rm(staging, { recursive: true, force: true }).catch(() => undefined);
  }
}

function prependPath(dir: string): void {
  const parts = (process.env.PATH ?? "").split(path.delimiter).filter((part) => part && part !== dir);
  process.env.PATH = [dir, ...parts].join(path.delimiter);
}

function isExecutable(file: string): boolean {
  try {
    fs.accessSync(file, fs.constants.X_OK);
    return fs.statSync(file).isFile();
  } catch {
    return false;
  }
}
