#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { verifyNativeArtifacts } from "./native-artifacts.mjs";

export const NATIVE_CARGO_ARGS = Object.freeze([
  "build", "--release", "--locked", "--manifest-path", "Cargo.toml",
  "-p", "rudder-native", "-p", "rudder-vcs-bridge",
  "--features", "rudder-vcs-bridge/cli", "--bin", "rudder-native", "--bin", "rudder-jj",
]);

export async function buildNative({ cwd = process.cwd(), env = process.env } = {}) {
  // Presence is intentional: an empty or invalid value must fail, never silently
  // fall back to Cargo or package a stale local build after a bad CI download.
  if (Object.hasOwn(env, "RUDDER_NATIVE_PREBUILTS")) {
    if (!env.RUDDER_NATIVE_PREBUILTS) throw new Error("RUDDER_NATIVE_PREBUILTS must name a complete verified artifact set");
    const verified = await verifyNativeArtifacts({ root: path.resolve(cwd, env.RUDDER_NATIVE_PREBUILTS), cwd, expectedRevision: env.RUDDER_NATIVE_SOURCE_REVISION });
    console.log(`Using ${verified.artifacts.length} verified native pairs from ${verified.sourceRevision}; Cargo build skipped`);
    return 0;
  }
  const result = spawnSync("cargo", NATIVE_CARGO_ARGS, { cwd, env, stdio: "inherit" });
  if (result.error) throw new Error(`Unable to run Cargo: ${result.error.code ?? "spawn failed"}`);
  if (result.status !== 0) return result.status ?? 1;
  const target = path.resolve(cwd, env.CARGO_TARGET_DIR || "target", "release");
  const extension = process.platform === "win32" ? ".exe" : "";
  for (const name of ["rudder-native", "rudder-jj"]) {
    const file = path.join(target, name + extension);
    if (!fs.existsSync(file) || !fs.statSync(file).isFile() || !fs.statSync(file).size) throw new Error(`Native build is missing ${name}; both executables are required`);
  }
  return 0;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { process.exitCode = await buildNative(); }
  catch (error) { console.error(`Native build: ${error.message}`); process.exitCode = 1; }
}
