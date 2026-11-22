import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { generateKeyPairSync } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const scripts = fileURLToPath(new URL("../mac/scripts/", import.meta.url));
const candidate = "refs/heads/release-check/5.0.23";
const versionTag = "refs/tags/v5.0.23";
const macTag = "mac-v5.0.23-5000023";
const sha = "abc123".repeat(6) + "abcd";

test("only a verified candidate DMG and feed can be published from their version tag", (t) => {
  const dir = mkdtempSync(join(tmpdir(), "rudder-prepared-release-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  for (const subdir of ["bin", "mac/App", "prepared"]) mkdirSync(join(dir, subdir), { recursive: true });
  symlinkSync(scripts, join(dir, "mac/scripts"), "dir");
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  const publicBytes = publicKey.export({ format: "jwk" }).x;
  const privateBytes = privateKey.export({ format: "jwk" }).d;
  writeFileSync(join(dir, "mac/App/Info.plist"), `<key>SUPublicEDKey</key><string>${Buffer.from(publicBytes, "base64url").toString("base64")}</string>`);
  writeFileSync(join(dir, "package.json"), JSON.stringify({ version: "5.0.23" }));
  writeFileSync(join(dir, "mac/project.yml"), '  CURRENT_PROJECT_VERSION: "5000023"\n');
  writeFileSync(join(dir, "prepared/release.env"), "VERSION=5.0.23\nBUILD=5000023\nMINIMUM_SYSTEM=14.0\n");
  const dmg = join(dir, "prepared/Rudder.dmg");
  writeFileSync(dmg, "original");
  writeFileSync(join(dir, "bin/gh"), `#!/bin/sh
case "$1 $2" in
  "api repos/exla-ai/rudder/git/matching-refs/tags/${macTag}") ;;
  "release create") printf '%s\\n' "$@" > release-args ;;
  "api repos/exla-ai/rudder/releases/latest") echo ${macTag} ;;
  *) exit 2 ;;
esac
`, { mode: 0o755 });
  function run(action, ref) {
    return spawnSync("bash", [join(scripts, "prepared-release.sh"), action, "prepared"], {
      cwd: dir,
      encoding: "utf8",
      env: {
        PATH: `${join(dir, "bin")}:${process.env.PATH}`,
        GITHUB_REF: ref,
        GITHUB_REPOSITORY: "exla-ai/rudder",
        GITHUB_SHA: sha,
        ...(action === "sign" && { SPARKLE_ED_PRIVATE_KEY: Buffer.from(privateBytes, "base64url").toString("base64") }),
      },
    });
  }

  const wrongBranch = run("sign", "refs/heads/main");
  assert.notEqual(wrongBranch.status, 0);
  assert.match(wrongBranch.stderr, /Candidate branch/);
  const signed = run("sign", candidate);
  assert.equal(signed.status, 0, signed.stderr);
  const verified = run("verify", versionTag);
  assert.equal(verified.status, 0, verified.stderr);

  const wrongTag = run("publish", "refs/tags/v5.0.24");
  assert.notEqual(wrongTag.status, 0);
  assert.match(wrongTag.stderr, /v5\.0\.23 tag/);
  writeFileSync(dmg, "modified"); // Same length, so the signature must catch the change.
  const tampered = run("publish", versionTag);
  assert.notEqual(tampered.status, 0);
  assert.match(tampered.stderr, /signature/);
  const argsFile = join(dir, "release-args");
  assert.equal(existsSync(argsFile), false);

  writeFileSync(dmg, "original");
  const published = run("publish", versionTag);
  assert.equal(published.status, 0, published.stderr);
  const args = readFileSync(argsFile, "utf8").trimEnd().split("\n");
  assert.deepEqual(args.slice(0, 3), ["release", "create", macTag]);
  assert.ok(args.includes("prepared/Rudder.dmg"));
  assert.ok(args.includes("prepared/appcast.xml"));
  assert.equal(args[args.indexOf("--target") + 1], sha);
  assert.ok(args.includes("--latest"));
});
