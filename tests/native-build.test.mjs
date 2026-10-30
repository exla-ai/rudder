import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { NATIVE_CARGO_ARGS } from "../scripts/build-native.mjs";
import { checkedSourceRevision, createNativeManifest, MANIFEST_NAME, readNativeBinary, verifyNativeArtifacts } from "../scripts/native-artifacts.mjs";
import { execute, nativeFixture } from "./helpers/native-artifacts.mjs";

const script = fileURLToPath(new URL("../scripts/build-native.mjs", import.meta.url));
async function cargoTrap(f) {
  const bin = path.join(f.root, "bin");
  await fs.mkdir(bin);
  await fs.writeFile(path.join(bin, "cargo"), `#!/usr/bin/env node
const fs=require('node:fs');
fs.appendFileSync('cargo-invocations.jsonl',JSON.stringify(process.argv.slice(2))+'\\n');
if(process.env.NATIVE_TEST_CARGO_FAIL)process.exit(7);
fs.mkdirSync('target/release',{recursive:true});
fs.writeFileSync('target/release/rudder-native','native');
if(!process.env.NATIVE_TEST_MISSING_BUDDY)fs.writeFileSync('target/release/rudder-jj','buddy');
`, { mode: 0o755 });
  return { ...f.env, PATH: `${bin}${path.delimiter}${f.env.PATH}` };
}
const run = (f, env) => execute(process.execPath, [script], { cwd: f.root, env, timeout: 15_000 });

test("ordinary archive builds use one exact Cargo graph without requiring Git", async t => {
  const f = await nativeFixture(t, { git: false, complete: false });
  await run(f, await cargoTrap(f));
  const calls = (await fs.readFile(path.join(f.root, "cargo-invocations.jsonl"), "utf8")).trim().split("\n").map(JSON.parse);
  assert.deepEqual(calls, [["build", "--release", "--locked", "--manifest-path", "Cargo.toml", "-p", "rudder-native", "-p", "rudder-vcs-bridge", "--features", "rudder-vcs-bridge/cli", "--bin", "rudder-native", "--bin", "rudder-jj"]]);
  assert.deepEqual(calls[0], [...NATIVE_CARGO_ARGS]);
});

test("Cargo failure or missing companion fails local builds", async t => {
  for (const mode of ["NATIVE_TEST_CARGO_FAIL", "NATIVE_TEST_MISSING_BUDDY"]) await t.test(mode, async t => {
    const f = await nativeFixture(t, { git: false, complete: false });
    const env = { ...await cargoTrap(f), [mode]: "1" };
    await assert.rejects(run(f, env), error => mode === "NATIVE_TEST_CARGO_FAIL" ? error.code === 7 : /missing rudder-jj/.test(error.stderr));
  });
});

test("both npm name variants reuse complete verified pairs without invoking Cargo", async t => {
  const f = await nativeFixture(t);
  const env = { ...await cargoTrap(f), RUDDER_NATIVE_PREBUILTS: f.artifacts };
  for (const name of ["@exla/rudder", "@viraatdas/rudder"]) {
    await fs.writeFile(path.join(f.root, "package.json"), JSON.stringify({ name, version: "1.0.0" }));
    assert.match((await run(f, env)).stdout, /Cargo build skipped/);
  }
  await assert.rejects(fs.stat(path.join(f.root, "cargo-invocations.jsonl")), { code: "ENOENT" });
});

test("bad prebuilt input never falls back to Cargo", async t => {
  const cases = {
    empty: async f => ({ RUDDER_NATIVE_PREBUILTS: "" }),
    absent: async f => ({ RUDDER_NATIVE_PREBUILTS: path.join(f.root, "absent") }),
    missingPlatform: async f => { await fs.rm(f.directory("darwin-x64"), { recursive: true }); },
    missingBuddy: async f => { await fs.rm(path.join(f.directory("linux-x64"), "rudder-jj")); },
    missingManifest: async f => { await fs.rm(path.join(f.directory("linux-x64"), MANIFEST_NAME)); },
    wrongSource: async f => f.mutateManifest("linux-x64", m => m.sourceRevision = "0".repeat(40)),
    wrongExpectedRevision: async () => ({ RUDDER_NATIVE_SOURCE_REVISION: "0".repeat(40) }),
    sameSizeCorruption: async f => { const p = path.join(f.directory("linux-x64"), "rudder-jj"); const b = await fs.readFile(p); b[0] ^= 1; await fs.writeFile(p, b); },
    wrongSize: async f => f.mutateManifest("linux-x64", m => m.files["rudder-jj"].size++),
    wrongPlatform: async f => f.mutateManifest("linux-x64", m => m.platform = "darwin-x64"),
    wrongFeatures: async f => f.mutateManifest("linux-x64", m => m.features.push("rudder-vcs-bridge/local-development")),
    unknownField: async f => f.mutateManifest("linux-x64", m => m.acceptUnverified = true),
    traversal: async f => f.mutateManifest("linux-x64", m => m.files["../rudder-jj"] = m.files["rudder-jj"]),
    symlink: async f => { const p = path.join(f.directory("linux-x64"), "rudder-jj"); const external = path.join(f.root, "external-buddy"); await fs.rename(p, external); await fs.symlink(external, p); },
    platformSymlink: async f => { const p = f.directory("linux-x64"); const external = path.join(f.root, "external-platform"); await fs.rename(p, external); await fs.symlink(external, p); },
    rootSymlink: async f => { const external = path.join(f.root, "external-root"); await fs.rename(f.artifacts, external); await fs.symlink(external, f.artifacts); },
    extraPlatform: async f => fs.mkdir(path.join(f.artifacts, "rudder-native-linux-arm64")),
    malformedManifest: async f => fs.writeFile(path.join(f.directory("linux-x64"), MANIFEST_NAME), "{"),
    wrongProfile: async f => f.mutateManifest("linux-x64", m => m.buildProfile = "debug"),
    unsafeSize: async f => f.mutateManifest("linux-x64", m => m.files["rudder-jj"].size = Number.MAX_SAFE_INTEGER + 1),
    oversizedManifest: async f => fs.writeFile(path.join(f.directory("linux-x64"), MANIFEST_NAME), " ".repeat(16 * 1024 + 1)),
    dirtyTrackedSource: async f => fs.appendFile(path.join(f.root, "native/fixture.rs"), "// modified\n"),
    hiddenIndexModification: async f => { await execute("git", ["update-index", "--assume-unchanged", "native/fixture.rs"], { cwd: f.root, env: f.env }); await fs.appendFile(path.join(f.root, "native/fixture.rs"), "// hidden dirty source\n"); },
    untrackedSource: async f => fs.writeFile(path.join(f.root, "native/new.rs"), "// untracked\n"),
    unsignedArchive: async f => { await fs.rm(path.join(f.root, ".git"), { recursive: true }); await fs.writeFile(path.join(f.root, ".source-revision"), f.revision); },
    parentGitDiscovery: async f => { f.root = path.join(f.root, "nested-archive"); await fs.mkdir(f.root); },
    modifiedPackageVersion: async f => fs.writeFile(path.join(f.root, "package.json"), JSON.stringify({ name: "@exla/rudder", version: "99.0.0" })),
    unapprovedPackageName: async f => fs.writeFile(path.join(f.root, "package.json"), JSON.stringify({ name: "unapproved-package", version: "1.0.0" })),
  };
  for (const [name, mutate] of Object.entries(cases)) await t.test(name, async t => {
    const f = await nativeFixture(t);
    const env = { ...await cargoTrap(f), RUDDER_NATIVE_PREBUILTS: f.artifacts, ...await mutate(f) };
    await assert.rejects(run(f, env));
    await assert.rejects(fs.stat(path.join(f.root, "cargo-invocations.jsonl")), { code: "ENOENT" });
  });
});

test("manifest creation refuses wrong revision and changed source", async t => {
  const f = await nativeFixture(t);
  await assert.rejects(createNativeManifest({ directory: f.directory("linux-x64"), platform: "linux-x64", cwd: f.root, expectedRevision: "0".repeat(40) }));
  await fs.appendFile(path.join(f.root, "Cargo.lock"), "# changed\n");
  assert.throws(() => checkedSourceRevision({ cwd: f.root, expectedRevision: f.revision }));
  await assert.rejects(createNativeManifest({ directory: f.directory("linux-x64"), platform: "linux-x64", cwd: f.root, expectedRevision: f.revision }));
});

test("artifact CLI produces the host manifest and verifies the full release set", async t => {
  const f = await nativeFixture(t);
  const helper = fileURLToPath(new URL("../scripts/native-artifacts.mjs", import.meta.url));
  const host = `${process.platform}-${process.arch}`;
  assert.match((await execute(process.execPath, [helper, "create", "--directory", f.directory(host), "--platform", host, "--revision", f.revision], { cwd: f.root, env: f.env, timeout: 15_000 })).stdout, /Created/);
  assert.match((await execute(process.execPath, [helper, "verify", "--root", f.artifacts, "--revision", f.revision], { cwd: f.root, env: f.env, timeout: 15_000 })).stdout, /Verified 3 native pairs/);
  const wrong = host === "linux-x64" ? "darwin-arm64" : "linux-x64";
  await assert.rejects(execute(process.execPath, [helper, "create", "--directory", f.directory(wrong), "--platform", wrong, "--revision", f.revision], { cwd: f.root, env: f.env, timeout: 15_000 }));
});

test("staging revalidates hashes and rejects oversized files before reading", async t => {
  const f = await nativeFixture(t);
  const verified = await verifyNativeArtifacts({ root: f.artifacts, cwd: f.root, expectedRevision: f.revision });
  const file = verified.artifacts[0].files[0];
  const destination = path.join(f.root, "staged");
  const original = await fs.readFile(file.path);
  const changed = Buffer.from(original); changed[0] ^= 1;
  await fs.writeFile(file.path, changed);
  await assert.rejects(readNativeBinary(file.path, file, destination), /SHA256 mismatch/);
  const large = path.join(f.root, "oversized");
  await fs.writeFile(large, "");
  await fs.truncate(large, 512 * 1024 * 1024 + 1);
  await assert.rejects(readNativeBinary(large), /size or type/);
  const data = Buffer.alloc(4 * 1024 * 1024, 0x5a);
  const input = path.join(f.root, "large-valid");
  await fs.writeFile(input, data);
  assert.deepEqual(await readNativeBinary(input), { size: data.length, sha256: createHash("sha256").update(data).digest("hex") });
});

test("Docker uses the same single Cargo graph and keeps cleanup plus paired output copies", async () => {
  const dockerfile = await fs.readFile(new URL("../cloud/worker/Dockerfile", import.meta.url), "utf8");
  const builder = dockerfile.split("FROM debian:")[0].replace(/\\\n\s*/g, " ");
  assert.equal([...builder.matchAll(/cargo build/g)].length, 1);
  const command = builder.match(/&& (cargo build .+?) && cp /)[1].replace(/\s+/g, " ").trim();
  assert.deepEqual(command.split(" "), ["cargo", ...NATIVE_CARGO_ARGS]);
  assert.match(builder, /cargo clean --locked --release -p rudder-engine -p rudder-native -p rudder-vcs-bridge/);
  assert.match(builder, /cp target\/release\/rudder-native \/rudder-native/);
  assert.match(builder, /cp target\/release\/rudder-jj \/rudder-jj/);
});
