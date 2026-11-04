import assert from "node:assert/strict";
import path from "node:path";
import { spawnSync } from "node:child_process";

// Compile once per test-file process. Cargo, including its inherited profile
// environment, selects the current harness; never search target/ for an ELF.
export function createNativeFixtureExporter(root, { kind, name, source, selector, directoryVariable }, run = spawnSync) {
  let executable, compileError;
  return (directory) => {
    if (compileError) throw compileError;
    if (!executable) {
      try {
        const built = run("cargo", ["test", "--workspace", "--locked", "--features", "rudder-vcs-bridge/cli", `--${kind}`, name, "--no-run", "--message-format=json-render-diagnostics"], {
          cwd: root, env: { ...process.env }, encoding: "utf8", timeout: 15 * 60_000, maxBuffer: 32 * 1024 * 1024,
        });
        assert.ok(!built.error && !built.signal && built.status === 0,
          `native fixture harness compilation failed: ${built.error?.message ?? built.signal ?? built.status}\n${built.stderr ?? ""}\n${(built.stdout ?? "").slice(-8192)}`);
        const candidates = new Set();
        for (const line of built.stdout.split("\n")) {
          if (!line.trim()) continue;
          const row = JSON.parse(line);
          if (row.reason !== "compiler-artifact" || row.target?.name !== name || !row.target.kind?.includes(kind) || row.target.src_path !== path.join(root, source) || row.profile?.test !== true) continue;
          assert.ok(typeof row.executable === "string" && path.isAbsolute(row.executable), "Cargo fixture artifact needs an absolute executable");
          candidates.add(row.executable);
        }
        assert.equal(candidates.size, 1, "Cargo must report exactly one current fixture test executable");
        [executable] = candidates;
      } catch (error) {
        // A failed cold compile should fail later cases without compiling again.
        compileError = error;
        throw error;
      }
    }
    const generated = run(executable, [selector, "--exact", "--ignored"], {
      cwd: root, env: { ...process.env, [directoryVariable]: directory }, encoding: "utf8", timeout: 20_000, maxBuffer: 2 * 1024 * 1024,
    });
    assert.ok(!generated.error && !generated.signal && generated.status === 0,
      `native fixture export failed: ${generated.error?.message ?? generated.signal ?? generated.status}\n${generated.stderr ?? ""}\n${generated.stdout ?? ""}`);
  };
}
