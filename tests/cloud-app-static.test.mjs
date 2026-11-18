// Web app static files: encoding negotiation, precompressed sidecars, the
// compressed-body cache and per-encoding ETags (cloud/src/app-static.ts).
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import zlib from "node:zlib";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const builtFile = path.join(path.resolve(here, ".."), "cloud", "dist", "app-static.js");
const built = fs.existsSync(builtFile);
const test = built ? nodeTest : (name, fn) => nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const lib = built ? await import(builtFile) : null;

const TYPES = { ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".png": "image/png" };

function get(port, file, headers = {}) {
  return new Promise((resolve, reject) => {
    http.get({ port, path: `/${file}`, headers }, (res) => {
      const chunks = [];
      res.on("data", (chunk) => chunks.push(chunk));
      res.on("end", () => {
        const raw = Buffer.concat(chunks);
        const coding = res.headers["content-encoding"];
        const body = coding === "br" ? zlib.brotliDecompressSync(raw) : coding === "gzip" ? zlib.gunzipSync(raw) : raw;
        resolve({ status: res.statusCode, headers: res.headers, raw, text: body.toString() });
      });
    }).on("error", reject);
  });
}

test("app files negotiate br/gzip, prefer sidecars and revalidate per encoding", async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-app-static-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const big = "export const rows = [\n" + Array.from({ length: 400 }, (_, i) => `  { id: ${i}, name: "row ${i}" },\n`).join("") + "];\n";
  fs.writeFileSync(path.join(dir, "big.js"), big);
  fs.writeFileSync(path.join(dir, "tiny.css"), "body { margin: 0 }\n");
  fs.writeFileSync(path.join(dir, "pic.png"), Buffer.alloc(4096, 7));
  fs.writeFileSync(path.join(dir, "bundle.js"), big);
  fs.writeFileSync(path.join(dir, "bundle.js.br"), zlib.brotliCompressSync("from the br sidecar"));
  fs.writeFileSync(path.join(dir, "bundle.js.gz"), zlib.gzipSync("from the gz sidecar"));

  const server = http.createServer((req, res) => {
    const file = path.join(dir, decodeURIComponent(new URL(req.url, "http://x").pathname));
    lib.sendStaticFile(req, res, file, { "content-type": TYPES[path.extname(file)], "cache-control": "no-cache" }).catch((error) => {
      res.writeHead(500);
      res.end(String(error.message));
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => server.close());
  const { port } = server.address();

  const br = await get(port, "big.js", { "accept-encoding": "gzip, deflate, br" });
  assert.equal(br.headers["content-encoding"], "br");
  assert.equal(br.headers.vary, "Accept-Encoding");
  assert.equal(br.headers["cache-control"], "no-cache");
  assert.equal(br.text, big);
  assert.ok(br.raw.length < big.length / 4);
  const gz = await get(port, "big.js", { "accept-encoding": "br;q=0, gzip" });
  assert.equal(gz.headers["content-encoding"], "gzip");
  assert.equal(gz.text, big);
  const plain = await get(port, "big.js");
  assert.equal(plain.headers["content-encoding"], undefined);
  assert.equal(plain.headers.vary, "Accept-Encoding");
  assert.equal(plain.text, big);
  assert.equal(new Set([br.headers.etag, gz.headers.etag, plain.headers.etag]).size, 3);

  const fresh = await get(port, "big.js", { "accept-encoding": "br", "if-none-match": br.headers.etag });
  assert.equal(fresh.status, 304);
  assert.equal(fresh.raw.length, 0);
  assert.equal(fresh.headers.etag, br.headers.etag);
  const otherEncoding = await get(port, "big.js", { "accept-encoding": "gzip", "if-none-match": br.headers.etag });
  assert.equal(otherEncoding.status, 200);
  assert.equal(otherEncoding.headers["content-encoding"], "gzip");

  // A changed file is recompressed, not served from the cache.
  const changed = big.replace("row 1\"", "row one\"");
  fs.writeFileSync(path.join(dir, "big.js"), changed);
  fs.utimesSync(path.join(dir, "big.js"), new Date(), new Date(Date.now() + 5000));
  const again = await get(port, "big.js", { "accept-encoding": "br" });
  assert.equal(again.text, changed);
  assert.notEqual(again.headers.etag, br.headers.etag);

  for (const file of ["tiny.css", "pic.png"]) {
    const res = await get(port, file, { "accept-encoding": "br, gzip" });
    assert.equal(res.headers["content-encoding"], undefined, file);
    assert.equal(res.headers.vary, undefined, file);
  }

  assert.equal((await get(port, "bundle.js", { "accept-encoding": "gzip, br" })).text, "from the br sidecar");
  assert.equal((await get(port, "bundle.js", { "accept-encoding": "gzip" })).text, "from the gz sidecar");
  assert.equal((await get(port, "bundle.js", { "accept-encoding": "identity" })).text, big);

  // A sidecar older than its source is a broken build, not something to serve.
  fs.utimesSync(path.join(dir, "bundle.js"), new Date(), new Date(Date.now() + 5000));
  const stale = await get(port, "bundle.js", { "accept-encoding": "br" });
  assert.equal(stale.status, 500);
  assert.match(stale.text, /older than/);
});
