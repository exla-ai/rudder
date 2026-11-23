import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { execute, nativeFixture } from "./helpers/native-artifacts.mjs";
import { MANIFEST_NAME, NATIVE_PLATFORMS } from "../scripts/native-artifacts.mjs";
import { verifyTarball } from "../scripts/release.mjs";

// The tag run publishes a tarball the home packed. It must hold the tagged
// commit's package.json and exactly the native pairs staged for that commit.
test("the tag run accepts only a tarball bound to the tagged commit and its staged native pairs", async t => {
  const f = await nativeFixture(t);
  const cwd = process.cwd();
  process.chdir(f.root);
  t.after(() => process.chdir(cwd));
  const packageJson = await fs.readFile("package.json", "utf8");
  const ctx = { revision: f.revision, packageJson };
  const staged = {};
  for (const platform of NATIVE_PLATFORMS) staged[platform] = JSON.parse(await fs.readFile(path.join(f.directory(platform), MANIFEST_NAME), "utf8"));

  let count = 0;
  async function pack(change = async () => {}) {
    const dir = path.join(f.root, `pack-${count += 1}`);
    await fs.mkdir(path.join(dir, "package/dist/native"), { recursive: true });
    await fs.writeFile(path.join(dir, "package/package.json"), packageJson);
    await fs.writeFile(path.join(dir, "package/dist/index.js"), "");
    for (const platform of NATIVE_PLATFORMS) await fs.cp(f.directory(platform), path.join(dir, "package/dist/native", platform), { recursive: true });
    await change(dir);
    // npm tarballs list files only, never directories.
    const files = (await fs.readdir(dir, { recursive: true, withFileTypes: true })).filter(entry => entry.isFile())
      .map(entry => path.relative(dir, path.join(entry.parentPath, entry.name)));
    await execute("tar", ["-czf", `${dir}.tgz`, "-C", dir, ...files]);
    return `${dir}.tgz`;
  }

  assert.match((await verifyTarball(await pack(), ctx, staged)).integrity, /^sha512-/);
  await assert.rejects(verifyTarball(await pack(dir => fs.writeFile(path.join(dir, "package/package.json"), packageJson.replace("1.0.0", "1.0.1"))), ctx, staged),
    /package\.json in .* differs/);
  await assert.rejects(verifyTarball(await pack(dir => fs.writeFile(path.join(dir, "package/dist/native/linux-x64/extra"), "x")), ctx, staged),
    /exactly the darwin-arm64 and linux-x64 native pairs/);
  // Same length as the real one, so only the hash can catch it.
  await assert.rejects(verifyTarball(await pack(dir => fs.writeFile(path.join(dir, "package/dist/native/darwin-arm64/rudder-jj"), "workspace-adapter:darwin-arm65")), ctx, staged),
    /SHA256 mismatch/);
  const other = structuredClone(staged);
  other["darwin-arm64"].files["rudder-native"].sha256 = "0".repeat(64);
  await assert.rejects(verifyTarball(await pack(), ctx, other), /darwin-arm64 pair in .* is not the one staged/);
});
