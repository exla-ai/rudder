// Explicit disposable-worker qualification, never a normal worker entrypoint.
// Credentials come only from the worker's private environment, never argv/report.
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

const exec = promisify(execFile);
const [rootId, expectedFile, reportFile] = process.argv.slice(2);
assert(rootId && expectedFile && reportFile, "root ID, reference file and report file required");
const expected = JSON.parse(await fs.readFile(expectedFile, "utf8"));
const moduleRoot = process.env.RUDDER_FABRIC_MODULE_ROOT || "/opt/rudder/dist";
const { FabricLocalClient } = await import(pathToFileURL(path.join(moduleRoot, "fabric-local.js")));
const { FabricRemoteClient } = await import(pathToFileURL(path.join(moduleRoot, "fabric-remote.js")));
const cloud = new URL(process.env.RUDDER_CLOUD_URL || "https://rudder-cloud-control.fly.dev");
assert.equal(cloud.protocol, "https:");
assert(!cloud.username && !cloud.password && !cloud.search && !cloud.hash);
assert(process.env.RUDDER_WORKER_TOKEN && process.env.RUDDER_WORKSPACE_ID);
const report = { format_version: 1, kind: "independent-worker-prepared-restore", machine: process.env.FLY_MACHINE_ID, checks: [] };
let local, phase = "fresh-local-store";
try {
  assert.equal(process.getuid(), 1500, "qualification requires the worker user");
  report.uid = process.getuid();
  const home = await fs.mkdtemp("/workspace/fabric-qualification-");
  await fs.chmod(home, 0o700);
  local = await FabricLocalClient.open({ home, nativeBinary: path.join(moduleRoot, "native/rudder-native") });
  assert.deepEqual((await local.request({ operation: "retention" })).pins, {});
  report.checks.push("fresh independent native CAS has no retained roots");
  const remote = new FabricRemoteClient({
    baseUrl: new URL("/api/rudder/fabric", cloud).href,
    authHeaders: () => ({ Authorization: `Bearer ${process.env.RUDDER_WORKER_TOKEN}`, "X-Rudder-Workspace-Id": process.env.RUDDER_WORKSPACE_ID }),
  });
  phase = "authenticated-download";
  const started = Date.now();
  const result = await remote.downloadRoot(rootId, local, "qualified-prepared-environment");
  report.downloadMs = Date.now() - started;
  assert.deepEqual(result.receipt.root, expected);
  assert.deepEqual(result.local.root, expected);
  report.receipt = result.receipt;
  report.local = result.local;
  report.downloadedObjects = result.downloaded_objects;
  report.downloadedBytes = result.downloaded_bytes;
  report.checks.push("deployed worker authentication resolves the expected account-scoped root", "every reachable object verifies before durable local publication");
  phase = "materialize-restored-toolchain";
  const metadata = await local.request({ operation: "read", object: expected });
  assert.equal(metadata.metadata.directory.kind, "directory");
  const runtime = path.join(home, "runtime");
  const exportStarted = Date.now();
  await local.request({ operation: "export_tree", directory: metadata.metadata.directory, destination: runtime });
  report.exportMs = Date.now() - exportStarted;
  const marker = JSON.parse(await fs.readFile(path.join(runtime, ".prepared-complete.json"), "utf8"));
  const scratch = path.join(home, "compile");
  await fs.mkdir(scratch);
  const env = { PATH: "/usr/bin:/bin", HOME: home, TMPDIR: scratch, LC_ALL: "C" };
  const run = async (program, args) => (await exec(program, args, { cwd: scratch, env, timeout: 60_000, maxBuffer: 1024 * 1024 })).stdout.trim();
  const rustc = path.join(runtime, marker.compiler_paths.rust);
  const node = path.join(runtime, marker.compiler_paths.node);
  phase = "restored-compiler-versions";
  report.rustc = await run(rustc, ["--version"]);
  report.node = await run(node, ["--version"]);
  assert.equal(report.rustc, marker.rustc);
  assert.equal(report.node, marker.node);
  phase = "compile-rust";
  await fs.writeFile(path.join(scratch, "main.rs"), 'fn main() { println!("{}", 6 * 7); }\n');
  const compileStarted = Date.now();
  await run(rustc, ["--sysroot", path.join(runtime, "tools/rust"), "--edition", "2021", "main.rs", "-o", "rust-check"]);
  assert.equal(await run(path.join(scratch, "rust-check"), []), "42");
  report.rustCompileMs = Date.now() - compileStarted;
  phase = "compile-typescript";
  await fs.writeFile(path.join(scratch, "main.ts"), "const answer: number = 6 * 7; console.log(answer);\n");
  const tsStarted = Date.now();
  await run(node, [path.join(runtime, marker.compiler_paths.typescript), "--target", "ES2020", "--module", "commonjs", "--outDir", "out", "main.ts"]);
  assert.equal(await run(node, [path.join(scratch, "out/main.js")]), "42");
  report.typescriptCompileMs = Date.now() - tsStarted;
  report.checks.push("restored pinned Rust compiler builds and runs a real program", "restored Node and TypeScript compiler build and run a real program");
  const retention = await local.request({ operation: "retention" });
  assert.deepEqual(retention.staging, {});
  assert.equal(Object.keys(retention.pins).length, 1);
  report.checks.push("transfer staging cleared; complete environment remains pinned");
  report.nodePeakRssBytes = process.resourceUsage().maxRSS * 1024;
  const status = await fs.readFile(`/proc/${local.pid}/status`, "utf8");
  report.nativePeakRssBytes = Number(/^VmHWM:\s+(\d+) kB$/m.exec(status)?.[1] || 0) * 1024;
  report.limits = "Full eager restore and export on disposable Linux VM; separate process peak RSS values. No lazy hydration, remote action hit, end-to-end agent-start or fleet durability claim.";
  report.ok = true;
} catch (error) {
  report.ok = false;
  report.phase = phase;
  report.errorName = error.name;
  report.errorCode = error.code;
  process.exitCode = 1;
} finally {
  if (local) await local.close().catch(() => { report.cleanupWarning = "inspect local transfer retention"; });
  report.verifiedAt = new Date().toISOString();
  await fs.writeFile(reportFile, JSON.stringify(report, null, 2) + "\n", { mode: 0o600 });
}
