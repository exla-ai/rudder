#!/usr/bin/env node
// One Rudder version everywhere: `npm version` runs this (package.json
// "version" script), so the version commit also carries
//   - the Mac and iPhone apps' version in mac/project.yml and ios/project.yml,
//     with the build number derived from it (5.0.8 -> 5000008), which Sparkle
//     and App Store Connect compare, and
//   - the Cloud worker image pin in cloud/fly.toml. The Mac release waits for
//     Cloud to run that image, so the pin must be in the tagged commit.
// scripts/release.mjs checks the same lines before it stages anything.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const WORKER_IMAGE_REPOSITORY = "registry.fly.io/rudder-workers-exla";

export function appBuild(version) {
  const parts = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  if (!parts) throw new Error(`version ${version} is not MAJOR.MINOR.PATCH; the Mac and iPhone apps need a plain release version`);
  const [major, minor, patch] = parts.slice(1).map(Number);
  if (minor > 999 || patch > 999) throw new Error(`version ${version} does not fit the app build number`);
  return String(major * 1_000_000 + minor * 1_000 + patch);
}

// file -> [the text before the quoted value, the value] for each versioned line.
const LINES = {
  "mac/project.yml": version => [["MARKETING_VERSION: ", version], ["CURRENT_PROJECT_VERSION: ", appBuild(version)]],
  "ios/project.yml": version => [["MARKETING_VERSION: ", version], ["CURRENT_PROJECT_VERSION: ", appBuild(version)]],
  "cloud/fly.toml": version => [["RUDDER_WORKER_IMAGE = ", `${WORKER_IMAGE_REPOSITORY}:${version}`]],
};

function find(text, file, key) {
  const match = new RegExp(`^(\\s*${key})"([^"]*)"$`, "m").exec(text);
  if (!match) throw new Error(`${file} has no ${key.trim()} line`);
  return match;
}

/** Throws unless every versioned line in the checkout at root names version. */
export function checkVersionFiles(root, version) {
  for (const [file, values] of Object.entries(LINES)) {
    const text = fs.readFileSync(path.join(root, file), "utf8");
    for (const [key, value] of values(version)) {
      const found = find(text, file, key)[2];
      if (found !== value) throw new Error(`${file} sets ${key.trim()} "${found}", not "${value}": run npm version and commit what it changes`);
    }
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
  const { version } = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
  for (const [file, values] of Object.entries(LINES)) {
    const full = path.join(root, file);
    let text = fs.readFileSync(full, "utf8");
    for (const [key, value] of values(version)) {
      const match = find(text, file, key);
      text = text.replace(match[0], `${match[1]}"${value}"`);
    }
    fs.writeFileSync(full, text);
    console.log(`${file}: ${values(version).map(([, value]) => value).join(", ")}`);
  }
  checkVersionFiles(root, version);
}
