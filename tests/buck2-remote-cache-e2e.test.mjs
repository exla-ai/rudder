import assert from "node:assert/strict";
import { execFile, spawnSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { EventEmitter, once } from "node:events";
import fs from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { gunzipSync } from "node:zlib";
import { createRemoteBuildCacheHandler, MemoryRemoteCacheStorage } from "../cloud/dist/remote-build-cache.js";
import { startBuck2Cache } from "../cloud/worker/buck2-cache.mjs";
import { startCacheProxy } from "../cloud/worker/cache-proxy.mjs";

const run = promisify(execFile);
const repo = path.resolve(import.meta.dirname, "..");
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

test("fresh Buck2 homes restore the board CSS action and output through the Cloud cache", async (t) => {
  if (process.platform !== "linux" || process.arch !== "x64") return t.skip("requires the Linux x64 worker toolchain");
  const located = spawnSync("which", ["buck2", "bazel-remote"], { encoding: "utf8" });
  if (located.status !== 0) return t.skip("requires buck2 and bazel-remote on PATH");
  const [buck2, bazelRemote] = located.stdout.trim().split("\n");
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "rudder-buck2-cache-"));
  t.after(() => fs.rm(root, { force: true, recursive: true }));

  const account = "buck2-cache-test-account";
  const namespace = `cache/v1/accounts/${sha256(account)}/buck2/`;
  const tokens = new Map();
  const changes = new EventEmitter();
  const storage = new MemoryRemoteCacheStorage();
  const handler = createRemoteBuildCacheHandler({
    storage, bucket: "buck2-test-bucket",
    authenticate(req) {
      const token = tokens.get(req.headers["x-rudder-workspace-id"]);
      assert.ok(token && req.headers.authorization === `Bearer ${token}`);
      return account;
    },
  });
  const cloud = http.createServer((req, res) => {
    // bazel-remote writes its upstream asynchronously after acknowledging a gRPC upload.
    if (req.method === "PUT") res.once("finish", () => changes.emit("write"));
    void handler(req, res, new URL(req.url, "http://127.0.0.1"));
  });
  cloud.listen(0, "127.0.0.1");
  await once(cloud, "listening");
  t.after(() => new Promise((resolve) => cloud.close(resolve)));
  const cloudUrl = `http://127.0.0.1:${cloud.address().port}`;

  async function build(phase) {
    const home = path.join(root, phase, "home");
    const cwd = path.join(root, phase, "repo");
    await fs.mkdir(home, { recursive: true });
    await fs.mkdir(cwd, { recursive: true });
    for (const name of [".buckconfig", "BUCK", "scripts/board-build.bzl", "src/board/ui/board.css", "node_modules/@esbuild/linux-x64/bin/esbuild"]) {
      const dest = path.join(cwd, name);
      await fs.mkdir(path.dirname(dest), { recursive: true });
      await fs.copyFile(path.join(repo, name), dest);
    }
    const workerToken = randomBytes(32).toString("hex");
    tokens.set(phase, workerToken);
    const proxy = startCacheProxy({ cloudUrl, homeId: phase, workerToken });
    try {
      const cache = await startBuck2Cache({
        home, binary: bazelRemote, cacheDir: path.join(root, phase, "reapi"), proxy: await proxy.ready,
      });
      try {
        const options = { cwd, env: { HOME: home, PATH: "/usr/bin:/bin", LANG: "C" }, timeout: 45_000 };
        const log = path.join(home, "events.json");
        const report = path.join(home, "report.json");
        try {
          await run(buck2, ["build", "//:board_css", "--console", "simple", "--event-log", log, "--build-report", report], options);
          const results = JSON.parse(await fs.readFile(report, "utf8")).results;
          const [target] = Object.values(results);
          const artifact = await fs.readFile(path.join(cwd, target.outputs.DEFAULT[0]));
          const events = gunzipSync(await fs.readFile(log)).toString("utf8").trim().split("\n").map(JSON.parse);
          const commands = events.flatMap((event) => event.Event?.data?.SpanEnd?.data?.ActionExecution?.commands ?? [])
            .filter((command) => command.status.Success).map((command) => command.details.command_kind.command);
          if (phase === "cold") {
            assert.ok(commands.some((command) => command.LocalCommand || command.OmittedLocalCommand), "expected a local CSS build");
            const outputKey = namespace + "cas/" + sha256(artifact);
            const uploaded = () => storage.keys().includes(outputKey) && storage.keys().some((key) => key.startsWith(namespace + "ac/"));
            const deadline = AbortSignal.timeout(10_000);
            while (!uploaded()) await once(changes, "write", { signal: deadline });
          } else {
            assert.ok(commands.some((command) => command.RemoteCommand?.cache_hit === true), "Buck2 did not report a remote action cache hit");
            assert.ok(proxy.snapshot().buck2.hits >= 2);
          }
          return artifact;
        } finally {
          await run(buck2, ["kill"], options);
        }
      } finally {
        await cache.close();
      }
    } finally {
      await proxy.close();
    }
  }

  const cold = await build("cold");
  assert.ok(cold.length > 0);
  const warm = await build("warm");
  assert.deepEqual(warm, cold);
  const { buck2: stats, recent } = handler.statsForAccount(account);
  assert.ok(recent.some((event) => event.op === "hit" && event.key.startsWith("ac/")));
  assert.ok(recent.some((event) => event.op === "hit" && event.key === "cas/" + sha256(warm)));
  assert.equal(stats.errors, 0);
  t.diagnostic(`Cold and warm CSS SHA-256 ${sha256(warm)}, ${warm.length} bytes. Cloud saw ${stats.writes} writes, ${stats.hits} hits, ${stats.bytesRead} bytes read.`);
});
