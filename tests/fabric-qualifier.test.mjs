import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import { DIAGNOSTIC_LIMIT, qualifierSha256, runCompilerCommand } from "../examples/fabric/qualify-worker.mjs";

test("compiler failure diagnostics preserve the cause while redacting credentials and bounding UTF-8", async (t) => {
  const cwd = await fs.mkdtemp(path.join(os.tmpdir(), "qualifier-diagnostic-"));
  t.after(() => fs.rm(cwd, { recursive: true, force: true }));
  const secret = "rdrw_fixture-private-worker-token";
  const hidden = "unknown-bearer-credential";
  const stderr = `error: self-contained linker not found\n${secret}\nBearer ${hidden}\nhttps://user:password@example.invalid/tool?signature=signed-value\n${cwd}/private-runtime/file\n`;
  const source = `process.stdout.write('a'+'🙂'.repeat(5000));process.stderr.write(${JSON.stringify(stderr)}+'env_absent='+(process.env.RUDDER_WORKER_TOKEN===undefined));process.exit(23);`;
  await assert.rejects(runCompilerCommand(process.execPath, ["-e", source], {
    cwd, env: { PATH: "/usr/bin:/bin", HOME: cwd, LC_ALL: "C" },
    privateEnv: { RUDDER_WORKER_TOKEN: secret }, privatePaths: [cwd],
  }), (error) => {
    assert.equal(error.name, "CompilerQualificationError"); assert.equal(error.code, 23);
    assert.match(error.diagnostics.stderr.text, /self-contained linker not found/);
    assert.match(error.diagnostics.stderr.text, /env_absent=true/);
    const serialized = JSON.stringify(error);
    for (const value of [secret, hidden, "user:password", "signed-value", cwd]) assert(!serialized.includes(value), value);
    for (const stream of Object.values(error.diagnostics)) assert(Buffer.byteLength(stream.text, "utf8") <= DIAGNOSTIC_LIMIT);
    assert.equal(error.diagnostics.stdout.truncated, true);
    assert(!error.diagnostics.stdout.text.includes("\ufffd"), "truncation must preserve complete Unicode characters");
    assert(!error.message.includes(source), "raw execFile command text must not escape through Error.message");
    return true;
  });
});

test("excessive compiler output terminates with a bounded sanitized diagnostic", async () => {
  await assert.rejects(runCompilerCommand(process.execPath, ["-e", "process.stderr.write('x'.repeat(128*1024));setTimeout(()=>{},10000)"], {
    cwd: process.cwd(), env: { PATH: "/usr/bin:/bin" }, privateEnv: {},
  }), (error) => {
    assert.equal(error.code, "ERR_CHILD_PROCESS_STDIO_MAXBUFFER");
    assert.equal(error.diagnostics.stderr.truncated, true);
    assert(Buffer.byteLength(error.diagnostics.stderr.text) <= DIAGNOSTIC_LIMIT);
    return true;
  });
});

test("qualifier report identity hashes the exact public script bytes", async () => {
  const bytes = await fs.readFile(new URL("../examples/fabric/qualify-worker.mjs", import.meta.url));
  assert.equal(await qualifierSha256(), createHash("sha256").update(bytes).digest("hex"));
});
