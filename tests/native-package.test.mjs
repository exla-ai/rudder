import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { execute, nativeFixture } from "./helpers/native-artifacts.mjs";
import { MANIFEST_NAME, NATIVE_PLATFORMS } from "../scripts/native-artifacts.mjs";

const script = fileURLToPath(new URL("../scripts/copy-native.mjs", import.meta.url));
const run = (f, env = f.prebuiltEnv()) => execute(process.execPath, [script], { cwd: f.root, env, timeout: 15_000 });

test("native package retains the matching manifested pair for every release platform", async t => {
  const f = await nativeFixture(t);
  // A skipped Cargo build must not mix a stale local executable into the set.
  await fs.mkdir(path.join(f.root, "target/release"), { recursive: true });
  await fs.writeFile(path.join(f.root, "target/release/rudder-native"), "stale-incomplete-dashboard");
  await run(f);
  for (const platform of NATIVE_PLATFORMS) {
    for (const [name, expected] of [["rudder-native", `dashboard:${platform}`], ["rudder-jj", `workspace-adapter:${platform}`]]) {
      const output = path.join(f.root, "dist/native", platform, name);
      assert.equal(await fs.readFile(output, "utf8"), expected);
      if (process.platform !== "win32") assert.equal((await fs.stat(output)).mode & 0o777, 0o755);
    }
    const manifest = JSON.parse(await fs.readFile(path.join(f.root, "dist/native", platform, MANIFEST_NAME), "utf8"));
    assert.equal(manifest.sourceRevision, f.revision);
  }
});

test("an incomplete or corrupt platform prevents any platform output from being replaced", async t => {
  for (const problem of ["missing-buddy", "corrupt", "wrong-source"]) await t.test(problem, async t => {
    const f = await nativeFixture(t);
    const platform = "linux-x64";
    if (problem === "missing-buddy") await fs.rm(path.join(f.directory(platform), "rudder-jj"));
    if (problem === "corrupt") await fs.writeFile(path.join(f.directory(platform), "rudder-jj"), "corrupt");
    if (problem === "wrong-source") await f.mutateManifest(platform, m => m.sourceRevision = "0".repeat(40));
    const previous = path.join(f.root, "dist/native/darwin-arm64/rudder-native");
    await fs.mkdir(path.dirname(previous), { recursive: true });
    await fs.writeFile(previous, "previous-package");
    await assert.rejects(run(f));
    assert.equal(await fs.readFile(previous, "utf8"), "previous-package");
    await assert.rejects(fs.stat(path.join(f.root, "dist/native/linux-x64")), { code: "ENOENT" });
  });
});

test("local source archive packaging needs no Git and copies both executables", async t => {
  const f = await nativeFixture(t, { git: false, complete: false });
  const extension = process.platform === "win32" ? ".exe" : "";
  await fs.mkdir(path.join(f.root, "target/release"), { recursive: true });
  await fs.mkdir(path.join(f.root, "dist/native"), { recursive: true });
  await fs.writeFile(path.join(f.root, `target/release/rudder-native${extension}`), "local-dashboard");
  await fs.writeFile(path.join(f.root, `target/release/rudder-jj${extension}`), "local-adapter");
  await fs.writeFile(path.join(f.root, "dist/native/rudder-native"), "obsolete");
  await run(f, f.env);
  const key = `${process.platform}-${process.arch}`;
  assert.equal(await fs.readFile(path.join(f.root, `dist/native/${key}/rudder-jj${extension}`), "utf8"), "local-adapter");
  await assert.rejects(fs.stat(path.join(f.root, "dist/native/rudder-native")), { code: "ENOENT" });
});

test("missing local companion fails before copying its dashboard", async t => {
  const f = await nativeFixture(t, { git: false, complete: false });
  const extension = process.platform === "win32" ? ".exe" : "";
  await fs.mkdir(path.join(f.root, "target/release"), { recursive: true });
  await fs.writeFile(path.join(f.root, `target/release/rudder-native${extension}`), "incomplete");
  await assert.rejects(run(f, f.env), error => /missing rudder-jj/.test(error.stderr));
  await assert.rejects(fs.stat(path.join(f.root, "dist")), { code: "ENOENT" });
});

test("an explicit Cargo target directory supplies the pair without falling back to stale local output", async t => {
  const f = await nativeFixture(t, { git: false, complete: false });
  const cache = path.join(f.root, "selected-target");
  const extension = process.platform === "win32" ? ".exe" : "";
  await fs.mkdir(path.join(cache, "release"), { recursive: true });
  await fs.mkdir(path.join(f.root, "target/release"), { recursive: true });
  await fs.writeFile(path.join(f.root, `target/release/rudder-native${extension}`), "stale-default-target");
  await fs.writeFile(path.join(cache, "release", `rudder-native${extension}`), "selected-dashboard");
  await fs.writeFile(path.join(cache, "release", `rudder-jj${extension}`), "selected-buddy");
  await run(f, { ...f.env, CARGO_TARGET_DIR: cache });
  assert.equal(await fs.readFile(path.join(f.root, `dist/native/${process.platform}-${process.arch}/rudder-native${extension}`), "utf8"), "selected-dashboard");
});

test("the published copy-native entrypoint includes its shared manifest helper", async t => {
  const f = await nativeFixture(t, { git: false, complete: false });
  const repository = fileURLToPath(new URL("../", import.meta.url));
  const pkg = JSON.parse(await fs.readFile(path.join(repository, "package.json"), "utf8"));
  assert.ok(pkg.files.includes("scripts/native-artifacts.mjs"));
  await fs.mkdir(path.join(f.root, "scripts"));
  for (const name of ["copy-native.mjs", "native-artifacts.mjs"]) await fs.copyFile(path.join(repository, "scripts", name), path.join(f.root, "scripts", name));
  await fs.writeFile(path.join(f.root, "package.json"), JSON.stringify({ name: "rudder-native-packlist-fixture", version: "1.0.0", type: "module", files: pkg.files }));
  const packed = JSON.parse((await execute("npm", ["pack", "--dry-run", "--ignore-scripts", "--json"], { cwd: f.root, env: f.env, timeout: 30_000 })).stdout);
  const names = packed[0].files.map(file => file.path);
  assert.ok(names.includes("scripts/native-artifacts.mjs"));
  assert.ok(names.includes("scripts/copy-native.mjs"));
});
