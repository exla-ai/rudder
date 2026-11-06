#!/usr/bin/env node
// Build and push the Rudder Cloud worker image for a published release.
//
//   node scripts/worker-image.mjs 4.2.0                       # from the npm release
//   node scripts/worker-image.mjs 4.2.0 --compile             # compile from this checkout
//   node scripts/worker-image.mjs 4.2.0 --label trial-4.2.0   # push under another tag
//
// The npm package already carries dist and the linux-x64 rudder-native and
// rudder-jj that the Release workflow built, so by default the builder fetches
// it (RUDDER_SOURCE=release) and skips the Rust stage. Fly's own builder pushes
// straight to registry.fly.io; Depot once reported a push that never arrived,
// so the image only counts once the registry serves its manifest. It is then
// copied, blob mounts only, to registry.fly.io/rudder-worker-images, the
// image-only app external home hosts pull from (cloud/src/home-host.ts).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync, spawnSync } from "node:child_process";

const APP = process.env.RUDDER_WORKER_APP || "rudder-workers-exla";
const EXTERNAL_APP = process.env.RUDDER_EXTERNAL_WORKER_APP || "rudder-worker-images";
const PACKAGE = "@exla/rudder";

const args = process.argv.slice(2);
const compile = args.includes("--compile");
const labelAt = args.indexOf("--label");
const version = args.find((arg, i) => !arg.startsWith("-") && args[i - 1] !== "--label");
const label = labelAt >= 0 ? args[labelAt + 1] : version;
if (!label || !/^[0-9A-Za-z][0-9A-Za-z._-]{0,127}$/.test(label) || !version || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error("Usage: node scripts/worker-image.mjs <version> [--compile] [--label <tag>]");
  process.exit(64);
}
process.chdir(path.resolve(path.dirname(new URL(import.meta.url).pathname), ".."));

function flyToken() {
  if (process.env.FLY_API_TOKEN) return process.env.FLY_API_TOKEN;
  // flyctl refreshes its own login; ask it rather than reading ~/.fly/config.yml,
  // whose token can lack the discharge the registry and builders require.
  return execFileSync("flyctl", ["auth", "token"], { encoding: "utf8" }).trim().split("\n").pop();
}

const MANIFEST_TYPES = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json";

// Same registry, so every blob is mounted from the source repository and only
// the manifest is uploaded. Fly's builder pushes one plain image manifest.
async function copyTag(fromApp, toApp, tag, token) {
  const auth = `Basic ${Buffer.from(`x:${token}`).toString("base64")}`;
  const base = "https://registry.fly.io/v2";
  const source = await fetch(`${base}/${fromApp}/manifests/${tag}`, { headers: { authorization: auth, accept: MANIFEST_TYPES } });
  if (!source.ok) throw new Error(`reading ${fromApp}:${tag} returned ${source.status}`);
  const type = source.headers.get("content-type") || "application/vnd.docker.distribution.manifest.v2+json";
  const body = Buffer.from(await source.arrayBuffer());
  const manifest = JSON.parse(body.toString("utf8"));
  if (!manifest.config || !Array.isArray(manifest.layers)) throw new Error(`${fromApp}:${tag} is not a single image manifest (${type})`);
  for (const { digest } of [manifest.config, ...manifest.layers]) {
    const mount = await fetch(`${base}/${toApp}/blobs/uploads/?mount=${encodeURIComponent(digest)}&from=${fromApp}`, { method: "POST", headers: { authorization: auth } });
    if (mount.status !== 201) throw new Error(`mounting ${digest} into ${toApp} returned ${mount.status}`);
  }
  const put = await fetch(`${base}/${toApp}/manifests/${tag}`, { method: "PUT", headers: { authorization: auth, "content-type": type }, body });
  if (!put.ok) throw new Error(`writing ${toApp}:${tag} returned ${put.status}`);
}

async function manifestServed(image, token) {
  const [repo, tag] = image.replace(/^registry\.fly\.io\//, "").split(":");
  const response = await fetch(`https://registry.fly.io/v2/${repo}/manifests/${tag}`, {
    method: "HEAD",
    headers: {
      authorization: `Basic ${Buffer.from(`x:${token}`).toString("base64")}`,
      accept: "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json",
    },
  });
  return response.ok;
}

const started = Date.now();
const image = `registry.fly.io/${APP}:${label}`;
const config = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "rudder-worker-image-")), "fly.toml");
fs.writeFileSync(config, `app = "${APP}"\n`);
try {
  if (compile) {
    if (!fs.existsSync("dist/index.js")) throw new Error("--compile copies ./dist; run `npm run build` first");
  } else {
    const published = execFileSync("npm", ["view", `${PACKAGE}@${version}`, "version"], { encoding: "utf8" }).trim();
    if (published !== version) throw new Error(`${PACKAGE}@${version} is not on npm yet; wait for the Release workflow`);
  }
  const build = spawnSync("flyctl", [
    "deploy", "--config", config, "--dockerfile", "cloud/worker/Dockerfile",
    "--build-only", "--push", "--remote-only", "--depot=false", "--image-label", label,
    ...(compile ? [] : ["--build-arg", "RUDDER_SOURCE=release", "--build-arg", `RUDDER_VERSION=${version}`]),
    ".",
  ], { stdio: "inherit" });
  if (build.status !== 0) throw new Error(`flyctl build failed (exit ${build.status})`);
  const token = flyToken();
  for (let i = 0; !(await manifestServed(image, token)); i += 1) {
    // registry.fly.io has taken about 6 minutes to serve a new tag after the push.
    if (i >= 180) throw new Error(`flyctl finished but the registry does not serve ${image}`);
    await new Promise((resolve) => setTimeout(resolve, 5000));
  }
  console.log(`${image} is in the registry (${Math.round((Date.now() - started) / 1000)}s).`);
  await copyTag(APP, EXTERNAL_APP, label, token);
  console.log(`Copied to registry.fly.io/${EXTERNAL_APP}:${label} for external home hosts.`);
} catch (error) {
  console.error(`worker-image: ${error instanceof Error ? error.message : String(error)}`);
  process.exitCode = 1;
} finally {
  fs.rmSync(path.dirname(config), { recursive: true, force: true });
}
