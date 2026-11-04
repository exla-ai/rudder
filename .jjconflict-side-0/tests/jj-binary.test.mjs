import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import fsp from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  MANAGED_JJ_ASSETS,
  MANAGED_JJ_VERSION,
  installManagedJj,
  managedJjAsset,
} from "../dist/jj-binary.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const distModule = path.resolve(__dirname, "../dist/jj-binary.js");

async function tempDir(t, prefix) {
  const dir = await fsp.mkdtemp(path.join(os.tmpdir(), prefix));
  t.after(() => fsp.rm(dir, { recursive: true, force: true }));
  return dir;
}

/** A jj release-shaped archive (./jj, ./LICENSE) holding a fake jj. */
async function fakeArchive(t, { withBinary = true } = {}) {
  const dir = await tempDir(t, "rudder-jj-archive-");
  const content = path.join(dir, "content");
  await fsp.mkdir(content);
  await fsp.writeFile(path.join(content, "LICENSE"), "Apache-2.0\n");
  if (withBinary) {
    await fsp.writeFile(path.join(content, "jj"), `#!/bin/sh\necho "jj ${MANAGED_JJ_VERSION}-fake"\n`, { mode: 0o755 });
  }
  const archive = path.join(dir, "jj.tar.gz");
  const tar = spawnSync("tar", ["-czf", archive, "-C", content, "."], { encoding: "utf8" });
  assert.equal(tar.status, 0, tar.stderr);
  const bytes = await fsp.readFile(archive);
  return { bytes, sha256: createHash("sha256").update(bytes).digest("hex") };
}

async function serve(t, bytes) {
  const server = http.createServer((req, res) => {
    if (req.url === "/jj.tar.gz") {
      res.writeHead(200, { "content-type": "application/gzip" });
      res.end(bytes);
      return;
    }
    res.writeHead(404);
    res.end();
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  return `http://127.0.0.1:${server.address().port}`;
}

test("every platform Rudder ships a jj for is pinned to a checksum", () => {
  for (const key of ["darwin-arm64", "darwin-x64", "linux-x64", "linux-arm64"]) {
    const asset = managedJjAsset(...key.split("-"));
    assert.ok(asset, `${key} has a managed jj`);
    assert.match(asset.sha256, /^[0-9a-f]{64}$/);
    assert.equal(
      asset.url,
      `https://github.com/jj-vcs/jj/releases/download/v${MANAGED_JJ_VERSION}/jj-v${MANAGED_JJ_VERSION}-${MANAGED_JJ_ASSETS[key].target}.tar.gz`,
    );
  }
  assert.equal(managedJjAsset("win32", "x64"), null, "no managed jj where there is no tar.gz release");
});

test("a verified archive installs a jj that runs", async (t) => {
  const { bytes, sha256 } = await fakeArchive(t);
  const base = await serve(t, bytes);
  const dest = path.join(await tempDir(t, "rudder-jj-home-"), "bin", "jj", MANAGED_JJ_VERSION, "jj");
  await installManagedJj({ url: `${base}/jj.tar.gz`, sha256, dest });
  const run = spawnSync(dest, ["--version"], { encoding: "utf8" });
  assert.equal(run.status, 0);
  assert.equal(run.stdout.trim(), `jj ${MANAGED_JJ_VERSION}-fake`);
  const leftovers = (await fsp.readdir(path.dirname(path.dirname(dest)))).filter((name) => name.startsWith(".install-"));
  assert.deepEqual(leftovers, [], "the staging directory is cleaned up");
});

test("a download that fails its checksum installs nothing", async (t) => {
  const { bytes } = await fakeArchive(t);
  const base = await serve(t, bytes);
  const dest = path.join(await tempDir(t, "rudder-jj-home-"), "bin", "jj", MANAGED_JJ_VERSION, "jj");
  await assert.rejects(
    installManagedJj({ url: `${base}/jj.tar.gz`, sha256: "0".repeat(64), dest }),
    /checksum/,
  );
  assert.equal(fs.existsSync(dest), false);
  await assert.rejects(installManagedJj({ url: `${base}/missing.tar.gz`, sha256: "0".repeat(64), dest }), /HTTP 404/);
});

test("an archive without a jj binary installs nothing", async (t) => {
  const { bytes, sha256 } = await fakeArchive(t, { withBinary: false });
  const base = await serve(t, bytes);
  const dest = path.join(await tempDir(t, "rudder-jj-home-"), "bin", "jj", MANAGED_JJ_VERSION, "jj");
  await assert.rejects(installManagedJj({ url: `${base}/jj.tar.gz`, sha256, dest }), /no jj binary/);
  assert.equal(fs.existsSync(dest), false);
});

test("a jj Rudder set up earlier is used without downloading again", async (t) => {
  const home = await tempDir(t, "rudder-jj-home-");
  const dir = path.join(home, "bin", "jj", MANAGED_JJ_VERSION);
  await fsp.mkdir(dir, { recursive: true });
  await fsp.writeFile(path.join(dir, "jj"), `#!/bin/sh\necho "jj ${MANAGED_JJ_VERSION}-fake"\n`, { mode: 0o755 });
  // A child process, so PATH and RUDDER_HOME are its own. The managed directory
  // is already on PATH (a parent rudder put it there): no probe, no download.
  const script = `
    const jj = await import(${JSON.stringify(distModule)});
    const source = await jj.ensureJjAvailable({ onInstall: () => console.log("DOWNLOADED") });
    console.log(JSON.stringify({ source, first: process.env.PATH.split(":")[0] }));
  `;
  const run = spawnSync(process.execPath, ["--input-type=module", "-e", script], {
    encoding: "utf8",
    env: { ...process.env, RUDDER_HOME: home, PATH: `${dir}:/usr/bin:/bin` },
  });
  assert.equal(run.status, 0, run.stderr);
  assert.doesNotMatch(run.stdout, /DOWNLOADED/);
  assert.deepEqual(JSON.parse(run.stdout.trim().split("\n").pop()), { source: "managed", first: dir });
});
