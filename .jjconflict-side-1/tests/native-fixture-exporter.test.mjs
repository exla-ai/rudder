import test from "node:test";
import assert from "node:assert/strict";
import { createNativeFixtureExporter } from "./helpers/native-fixture-exporter.mjs";

const root = "/synthetic/repo";
const spec = { kind: "bin", name: "rudder-native", source: "native/src/main.rs", selector: "agent_broker::tests::export_broker_fixture", directoryVariable: "RUDDER_AGENT_BROKER_FIXTURE_DIR" };
const artifact = (changes = {}) => ({ reason: "compiler-artifact", target: { name: spec.name, kind: [spec.kind], src_path: `${root}/${spec.source}` }, profile: { test: true }, executable: "/synthetic/current-harness", ...changes });
const result = (rows) => ({ status: 0, signal: null, stdout: rows.map(JSON.stringify).join("\n") + "\n", stderr: "" });

test("compile once, preserve caller environment and run every fixture directly with separate bounds", () => {
  const calls = [];
  const run = (file, args, options) => { calls.push({ file, args, options }); return file === "cargo" ? result([artifact(), { reason: "build-finished", success: true }]) : result([]); };
  const generate = createNativeFixtureExporter(root, spec, run);
  generate("/fixture/one"); generate("/fixture/two");
  assert.equal(calls.length, 3);
  assert.deepEqual(calls[0].args, ["test", "--workspace", "--locked", "--features", "rudder-vcs-bridge/cli", "--bin", "rudder-native", "--no-run", "--message-format=json-render-diagnostics"]);
  assert.deepEqual(calls[0].options.env, { ...process.env });
  assert.equal(calls[0].options.timeout, 900_000);
  for (const [index, directory] of [[1, "/fixture/one"], [2, "/fixture/two"]]) {
    assert.equal(calls[index].file, "/synthetic/current-harness");
    assert.deepEqual(calls[index].args, [spec.selector, "--exact", "--ignored"]);
    assert.deepEqual(calls[index].options.env, { ...process.env, [spec.directoryVariable]: directory });
    assert.equal(calls[index].options.timeout, 20_000);
  }
});

test("integration harness uses its exact Cargo target and exporter", () => {
  const integration = { kind: "test", name: "agent_api_tools", source: "engine/tests/agent_api_tools.rs", selector: "export_agent_mcp_fixture", directoryVariable: "RUDDER_AGENT_MCP_FIXTURE_DIR" };
  const calls = [];
  createNativeFixtureExporter(root, integration, (file, args, options) => {
    calls.push({ file, args, options });
    return file === "cargo" ? result([artifact({ target: { name: integration.name, kind: ["test"], src_path: `${root}/${integration.source}` } })]) : result([]);
  })("/fixture/mcp");
  assert.deepEqual(calls[0].args.slice(5, 7), ["--test", "agent_api_tools"]);
  assert.deepEqual(calls[1].args, ["export_agent_mcp_fixture", "--exact", "--ignored"]);
  assert.equal(calls[1].options.env.RUDDER_AGENT_MCP_FIXTURE_DIR, "/fixture/mcp");
});

test("wrong target, source, profile, absent or ambiguous executable never executes a fixture", () => {
  for (const rows of [[], [artifact({ target: { name: "other", kind: ["bin"], src_path: `${root}/${spec.source}` } })], [artifact({ target: { name: spec.name, kind: ["lib"], src_path: `${root}/${spec.source}` } })], [artifact({ target: { name: spec.name, kind: ["bin"], src_path: "/other/main.rs" } })], [artifact({ profile: { test: false } })], [artifact({ executable: null })], [artifact({ executable: "relative" })], [artifact(), artifact({ executable: "/synthetic/another-harness" })]]) {
    let calls = 0;
    const generate = createNativeFixtureExporter(root, spec, () => { calls++; return result(rows); });
    assert.throws(() => generate("/fixture/one"));
    assert.equal(calls, 1);
  }
});

test("failed or incomplete compiler result is remembered without repeated cold compiles", () => {
  for (const failure of [{ status: 1 }, { status: null, signal: "SIGTERM" }, { error: new Error("timeout") }, { stdout: "not Cargo JSON\n" }]) {
    let calls = 0;
    const generate = createNativeFixtureExporter(root, spec, () => { calls++; return { ...result([artifact()]), ...failure }; });
    assert.throws(() => generate("/fixture/one"));
    assert.throws(() => generate("/fixture/two"));
    assert.equal(calls, 1);
  }
});

test("fixture failures do not compile again and cannot be treated as exported data", () => {
  let builds = 0, runs = 0;
  const generate = createNativeFixtureExporter(root, spec, (file) => {
    if (file === "cargo") { builds++; return result([artifact()]); }
    runs++; return { ...result([]), error: new Error("fixture timed out"), status: null };
  });
  assert.throws(() => generate("/fixture/one"), /fixture timed out/);
  assert.throws(() => generate("/fixture/two"), /fixture timed out/);
  assert.equal(builds, 1); assert.equal(runs, 2);
});
