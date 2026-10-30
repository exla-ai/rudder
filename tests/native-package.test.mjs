import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import test from "node:test";

const execute = promisify(execFile);
const script = fileURLToPath(new URL("../scripts/copy-native.mjs", import.meta.url));
async function fixture(t) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "rudder-native-package-"));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const artifacts = path.join(root, "prebuilt");
  await fs.mkdir(artifacts);
  const run = () => execute(process.execPath, [script], {
    cwd: root, env: { ...process.env, RUDDER_NATIVE_PREBUILTS: artifacts }, timeout: 10_000,
  });
  return { root, artifacts, run };
}

test("native package retains the matching workspace adapter for every platform", async (t) => {
  const { root, artifacts, run } = await fixture(t);
  const keys = ["darwin-arm64", "darwin-x64", "linux-x64"];
  for (const key of keys) {
    const directory = path.join(artifacts, `rudder-native-${key}`);
    await fs.mkdir(directory);
    await fs.writeFile(path.join(directory, "rudder-native"), `dashboard:${key}`);
    await fs.writeFile(path.join(directory, "rudder-jj"), `workspace-adapter:${key}`);
  }
  await run();
  for (const key of keys) {
    for (const [name, expected] of [["rudder-native", `dashboard:${key}`], ["rudder-jj", `workspace-adapter:${key}`]]) {
      const output = path.join(root, "dist/native", key, name);
      assert.equal(await fs.readFile(output, "utf8"), expected);
      if (process.platform !== "win32") assert.equal((await fs.stat(output)).mode & 0o777, 0o755);
    }
  }
});

test("an incomplete platform artifact fails before publishing its dashboard", async (t) => {
  const { root, artifacts, run } = await fixture(t);
  const directory = path.join(artifacts, "rudder-native-linux-x64");
  await fs.mkdir(directory);
  await fs.writeFile(path.join(directory, "rudder-native"), "dashboard-without-adapter");
  await assert.rejects(run, (error) => /missing its rudder-jj companion/.test(error.stderr));
  await assert.rejects(fs.stat(path.join(root, "dist/native/linux-x64/rudder-native")), { code: "ENOENT" });
});

test("local build packages both executables and removes the obsolete flat dashboard", async (t) => {
  const { root, run } = await fixture(t);
  const extension = process.platform === "win32" ? ".exe" : "";
  await fs.mkdir(path.join(root, "target/release"), { recursive: true });
  await fs.mkdir(path.join(root, "dist/native"), { recursive: true });
  await fs.writeFile(path.join(root, `target/release/rudder-native${extension}`), "local-dashboard");
  await fs.writeFile(path.join(root, `target/release/rudder-jj${extension}`), "local-adapter");
  await fs.writeFile(path.join(root, "dist/native/rudder-native"), "obsolete");
  await run();
  const key = `${process.platform}-${process.arch}`;
  assert.equal(await fs.readFile(path.join(root, `dist/native/${key}/rudder-jj${extension}`), "utf8"), "local-adapter");
  await assert.rejects(fs.stat(path.join(root, "dist/native/rudder-native")), { code: "ENOENT" });
});
