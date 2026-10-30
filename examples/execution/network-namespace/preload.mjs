import fs from 'node:fs';
import cp from 'node:child_process';
import crypto from 'node:crypto';
import assert from 'node:assert/strict';

// This image-owned Node preload never returns to evaluate home/supervisor.mjs.
// Only generated qualification files and /proc/self metadata are inspected.
const input = globalThis.__RUDDER_NETNS_QUALIFICATION;
const output = '/workspace/netns-proof.json';
const sha = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const report = {formatVersion: 1, kind: 'alpha10-private-netns-tun', nonce: input.nonce,
  supervisorModuleEvaluated: false, modelCalls: false, hostSocketConnections: false,
  namespaceScope: 'fresh UID1500 probe process and its newly owned user/network namespaces'};
const fields = () => {
  const text = fs.readFileSync('/proc/self/status', 'utf8');
  return Object.fromEntries(['Uid', 'Gid', 'Groups', 'NoNewPrivs', 'CapEff', 'CapPrm', 'CapInh', 'CapAmb', 'CapBnd']
    .map(key => [key, new RegExp('^' + key + ':[ \\t]*([^\\r\\n]*)$', 'm').exec(text)?.[1]?.trim()]));
};
const witness = file => {
  const fd = fs.openSync(file, fs.constants.O_RDONLY | fs.constants.O_CLOEXEC);
  const stat = fs.fstatSync(fd, {bigint: true});
  return {fd, device: stat.dev.toString(), inode: stat.ino.toString()};
};
const identity = value => ({device: value.device, inode: value.inode});
const runtime = '/workspace/netns-qualification-' + input.nonce;
const env = {PATH: '/usr/bin:/bin', HOME: runtime, TMPDIR: runtime, LANG: 'C', LC_ALL: 'C'};
const run = (program, args, timeout) => cp.spawnSync(program, args,
  {cwd: runtime, env, timeout, encoding: 'utf8', maxBuffer: 256 * 1024});
let parentUser, parentNet;
try {
  assert.match(input.nonce, /^[0-9a-f-]{36}$/);
  // Refuse credential-bearing configured environment without logging names or values.
  const credentialFields = Object.keys(process.env).filter(key => /TOKEN|SECRET|PASSWORD|API_KEY|CREDENTIAL|AUTH_KEY/i.test(key));
  assert.equal(credentialFields.length, 0, 'credential isolation admission failed');
  report.credentialEnvironmentFields = credentialFields.length;
  assert.equal(process.getuid(), 1500); assert.equal(process.getgid(), 1500);
  const initial = fields();
  assert.equal(initial.Uid, '1500\t1500\t1500\t1500');
  assert.equal(initial.Gid, '1500\t1500\t1500\t1500');
  assert.equal(initial.Groups, ''); assert.equal(initial.NoNewPrivs, '1');
  for (const key of ['CapEff', 'CapPrm', 'CapInh', 'CapAmb', 'CapBnd']) assert.match(initial[key], /^0+$/);
  assert.equal(process.env.RUDDER_WORKER_MANAGED_EXECUTION, '1');
  assert.equal(process.env.RUDDER_WORKER_SKIP_AGENT_UPDATE, '1');
  assert.equal(JSON.parse(fs.readFileSync('/opt/rudder/package.json', 'utf8')).version, '4.0.0-alpha.10');
  report.boot = {uid: process.getuid(), gid: process.getgid(), initial, node: process.version,
    managedBootstrap: true, agentUpdatesDisabled: true, packageVersion: '4.0.0-alpha.10'};
  parentUser = witness('/proc/self/ns/user'); parentNet = witness('/proc/self/ns/net');
  report.parentBefore = {user: identity(parentUser), net: identity(parentNet)};
  fs.mkdirSync(runtime, {mode: 0o700});
  const source = Buffer.from(input.sourceBase64, 'base64');
  assert.equal(sha(source), input.sourceSha256); report.probeSourceSha256 = input.sourceSha256;
  fs.writeFileSync(runtime + '/probe.c', source, {mode: 0o600, flag: 'wx'});
  const version = run('/usr/bin/cc', ['--version'], 10_000);
  assert.equal(version.status, 0); report.compiler = version.stdout.split('\n')[0];
  const flags = ['-std=c11', '-O2', '-Wall', '-Wextra', '-Werror', '-o', runtime + '/probe', runtime + '/probe.c'];
  const compiled = run('/usr/bin/cc', flags, 30_000);
  report.compile = {status: compiled.status, signal: compiled.signal, flags};
  assert.equal(compiled.status, 0, 'qualification-only C program compilation failed');
  fs.chmodSync(runtime + '/probe', 0o700);
  report.probeBinarySha256 = sha(fs.readFileSync(runtime + '/probe'));
  const probe = run(runtime + '/probe', [], 25_000);
  report.probe = {status: probe.status, signal: probe.signal, stdoutSha256: sha(probe.stdout || ''),
    stderr: (probe.stderr || '').slice(0, 4096)};
  assert.equal(probe.status, 0, 'bounded namespace qualification failed');
  report.events = probe.stdout.trim().split('\n').map(line => JSON.parse(line));
  const result = report.events.filter(event => event.kind === 'qualification-result');
  assert.equal(result.length, 1); assert.equal(result[0].completed, true); report.result = result[0];
  const statuses = report.events.filter(event => event.kind === 'process-status');
  for (const event of statuses) {
    assert.equal(event.uid, 1500); assert.equal(event.euid, 1500);
    assert.equal(event.gid, 1500); assert.equal(event.egid, 1500);
    assert.equal(event.fields.NoNewPrivs, '1'); assert.match(event.fields.CapBnd, /^0+$/);
  }
  if (report.result.newnetAvailable) {
    for (const stage of ['fresh-exec-before-userns', 'after-external-exec', 'in-process-after-explicit-cap-drop']) {
      const event = statuses.find(value => value.stage === stage); assert(event);
      for (const key of ['CapEff', 'CapPrm', 'CapInh', 'CapAmb']) assert.match(event.fields[key], /^0+$/);
    }
    for (const stage of ['in-process-after-userns', 'in-process-after-newnet']) {
      const event = statuses.find(value => value.stage === stage); assert(event);
      const effective = BigInt('0x' + event.fields.CapEff);
      assert((effective & (1n << 21n)) !== 0n); // CAP_SYS_ADMIN, scoped to new userns.
      assert((effective & (1n << 12n)) !== 0n); // CAP_NET_ADMIN, scoped to new userns.
    }
    const before = report.events.find(value => value.kind === 'owned-namespace-fd' && value.namespace === 'net' && value.stage === 'before-userns');
    const inner = report.events.find(value => value.kind === 'owned-namespace-fd' && value.namespace === 'net' && value.stage === 'after-newnet');
    const afterExec = report.events.find(value => value.kind === 'owned-namespace-fd' && value.namespace === 'net' && value.stage === 'after-external-exec');
    assert.deepEqual(identity(before), report.parentBefore.net);
    assert.notDeepEqual(identity(inner), report.parentBefore.net);
    assert.deepEqual(identity(inner), identity(afterExec));
  }
  const afterUser = witness('/proc/self/ns/user'), afterNet = witness('/proc/self/ns/net');
  report.parentAfter = {user: identity(afterUser), net: identity(afterNet)};
  fs.closeSync(afterUser.fd); fs.closeSync(afterNet.fd);
  assert.deepEqual(report.parentAfter, report.parentBefore); report.parentNamespacesUnchanged = true;
  report.parentStatusAfter = fields(); assert.deepEqual(report.parentStatusAfter, initial);
  fs.unlinkSync(runtime + '/probe'); fs.unlinkSync(runtime + '/probe.c'); fs.rmdirSync(runtime);
  assert(!fs.existsSync(runtime)); report.probeRuntimeRemoved = true;
  report.ok = true;
} catch (error) {
  report.ok = false; report.error = 'qualification assertion failed';
  report.errorCode = typeof error?.code === 'string' ? error.code : 'ASSERTION';
  // Assertion messages, subprocess objects and environment never enter logs.
} finally {
  if (parentUser) fs.closeSync(parentUser.fd);
  if (parentNet) fs.closeSync(parentNet.fd);
  report.verifiedAt = new Date().toISOString();
  fs.writeFileSync(output, JSON.stringify(report, null, 2) + '\n', {mode: 0o600, flag: 'wx'});
  console.log(JSON.stringify({kind: report.kind, ok: report.ok, completed: report.result?.completed === true}));
}
setTimeout(() => process.exit(0), 480_000);
await new Promise(() => {});
