import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, chmod, readFile, writeFile, rm, lstat, symlink, link, mkdir } from "node:fs/promises";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

// Disposable synthetic homes only. Never select an actual saved profile.
const modulePath = process.env.RUDDER_CODEX_SNAPSHOT_STORE_TEST_MODULE
  ?? path.resolve("dist/managed-codex-access-snapshot-store.js");
const profilePath = process.env.RUDDER_MANAGED_PROFILE_TEST_MODULE ?? path.resolve("dist/managed-task-profile.js");
const cliPath = path.join(path.dirname(modulePath), "managed-task-profile-cli.js");
const api = await import(pathToFileURL(modulePath).href);
const profiles = await import(pathToFileURL(profilePath).href);
const enc = value => Buffer.from(JSON.stringify(value)).toString("base64url");
function snapshot(exp = Math.floor(Date.now() / 1000) + 600) {
  const claims = { exp, "https://api.openai.com/auth": { chatgpt_account_id: "FIXTURE_ACCOUNT", chatgpt_plan_type: "pro" } };
  return { formatVersion: 1, kind: "CodexOAuthAccessSnapshotV1", profileId: "selected", backend: "codex",
    provider: "openai-codex", accessToken: `${enc({ alg: "FIXTURE_ONLY" })}.${enc(claims)}.Zml4dHVyZQ`,
    chatgptAccountId: "FIXTURE_ACCOUNT", expiresAtMs: exp * 1000, chatgptPlanType: "pro" };
}
const raw = value => Buffer.from(JSON.stringify(value));
async function homeFixture(work) {
  const home = await mkdtemp(path.join(os.tmpdir(), "rudder-import-fixture-"));
  const previous = process.env.RUDDER_HOME;
  process.env.RUDDER_HOME = home;
  try { await chmod(home, 0o700); await work(home, path.join(home, api.CODEX_ACCESS_SNAPSHOT_FILENAME)); }
  finally {
    if (previous === undefined) delete process.env.RUDDER_HOME; else process.env.RUDDER_HOME = previous;
    await rm(home, { recursive: true, force: true });
  }
}
const refuses = promise => assert.rejects(promise, error => error.message ===
  "The selected Codex OAuth access snapshot is unavailable or expired.");

test("closed canonical access-only validator reuses account/expiry claims and refuses all extra credential kinds", () => {
  const value = snapshot();
  assert.deepEqual(api.validateCodexOAuthAccessSnapshot(raw(value)), value);
  const noPlan = snapshot(); delete noPlan.chatgptPlanType;
  const jwtClaims = { exp: noPlan.expiresAtMs / 1000,
    "https://api.openai.com/auth": { chatgpt_account_id: noPlan.chatgptAccountId } };
  noPlan.accessToken = `a.${enc(jwtClaims)}.b`;
  assert.deepEqual(api.validateCodexOAuthAccessSnapshot(raw(noPlan)), noPlan);
  const invalid = [Buffer.alloc(65533), Buffer.from([255]), Buffer.from("null"), raw([]), raw({}),
    raw({ ...value, refresh: "FIXTURE_REFRESH" }), raw({ ...value, idToken: "FIXTURE_ID" }),
    raw({ ...value, formatVersion: true }), raw({ ...value, kind: "other" }), raw({ ...value, backend: "claude" }),
    raw({ ...value, provider: "anthropic" }), raw({ ...value, chatgptAccountId: "foreign" }),
    raw({ ...value, expiresAtMs: value.expiresAtMs + 1 }), raw({ ...value, chatgptPlanType: "other" }),
    raw(snapshot(1)), raw(snapshot(Math.floor(Date.now() / 1000) + 86402)),
    Buffer.from(JSON.stringify(value) + "\n"), Buffer.from(" " + JSON.stringify(value)),
    Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), raw(value)]),
    raw(Object.fromEntries(Object.entries(value).reverse())),
    Buffer.from(JSON.stringify(value).replace('"formatVersion":1', '"formatVersion":1,"formatVersion":1'))];
  for (const bytes of invalid) assert.throws(() => api.validateCodexOAuthAccessSnapshot(bytes),
    error => error.message === "The selected Codex OAuth access snapshot is unavailable or expired.");
});

test("exclusive durable import creates only private access file; inspection selects it without any normal store", async () => {
  await homeFixture(async (home, filename) => {
    const bytes = raw(snapshot()); await api.importCodexOAuthAccessSnapshot(bytes);
    assert.deepEqual(await readFile(filename), Buffer.concat([bytes, Buffer.from("\n")]));
    const stat = await lstat(filename); assert.equal(stat.mode & 0o7777, 0o600); assert.equal(stat.nlink, 1);
    assert.equal(stat.uid, process.geteuid());
    assert.deepEqual(await api.loadImportedCodexOAuthAccessSnapshot(), JSON.parse(bytes));
    assert.deepEqual(await profiles.inspectManagedTaskProfile({ profileId: "selected", backend: "codex" }), {
      ok: true, metadata: { profileId: "selected", backend: "codex", formatVersion: 1,
        credentialKind: "oauth", provider: "openai-codex" } });
    await refuses(api.importCodexOAuthAccessSnapshot(bytes));
    await assert.rejects(readFile(path.join(home, "auth-profiles.json")), { code: "ENOENT" });
  });
});

test("file absence preserves normal exact profile selection", async () => {
  await homeFixture(async (home) => {
    assert.equal(await api.loadImportedCodexOAuthAccessSnapshot(), undefined);
    await writeFile(path.join(home, "auth-profiles.json"), JSON.stringify({ version: 1,
      profiles: { selected: { type: "api_key", provider: "openai", key: "FIXTURE_KEY" } } }), { mode: 0o600 });
    assert.equal((await profiles.inspectManagedTaskProfile({ profileId: "selected", backend: "codex" })).metadata.credentialKind, "api_key");
  });
});

test("present malformed/expired import never falls back, and foreign selection refuses", async () => {
  await homeFixture(async (home, filename) => {
    await writeFile(path.join(home, "auth-profiles.json"), JSON.stringify({ version: 1,
      profiles: { selected: { type: "api_key", provider: "openai", key: "FIXTURE_FALLBACK" } } }), { mode: 0o600 });
    for (const value of ["partial", JSON.stringify(snapshot(1)) + "\n"]) {
      await writeFile(filename, value, { mode: 0o600 });
      assert.equal((await profiles.inspectManagedTaskProfile({ profileId: "selected", backend: "codex" })).ok, false);
    }
    await writeFile(filename, JSON.stringify(snapshot()) + "\n", { mode: 0o600 });
    for (const selection of [{ profileId: "foreign", backend: "codex" }, { profileId: "selected", backend: "claude" }])
      assert.equal((await profiles.inspectManagedTaskProfile(selection)).error.code, "PROFILE_BACKEND_MISMATCH");
  });
});

test("public, hardlinked, symlink and nonregular import files refuse", async () => {
  for (const kind of ["public", "hardlink", "symlink", "directory", "oversize"]) {
    await homeFixture(async (home, filename) => {
      const other = path.join(home, "other");
      if (kind === "directory") await mkdir(filename, { mode: 0o700 });
      else if (kind === "symlink") { await writeFile(other, raw(snapshot()), { mode: 0o600 }); await symlink(other, filename); }
      else {
        await writeFile(filename, kind === "oversize" ? Buffer.alloc(65534) : Buffer.concat([raw(snapshot()), Buffer.from("\n")]), { mode: 0o600 });
        if (kind === "public") await chmod(filename, 0o640);
        if (kind === "hardlink") await link(filename, other);
      }
      await refuses(api.loadImportedCodexOAuthAccessSnapshot());
    });
  }
});

test("import requires existing private canonical home and never creates it or follows home alias", async () => {
  await homeFixture(async (home, filename) => {
    await chmod(home, 0o755); await refuses(api.importCodexOAuthAccessSnapshot(raw(snapshot())));
    await assert.rejects(readFile(filename), { code: "ENOENT" });
    await chmod(home, 0o700);
    const alias = home + "-alias"; await symlink(home, alias);
    try { process.env.RUDDER_HOME = alias; await refuses(api.importCodexOAuthAccessSnapshot(raw(snapshot()))); }
    finally { process.env.RUDDER_HOME = home; await rm(alias); }
    process.env.RUDDER_HOME = path.join(home, "absent");
    await refuses(api.importCodexOAuthAccessSnapshot(raw(snapshot())));
    await assert.rejects(lstat(process.env.RUDDER_HOME), { code: "ENOENT" });
  });
});

test("stdin bounded chunks/EOF use the same import; partial/oversized streams publish nothing", async () => {
  await homeFixture(async (_home, filename) => {
    const bytes = raw(snapshot());
    for (const parts of [[bytes.subarray(0, 20)], [Buffer.alloc(65532), Buffer.from("x")]]) {
      await refuses(api.importCodexOAuthAccessSnapshotStdin((async function* () { yield* parts; })()));
      await assert.rejects(lstat(filename), { code: "ENOENT" });
    }
    await api.importCodexOAuthAccessSnapshotStdin((async function* () { yield bytes.subarray(0, 13); yield bytes.subarray(13); })());
    assert.deepEqual(await api.loadImportedCodexOAuthAccessSnapshot(), JSON.parse(bytes));
  });
});

async function cli(home, bytes, args = ["import-codex-oauth-access-snapshot"]) {
  const child = spawn(process.execPath, [cliPath, ...args], { env: { ...process.env, RUDDER_HOME: home }, stdio: ["pipe", "pipe", "pipe"] });
  let out = "", err = "";
  child.stdout.setEncoding("utf8"); child.stderr.setEncoding("utf8");
  child.stdout.on("data", value => { out += value; }); child.stderr.on("data", value => { err += value; });
  child.stdin.on("error", () => {}); child.stdin.end(bytes);
  const code = await new Promise((resolve, reject) => { child.on("error", reject); child.on("close", resolve); });
  return { code, out, err };
}
test("actual CLI import ACK is fixed, no token echo; duplicate and malformed inputs refuse", async () => {
  await homeFixture(async (home) => {
    const bytes = raw(snapshot());
    assert.deepEqual(await cli(home, bytes), { code: 0, out: api.CODEX_ACCESS_SNAPSHOT_IMPORTED_ACK, err: "" });
    for (const input of [bytes, Buffer.from("partial"), Buffer.concat([bytes, Buffer.from("\n")]), Buffer.alloc(65533)]) {
      const result = await cli(home, input); assert.equal(result.code, 1); assert.equal(result.err, "");
      assert.equal(JSON.parse(result.out).ok, false); assert.ok(!result.out.includes(snapshot().accessToken));
    }
  });
});

test("actual imported snapshot FD3 handoff uses original nonblocking pipe and contains no refresh field", { skip: process.platform !== "linux" }, async () => {
  await homeFixture(async (home) => {
    const bytes = raw(snapshot()); await api.importCodexOAuthAccessSnapshot(bytes);
    const script = String.raw`
import os,sys,subprocess,json,select,base64,time
r,w=os.pipe2(os.O_NONBLOCK|os.O_CLOEXEC)
code="const a=await import(process.argv[1]); console.log(JSON.stringify(await a.handoffManagedTaskCodexOAuthAccessSnapshot({profileId:'selected',backend:'codex'},Number(process.argv[2]))));"
p=subprocess.Popen([sys.argv[1],'--input-type=module','-e',code,sys.argv[2],str(w)],pass_fds=(w,),stdout=subprocess.PIPE,stderr=subprocess.PIPE)
os.close(w);data=b'';end=time.monotonic()+5
try:
 while True:
  if time.monotonic()>end:raise RuntimeError('fixture timeout')
  if not select.select([r],[],[],0.05)[0]:continue
  chunk=os.read(r,8192)
  if not chunk:break
  data+=chunk
 out,err=p.communicate(timeout=2)
 print(json.dumps({'exit':p.returncode,'out':out.decode(),'err':err.decode(),'frame':base64.b64encode(data).decode()}))
finally:
 os.close(r)
 if p.poll() is None:p.kill();p.communicate()
`;
    const { stdout, stderr } = await promisify(execFile)("python3", ["-c", script, process.execPath, pathToFileURL(profilePath).href],
      { timeout: 8000, env: { ...process.env, RUDDER_HOME: home } });
    assert.equal(stderr, ""); const result = JSON.parse(stdout); assert.equal(result.exit, 0); assert.equal(result.err, "");
    assert.equal(JSON.parse(result.out).metadata.credentialKind, "oauth");
    const frame = Buffer.from(result.frame, "base64"); assert.equal(frame.readUInt32BE(), bytes.length);
    assert.deepEqual(frame.subarray(4), bytes); assert.ok(!frame.includes(Buffer.from("refresh")));
  });
});
