import test from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, writeFile, rm, chmod, readFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

const snapshotUrl = process.env.RUDDER_CODEX_ACCESS_SNAPSHOT_TEST_MODULE
  ? pathToFileURL(path.resolve(process.env.RUDDER_CODEX_ACCESS_SNAPSHOT_TEST_MODULE)).href
  : new URL("../dist/managed-codex-access-snapshot.js", import.meta.url).href;
const handoffUrl = process.env.RUDDER_MANAGED_PROFILE_TEST_MODULE
  ? pathToFileURL(path.resolve(process.env.RUDDER_MANAGED_PROFILE_TEST_MODULE)).href
  : new URL("../dist/managed-task-profile.js", import.meta.url).href;
const { codexOAuthAccessSnapshot, MAX_CODEX_ACCESS_TOKEN_BYTES } = await import(snapshotUrl);
const encode = value => Buffer.from(JSON.stringify(value)).toString("base64url");
const jwt = claims => `${encode({ alg: "FIXTURE_ONLY" })}.${encode(claims)}.Zml4dHVyZQ`;
const claims = (exp = Math.floor(Date.now() / 1000) + 600, auth = {}) => ({ exp,
  "https://api.openai.com/auth": { chatgpt_account_id: "fixture-account", ...auth } });
const oauth = (access = jwt(claims())) => ({ type: "oauth", provider: "openai-codex", access,
  refresh: "FIXTURE_REFRESH_MUST_NEVER_LEAVE", expires: 1, accountId: "fixture-account" });

test("snapshot uses JWT deadline DATA, preserves exact selected account, and never exports refresh", () => {
  const credential = oauth(jwt(claims(2000000000, { chatgpt_plan_type: "pro" })));
  const now = 2000000000000 - 1000;
  const result = codexOAuthAccessSnapshot("selected", credential, now);
  assert.deepEqual(result, { formatVersion: 1, kind: "CodexOAuthAccessSnapshotV1", profileId: "selected",
    backend: "codex", provider: "openai-codex", accessToken: credential.access,
    chatgptAccountId: "fixture-account", expiresAtMs: 2000000000000, chatgptPlanType: "pro" });
  assert.ok(!JSON.stringify(result).includes(credential.refresh));
  assert.equal(credential.expires, 1, "legacy estimated expiry neither used nor written");
  for (const plan of [undefined, null]) {
    const value = codexOAuthAccessSnapshot("selected", oauth(jwt(claims(2000000000, { chatgpt_plan_type: plan }))), now);
    assert.ok(!Object.hasOwn(value, "chatgptPlanType"));
  }
});

test("malformed, expired, foreign-account or wrong-provider snapshots always have static failures", () => {
  const bad = [oauth("FIXTURE_SECRET_INVALID"), oauth(jwt(claims(1))), oauth(jwt(claims(1.5))),
    oauth(jwt(claims(Number.MAX_SAFE_INTEGER))), oauth(jwt(claims(2000000000, { chatgpt_account_id: "other" }))),
    oauth(jwt(claims(2000000000, { chatgpt_plan_type: 4 }))), oauth(jwt({ exp: 2000000000 })),
    oauth(jwt({ ...claims(2000000000), padding: "x".repeat(17000) })),
    oauth(jwt(claims(2000000000, { chatgpt_plan_type: "bad\nplan" }))),
    { ...oauth(), accountId: undefined }, { ...oauth(), provider: "anthropic" },
    { type: "api_key", provider: "openai", key: "FIXTURE_KEY" },
    oauth(`a.${Buffer.from([0xff]).toString("base64url")}.b`), oauth("a." + "x".repeat(MAX_CODEX_ACCESS_TOKEN_BYTES) + ".b")];
  for (const credential of bad) assert.throws(() => codexOAuthAccessSnapshot("selected", credential, 2000000000000 - 1000),
    error => error.message === "The selected Codex OAuth access snapshot is unavailable or expired.");
  for (const now of [NaN, Infinity, -1, 2000000000000])
    assert.throws(() => codexOAuthAccessSnapshot("selected", oauth(jwt(claims(2000000000))), now));
  const boundary = 2000000000000 - 24 * 60 * 60 * 1000;
  assert.equal(codexOAuthAccessSnapshot("selected", oauth(jwt(claims(2000000000))), boundary).expiresAtMs, 2000000000000);
  assert.throws(() => codexOAuthAccessSnapshot("selected", oauth(jwt(claims(2000000000))), boundary - 1));
});

const runner = String.raw`
const api=await import(process.argv[1]), selection=JSON.parse(process.argv[2]), fd=Number(process.argv[3]);
const first=await api.handoffManagedTaskCodexOAuthAccessSnapshot(selection,fd);
const second=await api.handoffManagedTaskCodexOAuthAccessSnapshot(selection,fd);
console.log(JSON.stringify({first,second}));
await new Promise(resolve=>process.stdin.once('data',resolve));
`;
const fixturePython = String.raw`
import os,sys,json,subprocess,select,time,base64
c=json.loads(sys.argv[1]);r=w=p=None;payload=b'';initial=0
try:
 if c['kind']=='regular':w=os.open(c['home']+'/not-a-pipe',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
 else:
  r,w=os.pipe2(os.O_CLOEXEC|os.O_NONBLOCK)
  if c['kind']=='public':os.fchmod(w,0o640)
  if c['kind']=='blocking':os.set_blocking(w,True)
  if c['kind']=='full':
   while True:
    try:initial+=os.write(w,b'x'*4096)
    except BlockingIOError:break
  if c['kind']=='closed':os.close(r);r=None
 p=subprocess.Popen([c['node'],'--input-type=module','-e',c['runner'],c['module'],json.dumps(c['selection']),str(w)],pass_fds=(w,),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 os.close(w);w=None;end=time.monotonic()+6;control=b'';eof=r is None
 while b'\n' not in control or not eof:
  if time.monotonic()>=end:raise RuntimeError('fixture deadline')
  watch=[p.stdout] if b'\n' not in control else []
  if r is not None and not eof and (c['kind']!='full' or b'\n' in control):watch.append(r)
  for fd in select.select(watch,[],[],0.02)[0]:
   if fd==p.stdout:
    chunk=os.read(p.stdout.fileno(),8192)
    if not chunk:raise RuntimeError('no metadata')
    control+=chunk
    if len(control)>8192:raise RuntimeError('metadata bound')
   else:
    chunk=os.read(r,8192)
    if not chunk:eof=True
    payload+=chunk
    if len(payload)>131072:raise RuntimeError('pipe bound')
 alive=p.poll() is None;tail,errors=p.communicate(input=b'\n',timeout=2)
 if tail:raise RuntimeError('trailing output')
 print(json.dumps({'reply':json.loads(control),'payload':base64.b64encode(payload).decode(),'eof':eof,'alive':alive,'exit':p.returncode,'errors':errors.decode(),'initial':initial}))
finally:
 if p is not None and p.poll() is None:p.kill();p.communicate()
 if r is not None:os.close(r)
 if w is not None:os.close(w)
`;
const exec = promisify(execFile);
async function fixture(credential, kind = "pipe", backend = "codex") {
  const home = await mkdtemp(path.join(os.tmpdir(), "rudder-access-snapshot-"));
  try {
    await chmod(home, 0o700);
    const store = JSON.stringify({ version: 1, profiles: { selected: credential,
      other: { type: "api_key", provider: "openai", key: "FIXTURE_OTHER_KEY" } } });
    const filename = path.join(home, "auth-profiles.json");
    await writeFile(filename, store, { mode: 0o600 });
    const { stdout, stderr } = await exec("python3", ["-c", fixturePython, JSON.stringify({ home, kind,
      node: process.execPath, runner, module: handoffUrl, selection: { profileId: "selected", backend } })],
    { timeout: 9000, maxBuffer: 512 * 1024, env: { ...process.env, RUDDER_HOME: home,
      OPENAI_API_KEY: "FIXTURE_ENV_NO_FALLBACK", CODEX_HOME: path.join(home, "NO_EXTERNAL_LOOKUP") } });
    assert.equal(stderr, ""); assert.equal(await readFile(filename, "utf8"), store, "private profile is never modified");
    const result = JSON.parse(stdout); result.bytes = Buffer.from(result.payload, "base64");
    assert.equal(result.exit, 0); assert.equal(result.errors, ""); assert.equal(result.alive, true);
    assert.equal(result.eof, true); assert.equal(result.reply.second.error.code, "ALREADY_USED");
    for (const secret of [credential.access, credential.refresh, "FIXTURE_OTHER_KEY", "FIXTURE_ENV_NO_FALLBACK"].filter(Boolean))
      assert.ok(!JSON.stringify(result.reply).includes(secret));
    return result;
  } finally { await rm(home, { recursive: true, force: true }); }
}
const linux = (name, fn) => test(name, { skip: process.platform !== "linux" }, fn);
linux("actual private nonblocking pipe concurrently drains a snapshot larger than PIPE_BUF and closes before exit", async () => {
  const credential = oauth(jwt({ ...claims(), padding: "x".repeat(10000) }));
  const result = await fixture(credential);
  assert.equal(result.reply.first.ok, true); assert.ok(result.bytes.length > 4096);
  assert.equal(result.bytes.readUInt32BE(0), result.bytes.length - 4);
  assert.deepEqual(JSON.parse(result.bytes.subarray(4)), codexOAuthAccessSnapshot("selected", credential));
  assert.ok(!result.bytes.includes(Buffer.from(credential.refresh)));
  assert.equal(result.reply.first.metadata.credentialKind, "oauth");
});
linux("full, closed and invalid pipes fail boundedly with no replay and fixed metadata", async () => {
  for (const kind of ["full", "closed", "regular", "public", "blocking"]) {
    const result = await fixture(oauth(), kind);
    assert.equal(result.reply.first.ok, false);
    assert.ok(["INVALID_OAUTH_ACCESS_SNAPSHOT", "PRIVATE_PIPE_REQUIRED"].includes(result.reply.first.error.code));
    if (kind === "full") {
      assert.ok(result.initial > 0); assert.equal(result.bytes.length, result.initial);
      assert.ok(result.bytes.every(byte => byte === 120));
    } else assert.equal(result.bytes.length, 0);
  }
});
linux("OAuth snapshot handoff refuses stale access, API-key fallback and Claude OAuth without writing", async () => {
  for (const [credential, backend] of [[oauth(jwt(claims(1))), "codex"],
    [{ type: "api_key", provider: "openai", key: "FIXTURE_KEY" }, "codex"],
    [{ ...oauth(), provider: "anthropic" }, "claude"]]) {
    const result = await fixture(credential, "pipe", backend);
    assert.equal(result.reply.first.ok, false); assert.equal(result.bytes.length, 0);
  }
});
