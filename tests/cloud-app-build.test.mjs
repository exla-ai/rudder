// The web app's build (cloud/scripts/build-app.mjs): the shell names only
// files the build made, an open fetches one script and one stylesheet, and
// the service worker precaches everything the shell can load.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const cloud = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "cloud");
const installed = fs.existsSync(path.join(cloud, "node_modules", "esbuild"));
const test = installed ? nodeTest : (name, fn) => nodeTest(name, { skip: "cloud dependencies not installed (npm --prefix cloud ci)" }, fn);

test("the shell loads one hashed script and stylesheet, and the worker precaches the build", async () => {
  const { buildShell } = await import(path.join(cloud, "scripts", "build-app.mjs"));
  const files = await buildShell();
  const shell = files.get("index.html").toString();

  const refs = [...shell.matchAll(/(?:href|src)="(\/app\/[^"]+)"/g)].map((m) => m[1]);
  for (const ref of refs) assert.ok(files.has(ref.slice("/app/".length)), `${ref} is in the build`);
  const scripts = refs.filter((ref) => ref.endsWith(".js"));
  const styles = refs.filter((ref) => ref.endsWith(".css"));
  // Lazy app modules (home-update.js) are bundled in: no shared chunk to preload.
  assert.equal(scripts.length, 1, `one script, got ${scripts}`);
  assert.equal(styles.length, 1, `one stylesheet, got ${styles}`);
  assert.match(scripts[0], /^\/app\/assets\/app-[A-Z0-9]+\.js$/);
  assert.doesNotMatch(shell, /theme\.js|modulepreload|<!--/);
  assert.match(shell, /window\.RudderTheme/, "theme.js is inline");

  // Only the vendored libraries load lazily, and nothing comes from a CDN.
  const chunks = [...files.keys()].filter((name) => /^assets\/.*\.js$/.test(name)).map((name) => name.replace(/-[A-Z0-9]+\.js$/, ""));
  assert.deepEqual(chunks.sort(), ["assets/app", "assets/highlight.min", "assets/marked.esm", "assets/purify.es"]);
  const entry = files.get(scripts[0].slice("/app/".length)).toString();
  assert.doesNotMatch(entry, /cdn\.jsdelivr|import\("\/app\/home-update\.js"\)/);
  assert.match(entry, /"\/app\/home-update\.js"\]=\{/);

  const sw = files.get("sw.js").toString();
  const precache = JSON.parse(/^const PRECACHE = (.+);$/m.exec(sw)[1]);
  const assets = [...files.keys()].filter((name) => name.startsWith("assets/") && !name.endsWith(".map"));
  assert.deepEqual(precache.filter((u) => u.startsWith("/app/assets/")).sort(), assets.map((name) => `/app/${name}`).sort());
  assert.ok(precache.includes("/app"));
  assert.match(sw, /^const BUILD = "[0-9a-f]{16}";$/m);
});
