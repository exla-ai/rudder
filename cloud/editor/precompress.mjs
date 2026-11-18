// Writes name.br and name.gz next to each file given, at maximum quality, for
// the Cloud server to send instead of compressing on the fly.
import fs from "node:fs";
import zlib from "node:zlib";

const files = process.argv.slice(2);
if (files.length === 0) {
  throw new Error("usage: node editor/precompress.mjs <file>...");
}
for (const file of files) {
  const body = fs.readFileSync(file);
  const br = zlib.brotliCompressSync(body, {
    params: {
      [zlib.constants.BROTLI_PARAM_MODE]: zlib.constants.BROTLI_MODE_TEXT,
      [zlib.constants.BROTLI_PARAM_QUALITY]: zlib.constants.BROTLI_MAX_QUALITY,
      [zlib.constants.BROTLI_PARAM_SIZE_HINT]: body.length,
    },
  });
  const gz = zlib.gzipSync(body, { level: zlib.constants.Z_BEST_COMPRESSION });
  fs.writeFileSync(`${file}.br`, br);
  fs.writeFileSync(`${file}.gz`, gz);
  console.log(`${file}: ${body.length} bytes, br ${br.length}, gzip ${gz.length}`);
}
