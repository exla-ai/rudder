import test from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, writeFile, rm, chmod } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

// Root compiles first, then runs these against emitted JS. No real profile is
// ever read: each child receives a new private RUDDER_HOME containing fixtures.
const moduleUrl = process.env.RUDDER_MANAGED_PROFILE_TEST_MODULE
  ? pathToFileURL(path.resolve(process.env.RUDDER_MANAGED_PROFILE_TEST_MODULE)).href
  : new URL("../dist/managed-task-profile.js", import.meta.url).href;
const run = promisify(execFile);
const nodeRunner = String.raw`
const api = await import(process.argv[1]);
const selection = JSON.parse(process.argv[2]), fd = Number(process.argv[3]), operation = process.argv[4];
let result;
if (operation === "inspect") result = await api.inspectManagedTaskProfile(selection);
else if (operation === "double") result = { first: await api.handoffManagedTaskApiKey(selection, fd), second: await api.handoffManagedTaskApiKey(selection, fd) };
else if (operation === "inspectThenHandoff") result = { inspection: await api.inspectManagedTaskProfile(selection), handoff: await api.handoffManagedTaskApiKey(selection, fd) };
else result = await api.handoffManagedTaskApiKey(selection, fd);
await new Promise((resolve, reject) => process.stdout.write(JSON.stringify(result) + "\n", error => error ? reject(error) : resolve()));
// Stay alive: the parent must see pipe EOF BEFORE permitting actual process exit.
await new Promise(resolve => process.stdin.once("data", resolve));
`;
const pipeFixture = String.raw`
import os,sys,json,subprocess,select,time,base64
cfg=json.loads(sys.argv[1]); r=None; w=None; proc=None; initial=b''
try:
    kind=cfg['pipeKind']
    if kind=='none': inherited=-1
    elif kind=='regular':
        w=os.open(os.path.join(cfg['home'],'rejected-target'),os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600); inherited=w
    else:
        r,w=os.pipe2(os.O_CLOEXEC | (0 if kind=='blocking' else os.O_NONBLOCK))
        os.set_blocking(r,False)
        inherited=r if kind=='readonly' else w
        if kind=='public': os.fchmod(w,0o640)
        if kind=='full':
            for unused in range(256):
                try: initial+=b'x'*os.write(w,b'x'*4096)
                except BlockingIOError: break
            else: raise RuntimeError('fixture pipe exceeded bounded fill')
        if kind=='noReader': os.close(r); r=None
    proc=subprocess.Popen([cfg['node'],'--input-type=module','-e',cfg['runner'],cfg['module'],json.dumps(cfg['selection']),str(inherited),cfg['operation']],
        pass_fds=() if inherited<0 else (inherited,),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    if w is not None: os.close(w); w=None
    deadline=time.monotonic()+5; control=b''
    while b'\n' not in control:
        remaining=deadline-time.monotonic()
        if remaining<=0 or not select.select([proc.stdout],[],[],remaining)[0]: raise RuntimeError('fixture control timed out')
        chunk=os.read(proc.stdout.fileno(),4096)
        if not chunk: raise RuntimeError('helper exited before metadata')
        control+=chunk
        if len(control)>16384: raise RuntimeError('helper exceeded metadata bound')
    line,extra=control.split(b'\n',1)
    if extra: raise RuntimeError('helper emitted multiple control frames')
    alive=proc.poll() is None; payload=b''; eof=None
    if r is not None:
        eof=False
        for unused in range(257):
            try: chunk=os.read(r,4096)
            except BlockingIOError: break
            if not chunk: eof=True; break
            payload+=chunk
        else: raise RuntimeError('fixture exceeded bounded pipe drain')
    elif kind=='regular':
        with open(os.path.join(cfg['home'],'rejected-target'),'rb') as f: payload=f.read(8192)
    tail,errors=proc.communicate(input=b'\n',timeout=2)
    if tail: raise RuntimeError('helper emitted trailing control output')
    print(json.dumps({'control':json.loads(line),'payload':base64.b64encode(payload).decode(),'initialBytes':len(initial),
        'eofBeforeExit':eof,'helperAliveBeforeAck':alive,'stderr':errors.decode(),'exitCode':proc.returncode}))
finally:
    if proc is not None and proc.poll() is None: proc.kill(); proc.communicate()
    if r is not None: os.close(r)
    if w is not None: os.close(w)
`;
async function fixture(profiles, options = {}) {
  const home = await mkdtemp(path.join(os.tmpdir(), "rudder-managed-profile-test-"));
  try {
    await chmod(home, 0o700);
    await writeFile(path.join(home, "auth-profiles.json"), JSON.stringify({ version: 1, profiles }), { mode: 0o600 });
    const cfg = { home, node: process.execPath, runner: nodeRunner, module: moduleUrl,
      operation: options.operation ?? "handoff", selection: options.selection ?? { profileId: "selected", backend: "codex" },
      pipeKind: options.pipeKind ?? (options.operation === "inspect" ? "none" : "anonymous") };
    const { stdout, stderr } = await run("python3", ["-c", pipeFixture, JSON.stringify(cfg)], {
      timeout: 9000, maxBuffer: 2 * 1024 * 1024,
      env: { ...process.env, RUDDER_HOME: home, OPENAI_API_KEY: "FIXTURE_ENV_MUST_NOT_BE_USED", ANTHROPIC_API_KEY: "FIXTURE_ENV_MUST_NOT_BE_USED" },
    });
    assert.equal(stderr, "");
    const result = JSON.parse(stdout);
    assert.equal(result.exitCode, 0); assert.equal(result.stderr, ""); assert.equal(result.helperAliveBeforeAck, true);
    result.bytes = Buffer.from(result.payload, "base64");
    return result;
  } finally { await rm(home, { recursive: true, force: true }); }
}
const api = (key = "FIXTURE_SELECTED_KEY", provider = "openai") => ({ type: "api_key", provider, key });
const oauth = (provider = "openai-codex") => ({ type: "oauth", provider, access: "FIXTURE_ACCESS", refresh: "FIXTURE_REFRESH", expires: 123456 });
function frame(result, key) {
  assert.equal(result.eofBeforeExit, true, "helper must actually close writer while still alive");
  const bytes = Buffer.from(key, "utf8");
  assert.equal(result.bytes.readUInt32BE(0), bytes.length); assert.deepEqual(result.bytes.subarray(4), bytes);
  assert.equal(result.bytes.length, bytes.length + 4, "exactly one frame and no trailing body");
}
function noWrite(result, code) {
  assert.deepEqual(result.bytes, Buffer.alloc(0));
  assert.equal(result.control.ok, false); assert.equal(result.control.error.code, code);
  if (result.eofBeforeExit !== null) assert.equal(result.eofBeforeExit, true);
  assert.deepEqual(Object.keys(result.control.error).sort(), ["code", "message"]);
}

test("actual inherited pipe carries only exact selected key and closes before child exit", async () => {
  const result = await fixture({ selected: api(), other: api("FIXTURE_OTHER_KEY"), subscription: oauth() });
  frame(result, "FIXTURE_SELECTED_KEY");
  assert.deepEqual(result.control, { ok: true, metadata: { profileId: "selected", backend: "codex", formatVersion: 1, credentialKind: "api_key", provider: "openai" } });
  for (const secret of ["FIXTURE_SELECTED_KEY", "FIXTURE_OTHER_KEY", "FIXTURE_ACCESS", "FIXTURE_REFRESH", "FIXTURE_ENV_MUST_NOT_BE_USED"])
    assert.ok(!JSON.stringify(result.control).includes(secret));
});

test("normal loader legacy mode/apiKey normalization works for explicit Claude profile", async () => {
  const result = await fixture({ selected: { mode: "api_key", provider: "anthropic", apiKey: "FIXTURE_CLAUDE_LEGACY" }, other: api() },
    { selection: { profileId: "selected", backend: "claude" }, operation: "inspectThenHandoff" });
  frame(result, "FIXTURE_CLAUDE_LEGACY");
  assert.equal(result.control.inspection.metadata.credentialKind, "api_key");
  assert.deepEqual(result.control.handoff.metadata, result.control.inspection.metadata);
});

test("metadata inspection returns selected kind only and OAuth/token handoff never falls back", async () => {
  for (const [credential, code] of [[oauth(), "UNSUPPORTED_OAUTH"], [{ type: "token", provider: "openai", token: "FIXTURE_TOKEN" }, "UNSUPPORTED_TOKEN"]]) {
    const inspected = await fixture({ selected: credential, anotherApiProfile: api() }, { operation: "inspect" });
    assert.equal(inspected.control.ok, true); assert.equal(inspected.control.metadata.credentialKind, credential.type);
    assert.ok(!JSON.stringify(inspected.control).includes("FIXTURE_")); assert.equal(inspected.bytes.length, 0);
    noWrite(await fixture({ selected: credential, anotherApiProfile: api() }), code);
  }
});

test("exact profile/backend selection has no alternate profile, inherited property or env fallback", async () => {
  noWrite(await fixture({ selected: api("FIXTURE_WRONG_BACKEND", "anthropic") }), "PROFILE_BACKEND_MISMATCH");
  noWrite(await fixture({ another: api() }), "PROFILE_UNAVAILABLE");
  noWrite(await fixture({ selected: api() }, { selection: { profileId: "toString", backend: "codex" } }), "PROFILE_UNAVAILABLE");
  noWrite(await fixture({ selected: api() }, { selection: { profileId: "selected", backend: "codex", fallback: true } }), "INVALID_SELECTION");
  noWrite(await fixture({ selected: { type: "api_key", provider: "openai", keyRef: { source: "env", provider: "openai", id: "OPENAI_API_KEY" } } }), "INVALID_KEY");
});

test("regular file, read-only, blocking and public-mode descriptors never receive a key", async () => {
  for (const pipeKind of ["regular", "readonly", "blocking", "public"]) noWrite(await fixture({ selected: api() }, { pipeKind }), "PRIVATE_PIPE_REQUIRED");
});

test("UTF-8 byte bound is explicit and malformed or oversized selected keys do not write", async () => {
  frame(await fixture({ selected: api("k".repeat(4092)) }), "k".repeat(4092));
  for (const key of ["", "bad\nkey", "bad\0key", "\ud800", "k".repeat(4093), "é".repeat(2047)])
    noWrite(await fixture({ selected: api(key) }), "INVALID_KEY");
});

test("one-shot process gate prevents a second frame even after success", async () => {
  const result = await fixture({ selected: api() }, { operation: "double" });
  frame(result, "FIXTURE_SELECTED_KEY"); assert.equal(result.control.first.ok, true);
  assert.equal(result.control.second.error.code, "ALREADY_USED");
});

test("full or readerless actual pipe produces static failure with no credential retry", async () => {
  const full = await fixture({ selected: api() }, { pipeKind: "full" });
  assert.equal(full.control.error.code, "HANDOFF_FAILED"); assert.equal(full.eofBeforeExit, true);
  assert.ok(full.initialBytes > 0); assert.equal(full.bytes.length, full.initialBytes); assert.ok(full.bytes.every(byte => byte === 120));
  const missingReader = await fixture({ selected: api() }, { pipeKind: "noReader" });
  assert.equal(missingReader.control.error.code, "HANDOFF_FAILED"); assert.equal(missingReader.bytes.length, 0);
  for (const result of [full, missingReader]) {
    assert.ok(!JSON.stringify(result.control).includes("FIXTURE_SELECTED_KEY"));
    assert.deepEqual(Object.keys(result.control.error).sort(), ["code", "message"]);
  }
});
