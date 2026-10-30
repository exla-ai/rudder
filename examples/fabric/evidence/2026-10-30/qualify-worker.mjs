// Explicit disposable-worker qualification, never a normal worker entrypoint.
// Credentials come only from the worker's private environment, never argv/report.
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { createHash } from "node:crypto";

const exec = promisify(execFile);
const OUTPUT_LIMIT = 64 * 1024;
export const DIAGNOSTIC_LIMIT = 4096;

export async function qualifierSha256() {
  return createHash("sha256").update(await fs.readFile(new URL(import.meta.url))).digest("hex");
}

function diagnostic(value, privateEnv, privatePaths) {
  let text = typeof value === "string" ? value : Buffer.isBuffer(value) ? value.toString("utf8") : "";
  const secrets = Object.entries(privateEnv).filter(([key, value]) => /TOKEN|SECRET|PASSWORD|PASSWD|AUTH|CREDENTIAL|(?:API|ACCESS|PRIVATE)[_-]?KEY/i.test(key) && typeof value === "string" && value.length).map(([, value]) => value);
  for (const secret of secrets.sort((a, b) => b.length - a.length)) {
    for (const spelling of new Set([secret, encodeURIComponent(secret), JSON.stringify(secret).slice(1, -1)])) text = text.split(spelling).join("[REDACTED]");
  }
  text = text.replace(/-----BEGIN [^-]*PRIVATE KEY-----[\s\S]*?(?:-----END [^-]*PRIVATE KEY-----|$)/g, "[REDACTED PRIVATE KEY]")
    .replace(/\b(Bearer|Basic)\s+[^\s"'<>]+/gi, "$1 [REDACTED]")
    .replace(/\b(?:rdrw_|github_pat_|gh[pousr]_)[A-Za-z0-9_-]+/g, "[REDACTED]")
    .replace(/\b(token|secret|password|api[_-]?key|authorization|access[_-]?key)\b\s*[:=]\s*(?:"[^"]*"|'[^']*'|[^\s,;]+)/gi, "$1=[REDACTED]")
    .replace(/https?:\/\/[^\s"'<>]+/g, (value) => {
      try { const url = new URL(value); return `${url.protocol}//${url.host}${url.pathname}${url.search ? "?[REDACTED]" : ""}${url.hash ? "#[REDACTED]" : ""}`; }
      catch { return "[REDACTED URL]"; }
    });
  for (const location of [...privatePaths].filter(Boolean).sort((a, b) => b.length - a.length)) text = text.split(location).join("[qualification-path]");
  text = text.replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "").replace(/[\x00-\x08\x0b-\x1f\x7f]/g, "");
  const bytes = Buffer.from(text, "utf8");
  return { text: new TextDecoder().decode(bytes.subarray(0, DIAGNOSTIC_LIMIT), { stream: true }), truncated: bytes.length > DIAGNOSTIC_LIMIT };
}

export async function runCompilerCommand(program, args, { cwd, env, privateEnv = process.env, privatePaths = [] }) {
  const secrets = { ...privateEnv };
  try { return (await exec(program, args, { cwd, env, timeout: 60_000, maxBuffer: OUTPUT_LIMIT, encoding: "utf8" })).stdout.trim(); }
  catch (failure) {
    // execFile's message includes argv. Never retain its raw Error or cause.
    const error = new Error("prepared compiler qualification command failed");
    error.name = "CompilerQualificationError";
    error.code = typeof failure.code === "number" || typeof failure.code === "string" && /^[A-Za-z0-9_-]{1,64}$/.test(failure.code) ? failure.code : "command_failed";
    if (typeof failure.signal === "string" && /^SIG[A-Z0-9]+$/.test(failure.signal)) error.signal = failure.signal;
    error.diagnostics = { stdout: diagnostic(failure.stdout, secrets, privatePaths), stderr: diagnostic(failure.stderr, secrets, privatePaths) };
    throw error;
  }
}

async function archiveEvidence(file) {
  const stat = await fs.stat(file);
  assert(stat.isFile() && stat.size > 8 && stat.size <= 64 * 1024 * 1024, "compiler must produce a bounded nonempty library archive");
  const bytes = await fs.readFile(file);
  assert.equal(bytes.subarray(0, 8).toString("ascii"), "!<arch>\n", "Rust output must be a real rlib archive");
  return { bytes: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex") };
}

export async function qualifyPreparedCompilers({ runtime, scratch, home, marker, report = { checks: [] }, setPhase = () => {}, privateEnv = process.env }) {
  report.qualifierSha256 = await qualifierSha256();
  const env = { PATH: "/usr/bin:/bin", HOME: home, TMPDIR: scratch, LC_ALL: "C", SOURCE_DATE_EPOCH: "0", LD_LIBRARY_PATH: path.join(runtime, "tools/rust/lib") };
  const run = (program, args) => runCompilerCommand(program, args, { cwd: scratch, env, privateEnv, privatePaths: [runtime, scratch, home] });
  const rustc = path.join(runtime, marker.compiler_paths.rust);
  const node = path.join(runtime, marker.compiler_paths.node);
  report.compilerProfile = {
    sourceProfile: "rudder-linux-rust-typescript-v1", preparationRevision: marker.preparation_revision,
    rust: "build rlib and compile a dependent rlib through --extern; no native executable or Rust runtime execution",
    typescript: "compile with restored TypeScript and run JavaScript with restored Node",
    binaryLinking: "not qualified; preparation revision 2 omits the self-contained linker, so the original unsupported binary check failed without implying corrupt restored bytes",
  };
  assert.equal(marker.format_version, 1);
  assert.equal(marker.preparation_revision, 2, "qualifier supports the documented library-only prepared profile");
  setPhase("restored-compiler-versions");
  report.rustc = await run(rustc, ["--version"]);
  report.node = await run(node, ["--version"]);
  assert.equal(report.rustc, marker.rustc);
  assert.equal(report.node, marker.node);
  setPhase("compile-rust-library");
  await fs.writeFile(path.join(scratch, "library.rs"), "pub const ANSWER: u32 = 6 * 7;\npub fn answer() -> u32 { ANSWER }\npub fn standard_library() -> Vec<u32> { vec![ANSWER] }\n");
  const compileStarted = Date.now();
  const flags = ["--sysroot", path.join(runtime, "tools/rust"), "--crate-type=rlib", "--edition=2021", `--remap-path-prefix=${scratch}=.`, "-C", "target-cpu=generic"];
  const library = path.join(scratch, "librestored_library.rlib");
  await run(rustc, [...flags, "--crate-name=restored_library", "library.rs", "-o", library]);
  report.rustLibrary = await archiveEvidence(library);
  setPhase("consume-rust-library");
  await fs.writeFile(path.join(scratch, "consumer.rs"), "extern crate restored_library;\nconst _: [(); 42] = [(); restored_library::ANSWER as usize];\npub fn answer() -> u32 { restored_library::answer() }\npub fn standard_library() -> Vec<u32> { restored_library::standard_library() }\n");
  const consumer = path.join(scratch, "librestored_consumer.rlib");
  await run(rustc, [...flags, "--crate-name=restored_consumer", "--extern", `restored_library=${library}`, "consumer.rs", "-o", consumer]);
  report.rustConsumer = await archiveEvidence(consumer);
  report.rustCompileMs = Date.now() - compileStarted;
  setPhase("compile-typescript");
  await fs.writeFile(path.join(scratch, "main.ts"), "const answer: number = 6 * 7; console.log(answer);\n");
  const tsStarted = Date.now();
  await run(node, [path.join(runtime, marker.compiler_paths.typescript), "--strict", "--skipLibCheck", "--target", "ES2022", "--module", "commonjs", "--outDir", "out", "main.ts"]);
  assert.equal(await run(node, [path.join(scratch, "out/main.js")]), "42");
  report.typescriptCompileMs = Date.now() - tsStarted;
  report.checks.push("restored pinned Rust compiler builds a real library and compiles a dependent library through its metadata/API", "restored Node and TypeScript compiler build and run a real program returning 42");
  return report;
}

export async function qualifyWorker(args = process.argv.slice(2)) {
  const [rootId, expectedFile, reportFile] = args;
  assert(rootId && expectedFile && reportFile, "root ID, reference file and report file required");
  const expected = JSON.parse(await fs.readFile(expectedFile, "utf8"));
  const moduleRoot = process.env.RUDDER_FABRIC_MODULE_ROOT || "/opt/rudder/dist";
  const { FabricLocalClient } = await import(pathToFileURL(path.join(moduleRoot, "fabric-local.js")));
  const { FabricRemoteClient } = await import(pathToFileURL(path.join(moduleRoot, "fabric-remote.js")));
  const cloud = new URL(process.env.RUDDER_CLOUD_URL || "https://rudder-cloud-control.fly.dev");
  assert.equal(cloud.protocol, "https:");
  assert(!cloud.username && !cloud.password && !cloud.search && !cloud.hash);
  assert(process.env.RUDDER_WORKER_TOKEN && process.env.RUDDER_WORKSPACE_ID);
  const report = { format_version: 1, kind: "independent-worker-prepared-restore", machine: process.env.FLY_MACHINE_ID, qualifierSha256: await qualifierSha256(), checks: [] };
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
    await qualifyPreparedCompilers({ runtime, scratch, home, marker, report, setPhase: (value) => { phase = value; } });
    const retention = await local.request({ operation: "retention" });
    assert.deepEqual(retention.staging, {});
    assert.equal(Object.keys(retention.pins).length, 1);
    report.checks.push("transfer staging cleared; complete environment remains pinned");
    report.nodePeakRssBytes = process.resourceUsage().maxRSS * 1024;
    const status = await fs.readFile(`/proc/${local.pid}/status`, "utf8");
    report.nativePeakRssBytes = Number(/^VmHWM:\s+(\d+) kB$/m.exec(status)?.[1] || 0) * 1024;
    report.limits = "Full eager restore/export on disposable Linux VM; Rust library build/consumption only and TypeScript build/run. Separate process peak RSS values. No Rust executable/linker, lazy hydration, remote action hit, end-to-end agent-start or fleet durability claim.";
    report.ok = true;
  } catch (error) {
    report.ok = false;
    report.phase = phase;
    report.errorName = error.name;
    report.errorCode = error.code;
    if (error.signal) report.errorSignal = error.signal;
    if (error.diagnostics) report.diagnostics = error.diagnostics;
    process.exitCode = 1;
  } finally {
    if (local) await local.close().catch(() => { report.cleanupWarning = "inspect local transfer retention"; });
    report.verifiedAt = new Date().toISOString();
    await fs.writeFile(reportFile, JSON.stringify(report, null, 2) + "\n", { mode: 0o600 });
  }
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) await qualifyWorker();
