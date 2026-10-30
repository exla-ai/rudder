import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { FabricLocalClient } from "../dist/fabric-local.js";
import { canonicalFabricMetadata } from "../cloud/dist/fabric-cas.js";

const repo = fileURLToPath(new URL("..", import.meta.url));
const ref = (kind, bytes) => ({ kind, algorithm: "sha256", digest: createHash("sha256").update(bytes).digest("hex"), size_bytes: bytes.length });
async function fixture(t) {
  const binary = process.env.RUDDER_NATIVE_BIN || path.join(repo, "target/debug/rudder-native");
  if (!(await fs.stat(binary).catch(() => null))) { if (process.env.CI) assert.fail("fabric tests require the current native binary"); t.skip("build rudder-native or set RUDDER_NATIVE_BIN"); return; }
  const base = path.join(os.homedir(), ".cache/rudder-fabric-tests");
  await fs.mkdir(base, { recursive: true, mode: 0o700 });
  const directory = await fs.mkdtemp(path.join(base, "local-"));
  t.after(() => fs.rm(directory, { recursive: true, force: true }));
  const home = path.join(directory, "home");
  const local = await FabricLocalClient.open({ home, nativeBinary: binary });
  t.after(() => local.close().catch(() => {}));
  return { local, home, binary, directory };
}

test("persistent native binary transport retains only its own source pins and preserves raw bytes", async (t) => {
  const f = await fixture(t); if (!f) return;
  const source = path.join(f.directory, "source"); await fs.mkdir(source);
  const bytes = Buffer.concat([Buffer.from([0, 255, 10, 13]), Buffer.from("{\"ok\":false}\nø peer text", "utf8")]);
  await fs.writeFile(path.join(source, "ø-file"), bytes);
  const { root } = await f.local.request({ operation: "import_source", path: source, writers_quiesced: true, pin: "caller-base" });
  await assert.rejects(f.local.closure(root), { code: "local_request_failed" });
  const pin = await f.local.pin(root); assert.match(pin, /^transfer:rpc:/);
  const objects = await f.local.closure(root); assert.equal(objects.length, 3);
  const originalPid = f.local.pid;
  const values = await Promise.all(objects.map((object) => f.local.read(object)));
  assert.equal(f.local.pid, originalPid, "one helper handles every object");
  assert.ok(values.some((value) => value.equals(bytes)));
  await assert.rejects(f.local.unpin("caller-base"), { code: "invalid_pin" });
  const retained = await f.local.request({ operation: "retention" });
  assert.deepEqual(retained.pins["caller-base"], root); assert.deepEqual(retained.pins[pin], root);
  await f.local.close();
  const reopened = await FabricLocalClient.open({ home: f.home, nativeBinary: f.binary });
  try { const after = await reopened.request({ operation: "retention" }); assert.deepEqual(Object.keys(after.pins), ["caller-base"]); }
  finally { await reopened.close(); }
});

test("native receive checks live owned staging, typed schema and the entire closure before pinning", async (t) => {
  const f = await fixture(t); if (!f) return;
  const stage = await f.local.beginStaging(30_000);
  const missingBytes = Buffer.from("not received yet"); const missing = ref("blob", missingBytes);
  const directoryBytes = canonicalFabricMetadata({ entries: [{ name_hex: "61", node: { mode: 420, object: missing, type: "file" } }], format_version: 1, mode: 493 });
  const directory = ref("directory", directoryBytes);
  assert.deepEqual(await f.local.receive(stage.lease_id, directory, directoryBytes), [missing]);
  await assert.rejects(f.local.publish(stage.lease_id, "partial", directory), { code: "local_request_failed" });
  assert.deepEqual((await f.local.request({ operation: "retention" })).pins, {});
  const unsafeBytes = canonicalFabricMetadata({ entries: [{ name_hex: "2e2e", node: { target_hex: "61", type: "symlink" } }], format_version: 1, mode: 493 });
  await assert.rejects(f.local.receive(stage.lease_id, ref("directory", unsafeBytes), unsafeBytes), { code: "local_request_failed" });
  assert.equal((await f.local.request({ operation: "gc", grace_ms: 0 })).blocked_by_staging, true);
  await f.local.receive(stage.lease_id, missing, missingBytes);
  const receipt = await f.local.publish(stage.lease_id, "complete", directory);
  assert.equal(receipt.durability, "local"); assert.deepEqual(receipt.root, directory);
  assert.deepEqual((await f.local.request({ operation: "retention" })).staging, {});
  await assert.rejects(f.local.receive(stage.lease_id, missing, missingBytes), { code: "invalid_lease" });
});

test("EOF abandons only session staging and never drops explicit publication", async (t) => {
  const f = await fixture(t); if (!f) return;
  const stage = await f.local.beginStaging(30_000);
  const data = Buffer.from("unpublished"); await f.local.receive(stage.lease_id, ref("blob", data), data);
  await f.local.close();
  const next = await FabricLocalClient.open({ home: f.home, nativeBinary: f.binary });
  try {
    assert.deepEqual((await next.request({ operation: "retention" })).staging, {});
    const swept = await next.request({ operation: "gc", grace_ms: 0 }); assert.ok(swept.removed_objects >= 1);
  } finally { await next.close(); }
});

test("an expired native staging lease cannot write bytes or publish a root", async (t) => {
  const f = await fixture(t); if (!f) return;
  const stage = await f.local.beginStaging(1000);
  await new Promise((resolve) => setTimeout(resolve, 1050));
  const data = Buffer.from("not written after expiry"); const object = ref("blob", data);
  await assert.rejects(f.local.receive(stage.lease_id, object, data), { code: "local_request_failed" });
  await assert.rejects(f.local.request({ operation: "read", object }), { code: "local_request_failed" });
  await assert.rejects(f.local.publish(stage.lease_id, "expired", object), { code: "local_request_failed" });
  assert.deepEqual((await f.local.request({ operation: "retention" })).pins, {});
  await f.local.abandonStaging(stage.lease_id);
});

test("normal EOF cannot falsely confirm cleanup after local storage I/O fails", async (t) => {
  const f = await fixture(t); if (!f) return;
  const input = path.join(f.directory, "cleanup-source"); await fs.mkdir(input); await fs.writeFile(path.join(input, "file"), "retained");
  const { root } = await f.local.request({ operation: "import_source", path: input, writers_quiesced: true, pin: "caller-kept" });
  const owned = await f.local.pin(root);
  const moved = path.join(f.directory, "offline-cas"); await fs.rename(path.join(f.home, "fabric/cas"), moved);
  await assert.rejects(f.local.close(), { code: "local_cleanup_failed" });
  const state = JSON.parse(await fs.readFile(path.join(moved, "retention.json"), "utf8"));
  assert.deepEqual(state.state.pins["caller-kept"], root);
  assert.deepEqual(state.state.pins[owned], root, "unconfirmed cleanup stays conservative");
});

test("native malformed/oversized frames fail closed without reflecting the body", async (t) => {
  const f = await fixture(t); if (!f) return;
  for (const content of ["{private-test-only-invalid\n", "x".repeat(256 * 1024 + 1) + "\n",
    JSON.stringify({ format_version: 1, id: randomUUID(), request: { operation: "receive_binary", lease_id: randomUUID(), object: ref("blob", Buffer.from("abc")) }, binary_bytes: 3 }) + "\nab"]) {
    const child = spawn(f.binary, ["fabric", "rpc"], { env: { ...process.env, RUDDER_HOME: f.home }, stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "", stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk; }); child.stderr.on("data", (chunk) => { stderr += chunk; });
    const exit = new Promise((resolve, reject) => { child.once("error", reject); child.once("exit", resolve); });
    child.stdin.on("error", () => {}); child.stdin.end(content);
    await exit; const result = JSON.parse(stdout.trim()); assert.equal(result.ok, false);
    assert.ok(!stdout.includes("private-test-only")); assert.ok(!stderr.includes("private-test-only"));
  }
});
