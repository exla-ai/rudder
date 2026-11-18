import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, chmod, realpath, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { build } from "esbuild";

const result = await build({ entryPoints: [new URL("../src/project-files/engine.ts", import.meta.url).pathname], bundle: true, write: false, platform: "node", format: "esm", target: "node22" });
const { engineFileGrants } = await import(`data:text/javascript;base64,${Buffer.from(result.outputFiles[0].text).toString("base64")}`);

// A fake rudder-native that answers `engine files` the way the owner's control
// socket does, or fails the way the native CLI does.
async function fixture(t, { authority, stderr }) {
  const base = await mkdtemp(path.join(await realpath(os.tmpdir()), "project-files-engine-"));
  t.after(() => rm(base, { recursive: true, force: true }));
  const repo = path.join(base, "repo");
  await mkdir(repo);
  const owner = { project_id: "project-a", owner_id: "owner-a", epoch: 3 };
  const status = {
    format_version: 1, running: true, repo, authority, state: "ready", owner,
    instance_id: "instance-a", attach_socket: path.join(repo, ".rudder", "attach.sock"),
    file_grants: [{ scope: { projectId: "project-a", workspaceId: "checkout", attemptId: "checkout", ownerEpoch: 3 }, root: repo, access: { kind: "host" } }],
  };
  const native = path.join(base, "rudder-native");
  await writeFile(native, stderr
    ? `#!/bin/sh\necho 'Error: ${stderr}' >&2\necho '  0: detail' >&2\nexit 1\n`
    : `#!/bin/sh\ncat <<'JSON'\n${JSON.stringify(status)}\nJSON\n`);
  await chmod(native, 0o755);
  return { repo, native };
}

test("a dashboard owner (legacy-native) grants the checkout like the Fabric engine", async (t) => {
  for (const authority of ["fabric", "legacy-native"]) {
    const { repo, native } = await fixture(t, { authority });
    const grants = await engineFileGrants(repo, AbortSignal.timeout(3000), native);
    assert.deepEqual(grants.map((grant) => [grant.scope.workspaceId, grant.root]), [["checkout", repo]], authority);
  }
});

test("an old TS scheduler owner is refused by name", async (t) => {
  const { repo, native } = await fixture(t, { authority: "legacy-ts" });
  await assert.rejects(engineFileGrants(repo, AbortSignal.timeout(3000), native), { code: "denied", message: /old Rudder scheduler/ });
});

test("the native owner's own error reaches the caller", async (t) => {
  const { repo, native } = await fixture(t, { authority: "fabric", stderr: "the project owner has no engine control socket" });
  await assert.rejects(engineFileGrants(repo, AbortSignal.timeout(3000), native), { code: "denied", message: "the project owner has no engine control socket" });
});
