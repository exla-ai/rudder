import assert from "node:assert/strict";
import test from "node:test";

import { compareSemver, isUnpublishedYet, shouldAutoUpdateFromPackageRoot } from "../dist/version-check.js";

test("compareSemver orders patch/minor/major versions", () => {
  assert.equal(compareSemver("2.2.4", "2.2.5"), -1);
  assert.equal(compareSemver("2.3.0", "2.2.9"), 1);
  assert.equal(compareSemver("3.0.0", "3.0.0"), 0);
  assert.equal(compareSemver("3.0.0-beta.1", "3.0.0"), -1);
});

test("SemVer prerelease precedence follows the specification and ignores build metadata", () => {
  const ordered = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0"];
  for (let i = 0; i < ordered.length; i++) {
    for (let j = 0; j < ordered.length; j++) assert.equal(compareSemver(ordered[i], ordered[j]), Math.sign(i - j));
  }
  assert.equal(compareSemver("4.0.0-alpha.1+build.4", "4.0.0-alpha.1+build.99"), 0);
  assert.equal(compareSemver("4.0.0+build.0", "4.0.0"), 0);
  assert.equal(compareSemver("4.0.0-alpha-beta.2", "4.0.0-alpha-beta.11"), -1);
  assert.equal(compareSemver("4.0.0-1", "4.0.0-alpha"), -1);
});

test("v4 alpha can upgrade to stable v4 without downgrading to stable v3", () => {
  assert.equal(compareSemver("4.0.0-alpha.1", "4.0.0"), -1);
  assert.equal(compareSemver("4.0.0-alpha.1", "3.99.99"), 1);
  assert.equal(compareSemver("4.0.0-alpha.2", "4.0.0-alpha.10"), -1);
  assert.equal(compareSemver("4.0.0-alpha.10", "4.0.0-alpha.10"), 0);
});

test("numeric identifiers retain precision, and short legacy inputs keep their fallback", () => {
  assert.equal(compareSemver("1.2.9007199254740992", "1.2.9007199254740993"), -1);
  assert.equal(compareSemver("4.0.0-alpha.9007199254740992", "4.0.0-alpha.9007199254740993"), -1);
  assert.equal(compareSemver("v4.0.0-alpha.1", "4.0.0"), -1);
  assert.equal(compareSemver("3.9", "3.9.0"), 0);
  assert.equal(compareSemver("", "0.0.0"), 0);
  assert.equal(compareSemver("nonsense", "2.0.0"), -1);
  assert.equal(compareSemver("3.09.0", "3.9.0"), 0);
});

test("auto-update eligibility skips dev, npx, CI, and disabled environments", (t) => {
  const oldDisable = process.env.RUDDER_DISABLE_AUTO_UPDATE;
  const oldSkip = process.env.RUDDER_SKIP_AUTO_UPDATE;
  const oldCheck = process.env.RUDDER_DISABLE_UPDATE_CHECK;
  const oldCi = process.env.CI;
  t.after(() => {
    restoreEnv("RUDDER_DISABLE_AUTO_UPDATE", oldDisable);
    restoreEnv("RUDDER_SKIP_AUTO_UPDATE", oldSkip);
    restoreEnv("RUDDER_DISABLE_UPDATE_CHECK", oldCheck);
    restoreEnv("CI", oldCi);
  });

  delete process.env.RUDDER_DISABLE_AUTO_UPDATE;
  delete process.env.RUDDER_SKIP_AUTO_UPDATE;
  delete process.env.RUDDER_DISABLE_UPDATE_CHECK;
  delete process.env.CI;

  assert.equal(shouldAutoUpdateFromPackageRoot("/usr/local/lib/node_modules/@viraatdas/rudder"), true);
  assert.equal(shouldAutoUpdateFromPackageRoot(process.cwd()), false, "source checkout with src/main.ts is skipped");
  assert.equal(shouldAutoUpdateFromPackageRoot("/tmp/.npm/_npx/abc/node_modules/@viraatdas/rudder"), false);

  process.env.RUDDER_DISABLE_AUTO_UPDATE = "1";
  assert.equal(shouldAutoUpdateFromPackageRoot("/usr/local/lib/node_modules/@viraatdas/rudder"), false);
});

function restoreEnv(key, value) {
  if (value === undefined) {
    delete process.env[key];
  } else {
    process.env[key] = value;
  }
}

test("a publish that has not propagated is named, not dumped as an npm failure", () => {
  // What the registry actually returns in the window between a version being
  // readable in the packument and every replica being able to resolve it. The
  // updater used to inherit npm's stdio, so this arrived as a five-line error
  // block that reads like rudder itself is broken.
  const etarget = [
    "npm error code ETARGET",
    "npm error notarget No matching version found for @viraatdas/rudder@2.14.36.",
    "npm error notarget In most cases you or one of your dependencies are requesting",
  ].join("\n");
  assert.equal(isUnpublishedYet(etarget), true);

  // A genuine failure must NOT be swallowed by that branch.
  assert.equal(isUnpublishedYet("npm error code EACCES\nnpm error syscall mkdir"), false);
  assert.equal(isUnpublishedYet(""), false);
});
