#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import { randomUUID } from "node:crypto";
import { checkedSourceRevision, MANIFEST_NAME, readNativeBinary, verifyNativeArtifacts } from "./native-artifacts.mjs";

const nativeBinaryBase = "rudder-native";
const nativeBinaryName = process.platform === "win32" ? `${nativeBinaryBase}.exe` : nativeBinaryBase;
const companionBinaryName = process.platform === "win32" ? "rudder-jj.exe" : "rudder-jj";
const platformKey = `${process.platform}-${process.arch}`;
const distNativeDir = path.join("dist", "native");

function copyExecutable(src, dest) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(src, dest);
  if (process.platform !== "win32") {
    fs.chmodSync(dest, 0o755);
  }
}

function copyCurrentBuild() {
  const sources = process.env.CARGO_TARGET_DIR ? [path.join(process.env.CARGO_TARGET_DIR, "release", nativeBinaryName)] : [
    path.join("target", "release", nativeBinaryName),
    path.join("native", "target", "release", nativeBinaryName),
  ];
  const src = sources.find((candidate) => fs.existsSync(candidate));
  if (!src) {
    return false;
  }
  const companion = path.join(path.dirname(src), companionBinaryName);
  if (!fs.existsSync(companion)) {
    throw new Error("Native build is missing rudder-jj; run npm run build-native.");
  }
  copyExecutable(src, path.join(distNativeDir, platformKey, nativeBinaryName));
  copyExecutable(companion, path.join(distNativeDir, platformKey, companionBinaryName));
  return true;
}

async function copyPrebuiltArtifacts() {
  const root = process.env.RUDDER_NATIVE_PREBUILTS;
  if (!root) throw new Error("RUDDER_NATIVE_PREBUILTS must name a complete verified artifact set");
  const verified = await verifyNativeArtifacts({ root, expectedRevision: process.env.RUDDER_NATIVE_SOURCE_REVISION });
  // Check every platform before any output changes; rehash while copying into
  // private staging so a changed source cannot become a packaged executable.
  fs.mkdirSync("dist", { recursive: true });
  const staging = path.join("dist", `.native-stage-${randomUUID()}`);
  fs.mkdirSync(staging, { mode: 0o700 });
  try {
    for (const artifact of verified.artifacts) {
      const directory = path.join(staging, artifact.platform);
      fs.mkdirSync(directory);
      for (const file of artifact.files) {
        await readNativeBinary(file.path, file, path.join(directory, file.name));
      }
      fs.writeFileSync(path.join(directory, MANIFEST_NAME), JSON.stringify(artifact.manifest, null, 2) + "\n");
    }
    if (checkedSourceRevision({ expectedRevision: process.env.RUDDER_NATIVE_SOURCE_REVISION }) !== verified.sourceRevision) throw new Error("Native source changed while staging artifacts");
    fs.mkdirSync(distNativeDir, { recursive: true });
    for (const artifact of verified.artifacts) {
      const destination = path.join(distNativeDir, artifact.platform);
      fs.rmSync(destination, { recursive: true, force: true });
      fs.renameSync(path.join(staging, artifact.platform), destination);
    }
  } finally {
    fs.rmSync(staging, { recursive: true, force: true });
  }
}

function removeLegacyFlatBinary() {
  fs.rmSync(path.join(distNativeDir, "rudder-native"), { force: true });
  fs.rmSync(path.join(distNativeDir, "rudder-native.exe"), { force: true });
}

if (Object.hasOwn(process.env, "RUDDER_NATIVE_PREBUILTS")) await copyPrebuiltArtifacts();
else copyCurrentBuild();
removeLegacyFlatBinary();
