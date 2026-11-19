#!/usr/bin/env node
// The Mac app carries Rudder's version: `npm version` runs this (package.json
// "version" script) so mac/project.yml always matches package.json. The build
// number Sparkle compares is derived from it (5.0.8 -> 5000008), so it only
// grows when the version does.
import fs from "node:fs";
import path from "node:path";

const root = path.resolve(path.dirname(new URL(import.meta.url).pathname), "..");
const { version } = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
const parts = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
if (!parts) throw new Error(`package.json version ${version} is not MAJOR.MINOR.PATCH; the Mac app needs a plain release version`);
const [major, minor, patch] = parts.slice(1).map(Number);
if (minor > 999 || patch > 999) throw new Error(`version ${version} does not fit the Mac build number`);
const build = String(major * 1_000_000 + minor * 1_000 + patch);

const file = path.join(root, "mac/project.yml");
const text = fs.readFileSync(file, "utf8");
const next = text
  .replace(/^(\s*MARKETING_VERSION: )"[^"]*"$/m, `$1"${version}"`)
  .replace(/^(\s*CURRENT_PROJECT_VERSION: )"[^"]*"$/m, `$1"${build}"`);
if (!next.includes(`MARKETING_VERSION: "${version}"`) || !next.includes(`CURRENT_PROJECT_VERSION: "${build}"`)) {
  throw new Error("mac/project.yml has no MARKETING_VERSION / CURRENT_PROJECT_VERSION line to set");
}
fs.writeFileSync(file, next);
console.log(`mac/project.yml: ${version} (${build})`);
