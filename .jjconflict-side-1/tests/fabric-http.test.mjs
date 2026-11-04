import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs/promises";
import { createReadStream, constants } from "node:fs";
import os from "node:os";
import path from "node:path";
import { createHash, randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { FabricLocalClient } from "../dist/fabric-local.js";
import { FabricRemoteClient, runFabricTransferCommand } from "../dist/fabric-remote.js";
import { createFabricHttpHandler } from "../cloud/dist/fabric-http.js";
import { canonicalFabricMetadata } from "../cloud/dist/fabric-cas.js";

const repo = fileURLToPath(new URL("..", import.meta.url));
const sha = (bytes) => createHash("sha256").update(bytes).digest("hex");
const ref = (kind, bytes) => ({ kind, algorithm: "sha256", digest: sha(bytes), size_bytes: bytes.length });

// A file-backed fixture exercises the production streaming extension. It
// refuses a buffered large-object read, so tests cannot accidentally pass by
// allocating compiler-sized blobs in the HTTP service's object-storage path.
class FileStorage {
  maxChunk = 0; fileUploads = 0; tamperDigest; streamingReads = 0;
  constructor(root) { this.root = root; }
  file(key) { assert.match(key, /^fabric\/v1\/accounts\/[a-f0-9]{64}\/(?:objects\/sha256\/[a-f0-9]{64}|staging\/[a-f0-9-]+\.json|roots\/[A-Za-z0-9._-]+\.json)$/); return path.join(this.root, ...key.split("/")); }
  async get(key, maxBytes) {
    const file = this.file(key); const stat = await fs.stat(file).catch((error) => { if (error.code === "ENOENT") return null; throw error; });
    if (!stat) return null; assert.ok(stat.size <= maxBytes);
    assert.ok(stat.size <= 4 * 1024 * 1024, "large blobs must use streaming reads");
    const body = await fs.readFile(file); return { body, etag: sha(body) };
  }
  async putIfAbsent(key, bytes) {
    assert.ok(bytes.length <= 4 * 1024 * 1024, "large blobs must use private spool files");
    const file = this.file(key); await fs.mkdir(path.dirname(file), { recursive: true, mode: 0o700 });
    try { await fs.writeFile(file, bytes, { flag: "wx", mode: 0o600 }); return true; } catch (error) { if (error.code === "EEXIST") return false; throw error; }
  }
  async replaceIfMatch(key, etag, bytes) {
    const value = await this.get(key, 4096); if (!value || value.etag !== etag) return false;
    const temporary = this.file(key) + "." + randomUUID();
    await fs.writeFile(temporary, bytes, { mode: 0o600 }); await fs.rename(temporary, this.file(key)); return true;
  }
  async delete(key) { await fs.rm(this.file(key), { force: true }); }
  async putFileIfAbsent(key, source, reference) {
    assert.equal((await fs.stat(source)).size, reference.size_bytes);
    assert.equal((await fs.stat(path.dirname(source))).mode & 0o777, 0o700);
    const file = this.file(key); await fs.mkdir(path.dirname(file), { recursive: true, mode: 0o700 });
    try { await fs.copyFile(source, file, constants.COPYFILE_EXCL); this.fileUploads++; return true; } catch (error) { if (error.code === "EEXIST") return false; throw error; }
  }
  async readStream(key, maxBytes, consume) {
    const file = this.file(key); const stat = await fs.stat(file).catch((error) => { if (error.code === "ENOENT") return null; throw error; });
    if (!stat) return false; assert.ok(stat.size <= maxBytes); this.streamingReads++;
    const self = this;
    async function* chunks() {
      let first = true;
      for await (const original of createReadStream(file, { highWaterMark: 64 * 1024 })) {
        self.maxChunk = Math.max(self.maxChunk, original.length);
        const chunk = self.tamperDigest && key.endsWith(self.tamperDigest) && first ? Buffer.from(original) : original;
        if (self.tamperDigest && key.endsWith(self.tamperDigest) && first) chunk[0] ^= 1;
        first = false; yield chunk;
      }
    }
    await consume(chunks()); return true;
  }
}

async function fixture(t) {
  const binary = process.env.RUDDER_NATIVE_BIN || path.join(repo, "target/debug/rudder-native");
  if (!(await fs.stat(binary).catch(() => null))) { if (process.env.CI) assert.fail("fabric HTTP tests need the current native binary"); t.skip("build rudder-native or set RUDDER_NATIVE_BIN"); return; }
  const parent = path.join(os.homedir(), ".cache/rudder-fabric-tests"); await fs.mkdir(parent, { recursive: true, mode: 0o700 });
  const directory = await fs.mkdtemp(path.join(parent, "http-"));
  t.after(() => fs.rm(directory, { recursive: true, force: true }));
  const storage = new FileStorage(path.join(directory, "remote")); const requests = [];
  const tokens = new Map([["user-a", "account-a"], ["user-b", "account-b"]]);
  const workers = new Map([["workspace-a", { token: "worker-a", account: "account-a" }], ["workspace-b", { token: "worker-b", account: "account-b" }]]);
  const handler = createFabricHttpHandler({ storage, authenticate: (request) => {
    const bearer = request.headers.authorization?.replace(/^Bearer /, ""); const workspace = request.headers["x-rudder-workspace-id"];
    if (workspace !== undefined) { const worker = typeof workspace === "string" && workers.get(workspace); if (worker && worker.token === bearer) return worker.account; }
    else if (tokens.has(bearer)) return tokens.get(bearer);
    throw Object.assign(new Error("private authentication details"), { status: 401 });
  } });
  const server = http.createServer((request, response) => {
    const url = new URL(request.url, "http://127.0.0.1"); requests.push({ method: request.method, path: url.pathname });
    void handler(request, response, url);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(async () => { server.closeAllConnections(); await new Promise((resolve) => server.close(resolve)); });
  const url = `http://127.0.0.1:${server.address().port}`;
  const client = (token, workspace) => new FabricRemoteClient({ baseUrl: `${url}/api/rudder/fabric`, authHeaders: () => ({ Authorization: `Bearer ${token}`, ...(workspace ? { "X-Rudder-Workspace-Id": workspace } : {}) }) });
  const local = async (name) => { const home = path.join(directory, name); const connection = await FabricLocalClient.open({ home, nativeBinary: binary }); t.after(() => connection.close().catch(() => {})); return { home, connection }; };
  return { directory, storage, requests, url, client, local, binary };
}
async function source(f, name = "source", large = false) {
  const original = await f.local(name); const directory = path.join(f.directory, name + "-files"); await fs.mkdir(directory);
  await fs.writeFile(path.join(directory, "ø-hello.txt"), Buffer.from("hello\0\nø\xff", "utf8"));
  if (large) {
    const file = await fs.open(path.join(directory, "large-tool"), "wx", 0o755);
    const chunk = Buffer.alloc(1024 * 1024, 0x5a);
    try { for (let i = 0; i < 70; i++) await file.write(chunk); } finally { await file.close(); }
  }
  const { root: sourceRoot } = await original.connection.request({ operation: "import_source", path: directory, writers_quiesced: true, pin: "caller-source" });
  // Preserve a u64 cursor above JavaScript's precise-integer range as raw
  // canonical metadata throughout native→HTTP→native transfer.
  const bytes = canonicalFabricMetadata({ format_version: 1, source: sourceRoot, environment: null, parents: [], consistency: "quiesced", durability: "local", knowledge_cursor: (1n << 63n) + 7n });
  const root = ref("checkpoint", bytes); const stage = await original.connection.beginStaging(30_000);
  await original.connection.receive(stage.lease_id, root, bytes);
  await original.connection.publish(stage.lease_id, "caller-checkpoint", root);
  return { ...original, root, bytes, sourceRoot };
}

test("actual HTTP handler transfers a pinned native checkpoint into a fresh independent native CAS", async (t) => {
  const f = await fixture(t); if (!f) return; const original = await source(f); const remote = f.client("user-a");
  const uploaded = await remote.uploadRoot("checkpoint", original.root, original.connection);
  assert.equal(uploaded.receipt.durability, "replicated"); assert.equal(uploaded.uploaded_objects, 4);
  const pins = (await original.connection.request({ operation: "retention" })).pins;
  assert.deepEqual(Object.keys(pins).sort(), ["caller-checkpoint", "caller-source"]);
  assert.equal((await remote.uploadRoot("checkpoint", original.root, original.connection)).uploaded_objects, 0);
  const target = await f.local("independent-target");
  const downloaded = await remote.downloadRoot("checkpoint", target.connection, "retained-download");
  assert.equal(downloaded.local.durability, "local"); assert.deepEqual(downloaded.local.root, original.root);
  assert.equal(downloaded.downloaded_objects, uploaded.receipt.object_count);
  assert.deepEqual((await target.connection.request({ operation: "retention" })).staging, {});
  const pin = await target.connection.pin(original.root); await target.connection.closure(original.root);
  assert.deepEqual(await target.connection.read(original.root), original.bytes);
  await target.connection.unpin(pin);
  await original.connection.close(); await fs.rm(path.join(original.home, "fabric/cas"), { recursive: true });
  const sourceMetadata = await target.connection.request({ operation: "read", object: original.sourceRoot });
  const exported = path.join(f.directory, "restored");
  await target.connection.request({ operation: "export_tree", directory: sourceMetadata.metadata.directory, destination: exported });
  assert.equal(await fs.readFile(path.join(exported, "ø-hello.txt"), "utf8"), "hello\0\nø\xff");
  assert.ok(f.requests.some((request) => request.path.endsWith("/missing")));
});

test("actual native-to-HTTP-to-native transfer preserves chunked File and paged Directory identities", { timeout: 60_000 }, async (t) => {
  const f = await fixture(t); if (!f) return;
  const original = await f.local("chunked-source"), target = await f.local("chunked-target");
  const describe = (kind, value) => { const bytes = canonicalFabricMetadata(value); return { bytes, object: ref(kind, bytes) }; };
  const firstBytes = Buffer.alloc(4 * 1024 * 1024, 0x5a), lastBytes = Buffer.from("last verified bytes");
  const first = { bytes: firstBytes, object: ref("blob", firstBytes) }, last = { bytes: lastBytes, object: ref("blob", lastBytes) };
  const logical_size = first.bytes.length + last.bytes.length;
  const file = describe("file", { format_version: 1, layout: "fixed_4mib_fanout256_v1", level: 0, logical_size, children: [first.object, last.object] });
  const entry = { name_hex: "ff", node: { type: "file", object: file.object, logical_size, mode: 0o755 } };
  const shard = describe("directory_shard", { format_version: 1, prefix_hex: "ff",
    summary: { entry_count: 1, subdirectory_count: 0, entries_bytes: canonicalFabricMetadata(entry).length }, node: { type: "leaf", entries: [entry] } });
  const directory = describe("directory", { format_version: 2, mode: 0o755, entry_count: 1, subdirectory_count: 0, directory_shard: shard.object });
  const source = describe("source_root", { format_version: 1, directory: directory.object, parents: [], consistency: "quiesced" });
  const all = [source, directory, shard, file, first, last];
  const stage = await original.connection.beginStaging(30_000);
  for (const item of all) await original.connection.receive(stage.lease_id, item.object, item.bytes);
  await original.connection.publish(stage.lease_id, "chunked-source", source.object);
  const remote = f.client("worker-a", "workspace-a");
  const uploaded = await remote.uploadRoot("chunked-source", source.object, original.connection);
  assert.equal(uploaded.uploaded_objects, all.length);
  assert.equal(uploaded.receipt.object_count, all.length);
  const downloaded = await remote.downloadRoot("chunked-source", target.connection, "restored-chunked-source");
  assert.equal(downloaded.downloaded_objects, all.length);
  assert.deepEqual(downloaded.local.root, source.object);
  assert.deepEqual((await target.connection.request({ operation: "retention" })).staging, {});
  const pin = await target.connection.pin(source.object);
  try {
    const closure = await target.connection.closure(source.object);
    assert.equal(closure.length, all.length);
    for (const item of all) assert.deepEqual(await target.connection.read(item.object), item.bytes);
  } finally { await target.connection.unpin(pin); }
  assert.equal((await remote.uploadRoot("chunked-source", source.object, original.connection)).uploaded_objects, 0);
});

test("ancestry-free RunnableRoot round trip uses actual Rust and Cloud validation with a full u64 cursor", async (t) => {
  const f = await fixture(t); if (!f) return;
  const original = await f.local("runnable-source"), target = await f.local("runnable-target");
  const describe = (kind, value) => { const bytes = canonicalFabricMetadata(value); return { bytes, object: ref(kind, bytes) }; };
  const raw = (value) => { const bytes = Buffer.from(value); return { bytes, object: ref("blob", bytes) }; };
  const shard = describe("directory_shard", { format_version: 1, prefix_hex: "", summary: { entry_count: 0, subdirectory_count: 0, entries_bytes: 0 }, node: { type: "leaf", entries: [] } });
  const directory = describe("directory", { format_version: 2, mode: 493, entry_count: 0, subdirectory_count: 0, directory_shard: shard.object });
  const policy = raw('{"source":"all"}'), profile = raw('{"profile":"test"}'), hardlinks = raw('{"format_version":1,"groups":[]}');
  const source = describe("source_root", { format_version: 2, consistency: "quiesced", directory: directory.object, projection_policy: policy.object });
  const environment = describe("environment_root", { format_version: 2, directory: directory.object, platform: { os: "linux" }, installer_inputs: [policy.object], provenance: {} });
  const root = describe("runnable_root", { format_version: 1, consistency: "quiesced", source: source.object, environment: environment.object, view_root: directory.object, profile: profile.object, hardlinks: hardlinks.object, knowledge_cursor: (1n << 64n) - 1n });
  const all = [root, source, environment, directory, shard, policy, profile, hardlinks];
  const step = async (name, work) => { try { return await work(); } catch (error) { error.message = `${name}: ${error.message}`; throw error; } };
  const stage = await original.connection.beginStaging(30_000);
  for (const item of all) await step(`receive ${item.object.kind}`, () => original.connection.receive(stage.lease_id, item.object, item.bytes));
  await step("source publication", () => original.connection.publish(stage.lease_id, "runnable-source", root.object));
  const remote = f.client("user-a");
  const uploaded = await step("remote upload", () => remote.uploadRoot("runnable", root.object, original.connection));
  assert.equal(uploaded.receipt.object_count, all.length);
  const restored = await step("independent download", () => remote.downloadRoot("runnable", target.connection, "runnable-restored"));
  assert.equal(restored.downloaded_objects, all.length);
  const pin = await target.connection.pin(root.object);
  try {
    assert.equal((await target.connection.closure(root.object)).length, all.length);
    for (const item of all) assert.deepEqual(await target.connection.read(item.object), item.bytes);
  }
  finally { await target.connection.unpin(pin); }
  assert.match((await remote.getObject(root.object)).toString(), /"knowledge_cursor":18446744073709551615/);
  assert.equal(restored.local.durability, "local");
  assert.equal(restored.receipt.durability, "replicated");
  assert(!root.bytes.toString().includes('"durability"'), "publication levels must not rewrite a runnable identity");
});

test("actual auth callback binds user and worker credentials to accounts; caller fields cannot select another account", async (t) => {
  const f = await fixture(t); if (!f) return; const original = await source(f);
  await f.client("worker-a", "workspace-a").uploadRoot("private", original.root, original.connection);
  assert.deepEqual((await f.client("user-a").getRoot("private")).root, original.root);
  for (const remote of [f.client("user-b"), f.client("worker-b", "workspace-b")]) await assert.rejects(remote.getRoot("private"), { status: 404 });
  for (const remote of [f.client("user-a", "workspace-b"), f.client("worker-a", "workspace-b"), f.client("worker-a")]) await assert.rejects(remote.getRoot("private"), (error) => error.status === 401 && !error.message.includes("private authentication"));
  const response = await fetch(`${f.url}/api/rudder/fabric/missing`, { method: "POST", headers: { Authorization: "Bearer user-b", "Content-Type": "application/json" }, body: JSON.stringify({ format_version: 1, objects: [original.root], account_id: "account-a" }) });
  assert.equal(response.status, 400);
  const wrong = await fetch(`${f.url}/api/rudder/fabric/roots/private?account_id=account-a`, { headers: { Authorization: "Bearer user-b" } }); assert.equal(wrong.status, 404);
});

test("real >64MiB blob uses bounded server streams and binary native frames; download survives source loss", { timeout: 120_000 }, async (t) => {
  const f = await fixture(t); if (!f) return; const original = await source(f, "large-source", true); const remote = f.client("user-a");
  const uploaded = await remote.uploadRoot("large", original.root, original.connection);
  assert.ok(uploaded.uploaded_bytes > 64 * 1024 * 1024); assert.ok(f.storage.fileUploads >= 2);
  const target = await f.local("large-target"); const result = await remote.downloadRoot("large", target.connection, "large-retained");
  assert.equal(result.downloaded_bytes, uploaded.receipt.total_bytes); assert.ok(f.storage.streamingReads >= 2); assert.ok(f.storage.maxChunk <= 64 * 1024);
  await original.connection.close(); await fs.rm(path.join(original.home, "fabric/cas"), { recursive: true });
  const pin = await target.connection.pin(original.root); const inventory = await target.connection.closure(original.root);
  assert.ok(inventory.some((object) => object.kind === "blob" && object.size_bytes === 70 * 1024 * 1024));
  await target.connection.unpin(pin);
});

test("corrupt remote stream fails download, abandons its staging and creates no local success pin", async (t) => {
  const f = await fixture(t); if (!f) return; const original = await source(f); const remote = f.client("user-a");
  await remote.uploadRoot("corrupt-download", original.root, original.connection);
  const pin = await original.connection.pin(original.root); const objects = await original.connection.closure(original.root); await original.connection.unpin(pin);
  f.storage.tamperDigest = objects.find((object) => object.kind === "blob").digest;
  const target = await f.local("corrupt-target");
  await assert.rejects(remote.downloadRoot("corrupt-download", target.connection, "must-not-exist"));
  const retained = await target.connection.request({ operation: "retention" }); assert.deepEqual(retained.pins, {}); assert.deepEqual(retained.staging, {});
});

test("an interrupted actual HTTP upload cannot store or publish its partial blob", async (t) => {
  const f = await fixture(t); if (!f) return;
  const remote = f.client("user-a"); const stage = await remote.beginStaging();
  const bytes = Buffer.alloc(1024 * 1024, 0x37); const object = ref("blob", bytes);
  await new Promise((resolve) => {
    const request = http.request(`${f.url}/api/rudder/fabric/objects/blob/${object.digest}/${object.size_bytes}`, { method: "PUT", headers: { Authorization: "Bearer user-a", "Content-Type": "application/octet-stream", "Content-Length": bytes.length, "X-Rudder-Staging-Lease": stage.lease_id } });
    request.on("error", () => resolve());
    request.on("socket", (socket) => socket.once("connect", () => request.write(bytes.subarray(0, 1024), () => setTimeout(() => request.destroy(new Error("fixture abort")), 20))));
  });
  assert.deepEqual(await remote.missing([object]), [object]);
  assert.equal(f.storage.fileUploads, 0);
  await assert.rejects(remote.publishRoot("aborted", stage.lease_id, object));
  await assert.rejects(remote.getRoot("aborted"), { status: 404 });
});

test("public transfer command supports saved user login and paired worker environment without token argv", async (t) => {
  const f = await fixture(t); if (!f) return; const original = await source(f); const login = path.join(f.directory, "login"); await fs.mkdir(login);
  await fs.writeFile(path.join(login, "cloud.json"), JSON.stringify({ version: 1, cloudUrl: f.url, token: "user-a" }), { mode: 0o600 });
  const output = [];
  await runFabricTransferCommand(["upload", "from-command", "--root", JSON.stringify(original.root), "--home", original.home, "--url", f.url], { nativeBinary: f.binary, env: { ...process.env, RUDDER_HOME: login, RUDDER_CLOUD_TOKEN: "", RUDDER_WORKER_TOKEN: "", RUDDER_WORKSPACE_ID: "" }, stdout: (line) => output.push(JSON.parse(line)) });
  assert.equal(output[0].receipt.durability, "replicated");
  const targetHome = path.join(f.directory, "cli-target");
  await runFabricTransferCommand(["download", "from-command", "--pin", "from-worker", "--home", targetHome, "--url", f.url], { nativeBinary: f.binary, env: { ...process.env, RUDDER_WORKER_TOKEN: "worker-a", RUDDER_WORKSPACE_ID: "workspace-a" }, stdout: (line) => output.push(JSON.parse(line)) });
  assert.equal(output[1].local.durability, "local"); assert.deepEqual(output[1].local.root, original.root);
});
