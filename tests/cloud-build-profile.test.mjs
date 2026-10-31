import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { runCloudBuildProfile, parseCloudBuildResponse } from "../dist/cloud-build-profile.js";

const ref = (kind, digest = "a".repeat(64)) => ({ kind, algorithm: "sha256", digest, size_bytes: 4 });
const request = (root) => ({ format_version: 1, store: root, index: root, scratch: root, buck2: `${root}/buck2`,
  runnable: ref("runnable_root"), expected_projection_policy: ref("blob") });
const response = (req) => ({ format_version: 1, kind: "managed-build-data", runnable: req.runnable,
  profile: ref("blob"), workspace: `${req.scratch}/build-a123`, target_count: 1, outputs_verified: true, output_object_count: 1,
  build: { format_version: 1, run_receipt: ref("action_result"), frontend_graph_reused_actions: 0,
    stored: { key: ref("action_key"), outputs: ref("directory"), receipt: ref("action_result"), requires_frontend_validation: true },
    observation: { frontend: "buck2-rudder-linux-compile-v1", trace_id: "trace", executed_actions: 0, remote_cache_hits: 1,
      native_action_digests: [{ hash: "b".repeat(64), size_bytes: 12 }], event_log: ref("blob"), build_report: ref("blob") } } });

async function fixture(t, source) {
  const root = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), "rudder build caller ")));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const binary = path.join(root, "native fixture.cjs");
  await fs.writeFile(binary, `#!${process.execPath}\n${source}\n`, { mode: 0o755 });
  return { root, options: { nativeBinary: binary, timeoutMs: 3_000 } };
}

test("build client submits exactly the data helper command without provider credentials", async (t) => {
  const req = request("/placeholder");
  const { root, options } = await fixture(t, `
const fs = require('node:fs');
let raw=''; process.stdin.on('data',c=>raw+=c); process.stdin.on('end',()=>{
const req=JSON.parse(raw); fs.writeFileSync(req.scratch+'/received.json',JSON.stringify({args:process.argv.slice(2),req,keys:Object.keys(process.env)}));
const result=${JSON.stringify(response(req))}; result.runnable=req.runnable; result.workspace=req.scratch+'/build-a123'; console.log(JSON.stringify(result)); });`);
  const result = await runCloudBuildProfile(request(root), options);
  assert.equal(result.build.observation.remote_cache_hits, 1);
  const captured = JSON.parse(await fs.readFile(path.join(root, "received.json"), "utf8"));
  assert.deepEqual(captured.args, ["fabric", "build-profile"]);
  assert.deepEqual(captured.req, request(root));
  // macOS CoreFoundation adds __CF_USER_TEXT_ENCODING to every process; it is
  // not inherited from Rudder and carries no credentials.
  const keys = captured.keys.filter((key) => !(process.platform === "darwin" && key === "__CF_USER_TEXT_ENCODING"));
  assert.deepEqual(keys, ["PATH"]);
});

test("response requires current frontend evidence, exact input and verified outputs", () => {
  const req = request("/scratch");
  const mutate = [
    r => { r.outputs_verified = false; }, r => { r.runnable.digest = "c".repeat(64); },
    r => { delete r.runnable.algorithm; }, r => { r.build.stored.outputs.algorithm = "sha1"; },
    r => { r.workspace = "/other/build-a123"; }, r => { r.build.observation.remote_cache_hits = 0; },
    r => { r.build.observation.native_action_digests = []; }, r => { r.build.run_receipt.kind = "blob"; },
    r => { r.build.stored.requires_frontend_validation = false; },
  ];
  assert.equal(parseCloudBuildResponse(JSON.stringify(response(req)), req).target_count, 1);
  for (const change of mutate) { const r = structuredClone(response(req)); change(r); assert.throws(() => parseCloudBuildResponse(JSON.stringify(r), req)); }
});

test("helper failure and output overflow cannot return success", async (t) => {
  const { root, options } = await fixture(t, "process.stdin.resume(); process.stdin.on('end',()=>{console.error('rejected input'); process.exit(7);});");
  await assert.rejects(runCloudBuildProfile(request(root), options), /failed \(7\).*rejected input/);
  await fs.writeFile(options.nativeBinary, `#!${process.execPath}\nprocess.stdout.write('x'.repeat(300000)); setInterval(()=>{},1000);\n`, { mode: 0o755 });
  await assert.rejects(runCloudBuildProfile(request(root), options), /output exceeds/);
});

test("deadlines and cancellation fail honestly and terminate helper process groups", async (t) => {
  const { root, options } = await fixture(t, "process.stdin.resume(); setInterval(()=>{},1000);");
  await assert.rejects(runCloudBuildProfile(request(root), { ...options, timeoutMs: 40 }), /deadline exceeded.*reconciliation/);
  const controller = new AbortController();
  const pending = runCloudBuildProfile(request(root), { ...options, signal: controller.signal });
  setTimeout(() => controller.abort(), 40);
  await assert.rejects(pending, /cancelled.*reconciliation/);
});

test("cancelled helpers cannot leave an ordinary descendant running", { skip: process.platform === "win32" }, async (t) => {
  const { root, options } = await fixture(t, `
const fs=require('node:fs'), cp=require('node:child_process'); let raw='';
process.stdin.on('data',c=>raw+=c); process.stdin.on('end',()=>{
 const req=JSON.parse(raw); const child=cp.spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore'});
 fs.writeFileSync(req.scratch+'/descendant',String(child.pid)); setInterval(()=>{},1000);
});`);
  const controller = new AbortController();
  const pending = runCloudBuildProfile(request(root), { ...options, signal: controller.signal });
  const pidPath = path.join(root, "descendant");
  let pid;
  for (let i=0; i<100; i++) {
    const value = await fs.readFile(pidPath, "utf8").catch(() => "");
    if (value) { pid = Number(value); break; }
    await new Promise(resolve => setTimeout(resolve, 10));
  }
  assert.ok(pid, "helper should have spawned a descendant before cancellation");
  controller.abort();
  await assert.rejects(pending, /cancelled/);
  // Linux can retain a killed orphan as a zombie until its init process reaps
  // it. Either absence or zombie state proves it cannot keep building.
  for (let i=0; i<100; i++) {
    try { process.kill(pid, 0); } catch (error) { if (error.code === "ESRCH") return; throw error; }
    if (process.platform === "linux") {
      const stat = await fs.readFile(`/proc/${pid}/stat`, "utf8").catch(() => "");
      if (!stat || stat.slice(stat.lastIndexOf(")") + 2).startsWith("Z ")) return;
    }
    await new Promise(resolve => setTimeout(resolve, 10));
  }
  process.kill(pid, "SIGKILL");
  assert.fail("descendant remained running after helper cancellation");
});

test("invalid request paths and deadlines fail before launching a helper", async () => {
  const missingAlgorithm = request("/scratch");
  delete missingAlgorithm.runnable.algorithm;
  await assert.rejects(runCloudBuildProfile(missingAlgorithm), /typed SHA-256/);
  await assert.rejects(runCloudBuildProfile({ ...request("/scratch"), scratch: "relative" }), /absolute/);
  await assert.rejects(runCloudBuildProfile(request("/scratch"), { timeoutMs: Infinity }), /deadline must/);
});
