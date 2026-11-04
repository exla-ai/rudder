import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { createNativeManifest, NATIVE_PLATFORMS, MANIFEST_NAME } from "../../scripts/native-artifacts.mjs";

export const execute = promisify(execFile);
export async function nativeFixture(t, { git = true, complete = true } = {}) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "rudder-native-package-"));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const env = { ...process.env };
  for (const key of ["RUDDER_NATIVE_PREBUILTS", "RUDDER_NATIVE_SOURCE_REVISION", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET"]) delete env[key];
  let revision;
  if (git) {
    await fs.writeFile(path.join(root, "Cargo.toml"), "[workspace]\nmembers=[]\n");
    await fs.writeFile(path.join(root, "Cargo.lock"), "version = 4\n");
    await fs.mkdir(path.join(root, "native"));
    await fs.writeFile(path.join(root, "native", "fixture.rs"), "// native input\n");
    await fs.writeFile(path.join(root, "package.json"), JSON.stringify({ name: "@exla/rudder", version: "1.0.0" }));
    await execute("git", ["init", "--quiet", "--initial-branch=main"], { cwd: root, env });
    await execute("git", ["add", "Cargo.toml", "Cargo.lock", "native", "package.json"], { cwd: root, env });
    await execute("git", ["-c", "user.name=Native artifact fixture", "-c", "user.email=native-artifacts@example.invalid", "commit", "--quiet", "-m", "fixture"], { cwd: root, env });
    revision = (await execute("git", ["rev-parse", "HEAD"], { cwd: root, env })).stdout.trim();
    env.RUDDER_NATIVE_SOURCE_REVISION = revision;
  }
  const artifacts = path.join(root, "prebuilt");
  await fs.mkdir(artifacts);
  const directory = platform => path.join(artifacts, `rudder-native-${platform}`);
  if (complete) {
    for (const platform of NATIVE_PLATFORMS) {
      const dir = directory(platform);
      await fs.mkdir(dir);
      await fs.writeFile(path.join(dir, "rudder-native"), `dashboard:${platform}`);
      await fs.writeFile(path.join(dir, "rudder-jj"), `workspace-adapter:${platform}`);
      await createNativeManifest({ directory: dir, platform, cwd: root, expectedRevision: revision });
    }
  }
  const prebuiltEnv = () => ({ ...env, RUDDER_NATIVE_PREBUILTS: artifacts });
  const mutateManifest = async (platform, mutate) => {
    const file = path.join(directory(platform), MANIFEST_NAME);
    const value = JSON.parse(await fs.readFile(file, "utf8"));
    mutate(value);
    await fs.writeFile(file, JSON.stringify(value));
  };
  return { root, artifacts, env, revision, directory, prebuiltEnv, mutateManifest };
}
