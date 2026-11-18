// Agents on a Cloud home have no root. If the worker image lacks Chromium's
// system libraries, every agent that screenshots a web app (Claude Code's
// /run, Playwright checks) spends minutes unpacking .debs, or gives up.
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import test from "node:test";

// What `ldd` reports missing for Playwright's chromium and headless shell on
// node:22-bookworm-slim, as Debian bookworm packages, plus fonts to render text.
const CHROMIUM_RUNTIME = [
  "libasound2", "libatk-bridge2.0-0", "libatk1.0-0", "libatspi2.0-0", "libcairo2", "libcups2",
  "libdbus-1-3", "libdrm2", "libgbm1", "libglib2.0-0", "libnspr4", "libnss3", "libpango-1.0-0",
  "libx11-6", "libxcb1", "libxcomposite1", "libxdamage1", "libxext6", "libxfixes3",
  "libxkbcommon0", "libxrandr2", "fontconfig", "fonts-liberation",
];

test("the worker image installs what headless Chromium needs to start", async () => {
  const dockerfile = await fs.readFile(new URL("../cloud/worker/Dockerfile", import.meta.url), "utf8");
  const stage = dockerfile.split(/^FROM \S+ AS worker-tools$/m)[1]?.split(/^FROM /m)[0];
  assert.ok(stage, "worker-tools stage not found");
  const installed = [...stage.replace(/\\\n\s*/g, " ").matchAll(/apt-get install -y --no-install-recommends ([^&]+)/g)]
    .flatMap((match) => match[1].trim().split(/\s+/));
  assert.deepEqual(CHROMIUM_RUNTIME.filter((pkg) => !installed.includes(pkg)), []);
});

// The image copies worker modules one by one. A module one of them imports but
// the image lacks stops the home at start (ERR_MODULE_NOT_FOUND).
test("the worker image copies every module its worker modules import", async () => {
  const dockerfile = await fs.readFile(new URL("../cloud/worker/Dockerfile", import.meta.url), "utf8");
  const copied = new Set([...dockerfile.matchAll(/^COPY cloud\/worker\/(\S+\.mjs) \.\/\1$/gm)].map((m) => m[1]));
  assert.ok(copied.has("home.mjs"), "no worker modules found in the Dockerfile");
  const missing = [];
  for (const name of copied) {
    const source = await fs.readFile(new URL(`../cloud/worker/${name}`, import.meta.url), "utf8");
    for (const [, imported] of source.matchAll(/(?:from|import\()\s*["']\.\/([^"']+\.mjs)["']/g)) {
      if (!copied.has(imported)) missing.push(`${name} imports ${imported}`);
    }
  }
  assert.deepEqual(missing, []);
});
