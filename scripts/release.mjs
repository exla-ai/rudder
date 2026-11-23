#!/usr/bin/env node
// Stages a Rudder release before its tag and publishes it from the tag. GitHub
// builds and tests nothing: the full checks run once on the home, the
// darwin-arm64 pair and the Mac app build on Offstage Macs, and the linux-x64
// pair and the npm tarball build on the home. Release CI
// (.github/workflows/release.yml) only moves files, checks their hashes and
// publishes them.
//
// A draft GitHub release per version, tagged v<version> and targeting the
// release commit, holds three parts. Each part is <part>.json, naming the
// version, the source revision and every file's sha256 and size, uploaded
// after its files:
//   darwin  rudder-native-darwin-arm64.tgz       Offstage, on the release-check push
//   mac     Rudder.dmg appcast.xml release.env   Offstage and the Sparkle key, same push
//   npm     exla-rudder-<version>.tgz            the home (stage)
//
//   node scripts/release.mjs draft            CI: create or retarget the draft for this commit
//   node scripts/release.mjs put-darwin       CI: stage native-dist/darwin-arm64 from .offstage/native.mjs
//   node scripts/release.mjs put-mac <dir>    CI: stage the signed DMG, its signed feed and release.env
//   node scripts/release.mjs stage            home: build linux-x64, pack and stage the npm tarball,
//                                             then print what the tag publishes
//   node scripts/release.mjs publish-npm      CI, tag: verify the npm tarball and publish it
//   node scripts/release.mjs fetch-mac <dir>  CI, tag: download and verify the Mac files
//   node scripts/release.mjs finish           CI, tag: delete the draft and release-check/<version>
//
// Every command needs GH_TOKEN with push access to the repo: drafts are
// invisible to anyone else.
import { createHash } from "node:crypto";
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { isDeepStrictEqual } from "node:util";
import { createNativeManifest, NATIVE_BINARIES, NATIVE_PLATFORMS, MANIFEST_NAME, verifyNativeArtifact, verifyNativeArtifacts } from "./native-artifacts.mjs";
import { checkVersionFiles, macBuild, WORKER_IMAGE_REPOSITORY } from "./version.mjs";

export const REPO = "exla-ai/rudder";
const PACKAGE = "@exla/rudder";
// The staged tarball and npm.json stay here for scripts/worker-image.mjs;
// work/ holds everything else and is outside the image build context.
export const STAGE = ".release";
const WORK = path.join(STAGE, "work");
const WAIT_MS = 95 * 60_000;
const NATIVE_FILES = [...NATIVE_BINARIES, MANIFEST_NAME].sort();
export const tarballName = version => `exla-rudder-${version}.tgz`;
const PART_FILES = {
  darwin: () => ["rudder-native-darwin-arm64.tgz"],
  mac: () => ["Rudder.dmg", "appcast.xml", "release.env"],
  npm: version => [tarballName(version)],
};

class ReleaseError extends Error {}
const check = (condition, message) => { if (!condition) throw new ReleaseError(message); };
const log = message => console.log(`release: ${message}`);
const mb = bytes => `${(bytes / 1024 ** 2).toFixed(1)} MB`;

function git(args) {
  return execFileSync("git", args, { encoding: "utf8", maxBuffer: 4 * 1024 ** 2, stdio: ["ignore", "pipe", "pipe"] });
}
function run(command, args, options = {}) {
  const result = spawnSync(command, args, { stdio: "inherit", ...options });
  check(!result.error && result.status === 0, `${command} ${args.join(" ")} failed${result.error ? `: ${result.error.message}` : ` (exit ${result.status})`}`);
}

/** The version and commit of the Git checkout this runs in. */
export function checkout() {
  const root = git(["rev-parse", "--show-toplevel"]).trim();
  check(fs.realpathSync(root) === fs.realpathSync(process.cwd()), "Run from the root of a Git checkout of the release commit");
  const revision = git(["rev-parse", "HEAD"]).trim();
  const packageJson = git(["show", "HEAD:package.json"]);
  const { name, version } = JSON.parse(packageJson);
  check(name === PACKAGE, `package.json names ${name}, not ${PACKAGE}`);
  check(/^\d+\.\d+\.\d+$/.test(version), `package.json version ${version} is not MAJOR.MINOR.PATCH; every release ships the Mac app, which needs one`);
  return { revision, version, packageJson, tag: `v${version}`, branch: `release-check/${version}` };
}

function ciContext(ref) {
  const ctx = checkout();
  const expected = ref === "tag" ? `refs/tags/${ctx.tag}` : `refs/heads/${ctx.branch}`;
  check(process.env.GITHUB_REPOSITORY === REPO, `This runs in ${REPO}, not ${process.env.GITHUB_REPOSITORY}`);
  check(process.env.GITHUB_SHA === ctx.revision, `GITHUB_SHA ${process.env.GITHUB_SHA} is not the checked-out commit ${ctx.revision}`);
  check(process.env.GITHUB_REF === expected, `package.json says ${ctx.version}, so this runs on ${expected}, not ${process.env.GITHUB_REF}`);
  return ctx;
}

export function github(token = process.env.GH_TOKEN) {
  check(token, `GH_TOKEN must hold a GitHub token with push access to ${REPO}`);
  async function request(method, url, { body, type, accept = "application/vnd.github+json", ok = [200] } = {}) {
    const response = await fetch(url.startsWith("https://") ? url : `https://api.github.com${url}`, {
      method, body, redirect: "manual", signal: AbortSignal.timeout(10 * 60_000),
      headers: { authorization: `Bearer ${token}`, accept, "x-github-api-version": "2022-11-28", "user-agent": "rudder-release", ...(type && { "content-type": type }) },
    });
    if (!ok.includes(response.status)) {
      throw new ReleaseError(`GitHub ${method} ${url} answered ${response.status}: ${(await response.text()).slice(0, 400)}`);
    }
    return response;
  }
  const json = async (method, url, body) => (await request(method, url, body === undefined ? {}
    : { body: JSON.stringify(body), type: "application/json", ok: [200, 201] })).json();
  return {
    request, json,
    async pages(url) {
      const all = [];
      for (let page = 1; page <= 30; page += 1) {
        const items = await json("GET", `${url}${url.includes("?") ? "&" : "?"}per_page=100&page=${page}`);
        all.push(...items);
        if (items.length < 100) return all;
      }
      throw new ReleaseError(`${url} has more than 3000 entries`);
    },
    async ref(name) {
      const response = await request("GET", `/repos/${REPO}/git/ref/${name}`, { ok: [200, 404] });
      return response.status === 200 ? (await response.json()).object.sha : null;
    },
  };
}

async function describe(file) {
  const hash = createHash("sha256");
  const sha512 = createHash("sha512");
  let size = 0;
  for await (const chunk of fs.createReadStream(file)) { hash.update(chunk); sha512.update(chunk); size += chunk.length; }
  return { sha256: hash.digest("hex"), size, integrity: `sha512-${sha512.digest("base64")}` };
}

async function releasesFor(api, ctx) {
  const all = (await api.pages(`/repos/${REPO}/releases`)).filter(release => release.tag_name === ctx.tag);
  check(all.every(release => release.draft), `${ctx.tag} is a published GitHub release; a Rudder version publishes only its Mac release, mac-v*`);
  check(all.length <= 1, `There are ${all.length} draft releases ${ctx.tag} (${all.map(r => r.id).join(", ")}); delete all but one with gh api -X DELETE repos/${REPO}/releases/<id>`);
  return all[0] ?? null;
}

async function theDraft(api, ctx) {
  const draft = await releasesFor(api, ctx);
  check(draft, `There is no draft release ${ctx.tag}: push ${ctx.revision} to ${ctx.branch} first`);
  check(draft.target_commitish === ctx.revision, `The draft release ${ctx.tag} stages ${draft.target_commitish}, not ${ctx.revision}: push ${ctx.revision} to ${ctx.branch}`);
  return draft;
}

async function assets(api, draft) {
  return api.pages(`/repos/${REPO}/releases/${draft.id}/assets`);
}

async function upload(api, draft, file, name) {
  for (const asset of (await assets(api, draft)).filter(asset => asset.name === name)) {
    await api.request("DELETE", `/repos/${REPO}/releases/assets/${asset.id}`, { ok: [204] });
  }
  const bytes = await fsp.readFile(file);
  const response = await api.request("POST", `https://uploads.github.com/repos/${REPO}/releases/${draft.id}/assets?name=${encodeURIComponent(name)}`,
    { body: bytes, type: "application/octet-stream", ok: [201] });
  const asset = await response.json();
  check(asset.name === name && asset.size === bytes.length, `GitHub stored ${name} as ${asset.name} with ${asset.size} bytes, not ${bytes.length}`);
}

async function download(api, asset, file) {
  const first = await api.request("GET", `/repos/${REPO}/releases/assets/${asset.id}`, { accept: "application/octet-stream", ok: [200, 302] });
  // The redirect goes to a signed storage URL; it must not get the token.
  const response = first.status === 200 ? first : await fetch(first.headers.get("location"), { signal: AbortSignal.timeout(10 * 60_000) });
  check(response.ok, `Downloading ${asset.name} answered ${response.status}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  check(bytes.length === asset.size, `${asset.name} downloaded ${bytes.length} bytes, not ${asset.size}`);
  await fsp.writeFile(file, bytes, { flag: "wx" });
}

/** The draft's <part>.json, checked for its shape and version, or null when it has none yet. */
async function readPart(api, draft, ctx, part) {
  const asset = (await assets(api, draft)).find(asset => asset.name === `${part}.json`);
  if (!asset) return null;
  const temporary = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-release-part-"));
  try {
    await download(api, asset, path.join(temporary, "part.json"));
    const manifest = JSON.parse(await fsp.readFile(path.join(temporary, "part.json"), "utf8"));
    check(manifest.format === 1 && manifest.part === part && manifest.version === ctx.version
      && /^[0-9a-f]{40}$/.test(manifest.revision)
      && isDeepStrictEqual(Object.keys(manifest.files ?? {}).sort(), PART_FILES[part](ctx.version).sort()), `${part}.json in ${ctx.tag} is not a ${ctx.version} ${part} part`);
    return manifest;
  } finally { await fsp.rm(temporary, { recursive: true, force: true }); }
}

/** Downloads a part's files into dir (made fresh) and checks each against its manifest. */
async function fetchPart(api, draft, ctx, part, dir) {
  const manifest = await readPart(api, draft, ctx, part);
  check(manifest, `${ctx.tag} has no ${part}.json: the ${part} part was never staged`);
  check(manifest.revision === ctx.revision, `${part}.json in ${ctx.tag} was staged from ${manifest.revision}, not ${ctx.revision}`);
  await fsp.rm(dir, { recursive: true, force: true });
  await fsp.mkdir(dir, { recursive: true });
  const present = await assets(api, draft);
  for (const [name, expected] of Object.entries(manifest.files)) {
    const asset = present.find(asset => asset.name === name);
    check(asset, `${ctx.tag} has ${part}.json but no ${name}`);
    await download(api, asset, path.join(dir, name));
    const actual = await describe(path.join(dir, name));
    check(actual.sha256 === expected.sha256 && actual.size === expected.size, `${name} in ${ctx.tag} does not match ${part}.json (sha256 ${actual.sha256}, expected ${expected.sha256})`);
  }
  return manifest;
}

async function putPart(api, draft, ctx, part, dir, extra) {
  const files = {};
  for (const name of PART_FILES[part](ctx.version)) {
    const { sha256, size } = await describe(path.join(dir, name));
    files[name] = { sha256, size };
  }
  const manifest = { format: 1, part, version: ctx.version, revision: ctx.revision, files, ...extra };
  const json = path.join(dir, `${part}.json`);
  await fsp.writeFile(json, `${JSON.stringify(manifest, null, 2)}\n`);
  for (const name of Object.keys(files)) await upload(api, draft, path.join(dir, name), name);
  await upload(api, draft, json, `${part}.json`);
  log(`staged ${part} for ${ctx.version} at ${ctx.revision}: ${Object.entries(files).map(([name, file]) => `${name} ${file.sha256}`).join(", ")}`);
  return manifest;
}

async function stagingRun(api, ctx) {
  const { workflow_runs: runs } = await api.json("GET",
    `/repos/${REPO}/actions/workflows/release.yml/runs?event=push&head_sha=${ctx.revision}&branch=${encodeURIComponent(ctx.branch)}&per_page=1`);
  return runs[0] ?? null;
}

/** Waits until CI has staged part for this commit, or its Release run ended without it. */
async function waitForPart(api, ctx, part, what) {
  const deadline = Date.now() + WAIT_MS;
  for (let said = 0; ; await sleep(15_000)) {
    // The run's state first: a run that completed before the part was read
    // really finished without it.
    const staging = await stagingRun(api, ctx);
    const draft = await releasesFor(api, ctx);
    if (draft?.target_commitish === ctx.revision) {
      const manifest = await readPart(api, draft, ctx, part);
      if (manifest?.revision === ctx.revision) return draft;
    }
    if (staging?.status === "completed") {
      check(staging.conclusion === "success", `The Release run for ${ctx.branch} ended ${staging.conclusion}: ${staging.html_url} (gh run view ${staging.id} --repo ${REPO} --log-failed)`);
      throw new ReleaseError(`The Release run ${staging.html_url} passed without staging ${part}.json for ${ctx.revision}`);
    }
    check(Date.now() < deadline, `Gave up after ${WAIT_MS / 60_000} minutes waiting for ${what}`);
    if (Date.now() - said >= 60_000) {
      log(`waiting for ${what} (${staging ? staging.html_url : `no Release run for ${ctx.branch} at ${ctx.revision} yet`})`);
      said = Date.now();
    }
  }
}

/** Unpacks a native pair archive into dir: exactly its three regular files. */
async function extractNative(archive, dir) {
  const names = execFileSync("tar", ["-tzf", archive], { encoding: "utf8", maxBuffer: 4096 }).trim().split("\n").sort();
  check(isDeepStrictEqual(names, NATIVE_FILES), `${path.basename(archive)} holds ${names.join(", ")}, not ${NATIVE_FILES.join(", ")}`);
  await fsp.mkdir(dir, { recursive: true });
  execFileSync("tar", ["-xzf", archive, "-C", dir, "--no-same-owner", ...NATIVE_FILES]);
  for (const name of NATIVE_FILES) check((await fsp.lstat(path.join(dir, name))).isFile(), `${name} in ${path.basename(archive)} is not a regular file`);
  for (const name of NATIVE_BINARIES) await fsp.chmod(path.join(dir, name), 0o755);
}

/**
 * Checks an npm tarball against the commit it claims: package.json byte for
 * byte, both native pairs bound to the revision with the expected manifests,
 * and the entry point. Returns its sha256, size and npm integrity.
 */
export async function verifyTarball(file, ctx, nativeManifests) {
  const entries = execFileSync("tar", ["-tzf", file], { encoding: "utf8", maxBuffer: 64 * 1024 ** 2 }).split("\n").filter(Boolean);
  check(entries.every(entry => entry.startsWith("package/") && !entry.split("/").includes("..")), `${path.basename(file)} has entries outside package/`);
  const nativeEntries = NATIVE_PLATFORMS.flatMap(platform => NATIVE_FILES.map(name => `package/dist/native/${platform}/${name}`));
  check(isDeepStrictEqual(entries.filter(entry => entry.startsWith("package/dist/native/")).sort(), [...nativeEntries].sort()),
    `${path.basename(file)} must hold exactly the ${NATIVE_PLATFORMS.join(" and ")} native pairs under dist/native`);
  for (const required of ["package/package.json", "package/dist/index.js"]) check(entries.includes(required), `${path.basename(file)} has no ${required}`);
  const temporary = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-release-tarball-"));
  try {
    execFileSync("tar", ["-xzf", file, "-C", temporary, "--no-same-owner", "package/package.json", ...nativeEntries]);
    const packageJson = path.join(temporary, "package/package.json");
    check((await fsp.lstat(packageJson)).isFile() && await fsp.readFile(packageJson, "utf8") === ctx.packageJson,
      `package.json in ${path.basename(file)} differs from ${ctx.revision}`);
    const root = path.join(temporary, "prebuilt");
    await fsp.mkdir(root);
    for (const platform of NATIVE_PLATFORMS) {
      await fsp.rename(path.join(temporary, "package/dist/native", platform), path.join(root, `rudder-native-${platform}`));
    }
    const verified = await verifyNativeArtifacts({ root, expectedRevision: ctx.revision });
    for (const { platform, manifest } of verified.artifacts) {
      check(isDeepStrictEqual(manifest, nativeManifests[platform]), `The ${platform} pair in ${path.basename(file)} is not the one staged for ${ctx.revision}`);
    }
  } finally { await fsp.rm(temporary, { recursive: true, force: true }); }
  return describe(file);
}

async function draft() {
  const ctx = ciContext("branch");
  checkVersionFiles(process.cwd(), ctx.version);
  const api = github();
  check(!(await api.ref(`tags/${ctx.tag}`)), `${ctx.tag} is tagged already: stage the next version`);
  const existing = await releasesFor(api, ctx);
  if (!existing) {
    const created = await api.json("POST", `/repos/${REPO}/releases`, {
      tag_name: ctx.tag, target_commitish: ctx.revision, draft: true, name: `Rudder ${ctx.version} (staged)`,
      body: `Files staged for the ${ctx.tag} tag by ${ctx.branch} and the home. The ${ctx.tag} Release run publishes npm and the Mac release from them, then deletes this draft. Never publish it: the latest release must stay the Mac release with appcast.xml.`,
    });
    log(`created the draft release ${ctx.tag} (${created.id}) for ${ctx.revision}`);
  } else if (existing.target_commitish !== ctx.revision) {
    await api.json("PATCH", `/repos/${REPO}/releases/${existing.id}`, { target_commitish: ctx.revision });
    log(`moved the draft release ${ctx.tag} (${existing.id}) from ${existing.target_commitish} to ${ctx.revision}`);
  } else {
    log(`the draft release ${ctx.tag} (${existing.id}) stages ${ctx.revision}`);
  }
}

async function putDarwin() {
  const ctx = ciContext("branch");
  const api = github();
  const release = await theDraft(api, ctx);
  const source = "native-dist/darwin-arm64";
  const native = await verifyNativeArtifact({ directory: source, platform: "darwin-arm64", expectedRevision: ctx.revision });
  const dir = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-release-darwin-"));
  try {
    execFileSync("tar", ["-czf", path.join(dir, "rudder-native-darwin-arm64.tgz"), "-C", source, ...NATIVE_FILES]);
    await putPart(api, release, ctx, "darwin", dir, { native });
  } finally { await fsp.rm(dir, { recursive: true, force: true }); }
}

function releaseEnv(file) {
  return Object.fromEntries(fs.readFileSync(file, "utf8").split("\n").filter(Boolean).map(line => line.split(/=(.*)/s).slice(0, 2)));
}

async function putMac(dir) {
  check(dir, "usage: release.mjs put-mac <dir with Rudder.dmg, appcast.xml and release.env>");
  const ctx = ciContext("branch");
  const env = releaseEnv(path.join(dir, "release.env"));
  check(env.VERSION === ctx.version && env.BUILD === macBuild(ctx.version), `release.env is ${env.VERSION} (${env.BUILD}), not ${ctx.version} (${macBuild(ctx.version)})`);
  const api = github();
  await putPart(api, await theDraft(api, ctx), ctx, "mac", dir, { build: env.BUILD });
}

async function stage() {
  check(process.platform === "linux" && process.arch === "x64", "Stage on the linux-x64 home: it builds the linux-x64 pair");
  const ctx = checkout();
  const dirty = git(["status", "--porcelain=v1", "--untracked-files=all"]).trim();
  check(!dirty, `Stage from a clean clone of ${ctx.branch}; this checkout has changes:\n${dirty}`);
  checkVersionFiles(process.cwd(), ctx.version);
  const api = github();
  check(!(await api.ref(`tags/${ctx.tag}`)), `${ctx.tag} is tagged already: stage the next version`);
  const pushed = await api.ref(`heads/${ctx.branch}`);
  check(pushed === ctx.revision, `${ctx.branch} on GitHub is ${pushed ?? "missing"}, not this checkout's ${ctx.revision}: push it first`);

  let release = await theDraft(api, ctx);
  const existingNpm = await readPart(api, release, ctx, "npm");
  const tarball = path.resolve(STAGE, tarballName(ctx.version));
  const localNpmPath = path.resolve(STAGE, "npm.json");
  let native;
  let verified;
  if (
    existingNpm?.revision === ctx.revision
    && fs.existsSync(tarball)
    && fs.existsSync(localNpmPath)
    && isDeepStrictEqual(JSON.parse(fs.readFileSync(localNpmPath, "utf8")), existingNpm)
  ) {
    native = existingNpm.native;
    verified = await verifyTarball(tarball, ctx, native);
    check(verified.integrity === existingNpm.integrity, `Local ${tarballName(ctx.version)} hashes to ${verified.integrity}, draft npm.json says ${existingNpm.integrity}`);
    log(`reusing already-staged ${tarballName(ctx.version)} for ${ctx.revision}`);
  } else {
    await fsp.rm(STAGE, { recursive: true, force: true });
    const prebuilt = path.resolve(WORK, "prebuilt");
    await fsp.mkdir(prebuilt, { recursive: true });
    const env = { ...process.env };
    delete env.RUDDER_NATIVE_PREBUILTS;
    delete env.RUDDER_NATIVE_SOURCE_REVISION;

    log("building the linux-x64 pair");
    run("nice", ["-n", "10", process.execPath, "scripts/build-native.mjs"], { env });
    const target = path.resolve(env.CARGO_TARGET_DIR || "target", "release");
    const linux = path.join(prebuilt, "rudder-native-linux-x64");
    await fsp.mkdir(linux);
    for (const name of NATIVE_BINARIES) await fsp.copyFile(path.join(target, name), path.join(linux, name));
    const linuxManifest = await createNativeManifest({ directory: linux, platform: "linux-x64", expectedRevision: ctx.revision });

    release = await waitForPart(api, ctx, "darwin", "the darwin-arm64 pair from Offstage");
    const darwinPart = await fetchPart(api, release, ctx, "darwin", path.resolve(WORK, "darwin"));
    const darwin = path.join(prebuilt, "rudder-native-darwin-arm64");
    await extractNative(path.resolve(WORK, "darwin", "rudder-native-darwin-arm64.tgz"), darwin);
    const darwinManifest = await verifyNativeArtifact({ directory: darwin, platform: "darwin-arm64", expectedRevision: ctx.revision });
    check(isDeepStrictEqual(darwinManifest, darwinPart.native), "The darwin-arm64 pair differs from its darwin.json");
    native = { "darwin-arm64": darwinManifest, "linux-x64": linuxManifest };

    log("packing the npm tarball");
    const packEnv = { ...env, RUDDER_NATIVE_PREBUILTS: prebuilt, RUDDER_NATIVE_SOURCE_REVISION: ctx.revision };
    run("npm", ["run", "build"], { env: packEnv });
    const packed = JSON.parse(execFileSync("npm", ["pack", "--ignore-scripts", "--json", "--pack-destination", STAGE], { encoding: "utf8", env: packEnv }));
    check(packed.length === 1 && packed[0].filename === tarballName(ctx.version), `npm pack made ${packed.map(p => p.filename).join(", ")}, not ${tarballName(ctx.version)}`);
    verified = await verifyTarball(tarball, ctx, native);
    check(verified.integrity === packed[0].integrity, `npm pack reported ${packed[0].integrity}, the tarball hashes to ${verified.integrity}`);
    release = await theDraft(api, ctx);
    await putPart(api, release, ctx, "npm", STAGE, { integrity: verified.integrity, native });
  }

  release = await waitForPart(api, ctx, "mac", "the signed Mac app from Offstage");
  const macDir = path.resolve(WORK, "mac");
  const macPart = await fetchPart(api, release, ctx, "mac", macDir);
  run("bash", ["mac/scripts/prepared-release.sh", "verify", macDir]);

  const pair = platform => NATIVE_BINARIES.map(name => `${name} ${native[platform].files[name].sha256}`).join(", ");
  console.log(`
Staged Rudder ${ctx.version} from ${ctx.revision} in the draft release ${ctx.tag}.
The ${ctx.tag} tag at that commit publishes exactly these files:
  npm    ${PACKAGE}@${ctx.version}: ${tarballName(ctx.version)}, ${mb(verified.size)}, ${verified.integrity}
           darwin-arm64 (Offstage): ${pair("darwin-arm64")}
           linux-x64 (this home):   ${pair("linux-x64")}
  Mac    mac-v${ctx.version}-${macPart.build}: Rudder.dmg ${mb(macPart.files["Rudder.dmg"].size)} sha256 ${macPart.files["Rudder.dmg"].sha256},
           appcast.xml signed and verified against mac/App/Info.plist; published once Cloud runs
           ${WORKER_IMAGE_REPOSITORY}:${ctx.version}
The worker image builds from ${path.join(STAGE, tarballName(ctx.version))}: node scripts/worker-image.mjs ${ctx.version}`);
}

async function publishNpm() {
  const ctx = ciContext("tag");
  const api = github();
  const release = await theDraft(api, ctx);
  const darwin = await readPart(api, release, ctx, "darwin");
  check(darwin?.revision === ctx.revision, `${ctx.tag} has no darwin.json for ${ctx.revision}`);
  const dir = path.resolve(WORK, "publish");
  const part = await fetchPart(api, release, ctx, "npm", dir);
  check(isDeepStrictEqual(part.native["darwin-arm64"], darwin.native), "The npm tarball's darwin-arm64 pair is not the one Offstage built for this commit");
  const tarball = path.join(dir, tarballName(ctx.version));
  const verified = await verifyTarball(tarball, ctx, part.native);
  check(verified.integrity === part.integrity, `${tarballName(ctx.version)} hashes to ${verified.integrity}, npm.json says ${part.integrity}`);
  // A re-run after a lost response must not publish twice, or anything else.
  const registry = await fetch(`https://registry.npmjs.org/${PACKAGE.replace("/", "%2f")}/${ctx.version}`, { signal: AbortSignal.timeout(30_000) });
  if (registry.status === 200) {
    const { dist } = await registry.json();
    check(dist?.integrity === verified.integrity, `${PACKAGE}@${ctx.version} is on npm with ${dist?.integrity}, not the staged ${verified.integrity}`);
    log(`${PACKAGE}@${ctx.version} is on npm already, with the staged tarball's integrity`);
    return;
  }
  check(registry.status === 404, `The npm registry answered ${registry.status} for ${PACKAGE}@${ctx.version}`);
  run("npm", ["publish", tarball, "--provenance", "--access", "public", "--tag", "latest", "--ignore-scripts"]);
}

async function fetchMac(dir) {
  check(dir, "usage: release.mjs fetch-mac <dir>");
  const ctx = ciContext("tag");
  const api = github();
  await fetchPart(api, await theDraft(api, ctx), ctx, "mac", path.resolve(dir));
}

async function finish() {
  const ctx = ciContext("tag");
  const api = github();
  const release = await releasesFor(api, ctx);
  if (release) {
    check(release.target_commitish === ctx.revision, `The draft release ${ctx.tag} stages ${release.target_commitish}, not ${ctx.revision}`);
    await api.request("DELETE", `/repos/${REPO}/releases/${release.id}`, { ok: [204] });
    log(`deleted the draft release ${ctx.tag} (${release.id})`);
  }
  const branch = await api.ref(`heads/${ctx.branch}`);
  if (branch === null) return;
  check(branch === ctx.revision, `${ctx.branch} moved to ${branch} after ${ctx.revision} was tagged; delete it by hand`);
  await api.request("DELETE", `/repos/${REPO}/git/refs/heads/${ctx.branch}`, { ok: [204] });
  log(`deleted ${ctx.branch}`);
}

export const COMMANDS = { draft, "put-darwin": putDarwin, "put-mac": putMac, stage, "publish-npm": publishNpm, "fetch-mac": fetchMac, finish };

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [command, ...args] = process.argv.slice(2);
  const action = COMMANDS[command];
  if (!action) {
    console.error(`usage: node scripts/release.mjs ${Object.keys(COMMANDS).join("|")}`);
    process.exit(64);
  }
  action(...args).catch(error => {
    console.error(`release ${command}: ${error instanceof ReleaseError ? error.message : error.stack}`);
    process.exitCode = 1;
  });
}
