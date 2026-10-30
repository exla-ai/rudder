import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import http from 'node:http';
import {createHash, randomUUID} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';

const exec = promisify(execFile), directory = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(directory, '../../..');
const frozen = '/workspace/home/.cache/rudder-release-build/alpha9-11f1f75';
const fly = '/workspace/home/.fly/bin/flyctl', app = 'rudder-workers-exla';
const role = 'alpha9-compatibility-refusal', forbidden = '8edd225a7d0378';
const image = 'registry.fly.io/rudder-workers-exla:4.0.0-alpha.9';
const digest = 'sha256:939c5593c5ed1232577d575f7ff382448c466a9033874c94a1a684c4ec5d599a';
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const q = value => "'" + value.replaceAll("'", "'\\''") + "'";
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const emit = value => console.log(JSON.stringify({at: new Date().toISOString(), ...value}));
const ownFiles = ['qualify-alpha9.mjs', 'guest.py', 'probe-ts.mjs'];
const oldFiles = ['engine/src/journal/schema.rs', 'engine/src/journal.rs', 'engine/src/paths.rs',
  'native/src/engine_host.rs', 'native/src/gitio.rs', 'native/src/launch.rs', 'native/src/main.rs',
  'src/engine-authority.ts', 'src/state.ts', 'src/run-manager.ts', 'src/scheduler.ts',
  'src/board/daemon.ts', 'src/backends.ts', 'package.json', 'cloud/worker/Dockerfile'];

if (process.argv[2] === 'prepare') {
  const nonce = randomUUID(), name = 'rudder-alpha9-compat-' + nonce.slice(0, 8);
  const evidence = path.join(directory, 'evidence', '2026-10-30-alpha9-refusal');
  await fs.mkdir(path.dirname(evidence), {recursive: true}); await fs.mkdir(evidence);
  const manifest = {formatVersion: 1, sourceRevision: '11f1f75e5a18555f6c7e257aacc6b4097f425c5e', files: {}};
  for (const [root, prefix, files] of [[directory, 'qualification', ownFiles], [frozen, 'alpha9', oldFiles]]) {
    for (const file of files) {
      const full = path.join(root, file), info = await fs.lstat(full);
      assert(info.isFile() && !info.isSymbolicLink());
      const bytes = await fs.readFile(full); manifest.files[prefix + '/' + file] = {sha256: sha(bytes), bytes: bytes.length};
    }
  }
  await fs.writeFile(path.join(evidence, 'source-sha256.json'), JSON.stringify(manifest, null, 2) + '\n');
  const source = path.join(evidence, 'frozen-source.tar.gz'), payload = path.join(evidence, 'payload.tar.gz');
  await exec('tar', ['-czf', source, '--no-recursion', '-C', directory, ...ownFiles, '-C', frozen, ...oldFiles]);
  await exec('tar', ['-czf', payload, '--no-recursion', '-C', directory, 'guest.py', 'probe-ts.mjs']);
  const state = {nonce, name, image, digest, app, role, evidence, sourceSha256: sha(await fs.readFile(source)),
    payloadSha256: sha(await fs.readFile(payload)), sourceManifestSha256: sha(await fs.readFile(path.join(evidence, 'source-sha256.json')))};
  const statePath = path.join(evidence, 'state.json');
  await fs.writeFile(statePath, JSON.stringify(state, null, 2) + '\n');
  emit({step: 'prepared-no-resources', name, image, digest, statePath});
  process.exit(0);
}
assert.equal(process.argv[2], 'run');
const state = JSON.parse(await fs.readFile(process.argv[3], 'utf8'));
assert.match(state.nonce, /^[0-9a-f-]{36}$/);
assert.equal(state.name, 'rudder-alpha9-compat-' + state.nonce.slice(0, 8));
assert.equal(state.image, image); assert.equal(state.digest, digest); assert.equal(state.app, app); assert.equal(state.role, role);
assert.equal(state.evidence, path.join(directory, 'evidence', '2026-10-30-alpha9-refusal'));
const report = {formatVersion: 1, kind: role, name: state.name, nonce: state.nonce,
  requestedImage: image, expectedDigest: digest, sourceSha256: state.sourceSha256,
  sourceManifestSha256: state.sourceManifestSha256, startedAt: new Date().toISOString(), steps: []};
let owned, server, phase = 'preflight';
async function command(args, label, timeout = 25000) {
  phase = label; const started = Date.now();
  const result = await exec(fly, args, {timeout, maxBuffer: 8 * 1024 * 1024});
  report.steps.push({step: label, ms: Date.now() - started}); return result.stdout;
}
async function machines() { return JSON.parse(await command(['machine', 'list', '-a', app, '--json'], 'filtered-inventory')); }
function matches(machine) { return machine.id !== forbidden && machine.name === state.name
  && machine.config?.metadata?.qualification === state.nonce && machine.config?.metadata?.rudder_role === role; }
async function remote(script, label) {
  assert(owned && owned !== forbidden);
  return command(['machine', 'exec', owned, '-a', app, '/bin/sh -c ' + q(script), '--timeout', '15'], label);
}
try {
  const payload = await fs.readFile(path.join(state.evidence, 'payload.tar.gz'));
  assert.equal(sha(payload), state.payloadSha256);
  assert.equal(sha(await fs.readFile(path.join(state.evidence, 'frozen-source.tar.gz'))), state.sourceSha256);
  assert.deepEqual(JSON.parse(await command(['secrets', 'list', '-a', app, '--json'], 'assert-empty-app-secrets')), []);
  assert(!(await machines()).some(matches));
  const config = {init: {exec: ['/bin/sleep', '1200']}, guest: {cpu_kind: 'shared', cpus: 2, memory_mb: 2048},
    env: {}, services: [], mounts: [], restart: {policy: 'no'},
    metadata: {qualification: state.nonce, rudder_role: role, purpose: 'disposable-schema5-old-client-refusal'}};
  const configPath = path.join(state.evidence, 'machine-config.json');
  await fs.writeFile(configPath, JSON.stringify(config, null, 2) + '\n'); report.requestedConfig = config;
  try {
    const output = await command(['machine', 'run', image, '-a', app, '--machine-config', configPath,
      '--name', state.name, '--region', 'iad', '--detach', '--skip-dns-registration'], 'create-owned-canary', 120000);
    owned = /Machine ID:\s*([0-9a-f]+)/.exec(output)?.[1];
  } catch (error) {
    const candidates = (await machines()).filter(matches);
    if (candidates.length !== 1) throw error;
    owned = candidates[0].id;
  }
  assert(owned && owned !== forbidden); report.machine = owned;
  emit({step: 'created-owned-canary', machine: owned, name: state.name});
  let machine;
  const bootDeadline = Date.now() + 180000;
  while (Date.now() < bootDeadline) {
    machine = (await machines()).find(candidate => candidate.id === owned && matches(candidate));
    assert(machine); if (machine.state === 'started') break; await delay(2000);
  }
  assert.equal(machine.state, 'started'); assert.equal(machine.image_ref.digest, digest);
  assert(!machine.config.services?.length && !machine.config.mounts?.length);
  assert(Object.keys(machine.config.env || {}).every(key => ['PRIMARY_REGION', 'FLY_PRIMARY_REGION'].includes(key)));
  report.instance = machine.instance_id; report.region = machine.region;
  report.image = {registry: machine.image_ref.registry, repository: machine.image_ref.repository,
    tag: machine.image_ref.tag, digest: machine.image_ref.digest};
  const address = Object.values(os.networkInterfaces()).flat()
    .find(value => value?.family === 'IPv6' && value.address.startsWith('fdaa:'))?.address;
  assert(address);
  server = http.createServer((req, res) => {
    if (req.method !== 'GET' || req.url !== '/' + state.nonce + '/payload.tar.gz'
      || req.socket.remoteAddress !== machine.private_ip) { res.writeHead(404); res.end(); return; }
    res.writeHead(200, {'Content-Length': payload.length, Connection: 'close'}); res.end(payload);
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, address, resolve); });
  const url = `http://[${address}]:${server.address().port}/${state.nonce}/payload.tar.gz`;
  const download = `const r=await fetch(${JSON.stringify(url)});if(!r.ok)throw Error('fixture download');const b=Buffer.from(await r.arrayBuffer());if(crypto.createHash('sha256').update(b).digest('hex')!==${JSON.stringify(state.payloadSha256)})throw Error('fixture hash');fs.writeFileSync('/tmp/alpha9-compat.tar.gz',b);`;
  await remote('/usr/local/bin/node --input-type=module -e ' + q("import fs from 'node:fs';import crypto from 'node:crypto';" + download), 'private-source-download');
  const fixture = '/workspace/alpha9-compat-' + state.nonce;
  await remote(`mkdir -m755 /tmp/alpha9-compat-src && tar -xzf /tmp/alpha9-compat.tar.gz -C /tmp/alpha9-compat-src && chmod 644 /tmp/alpha9-compat-src/* && install -d -m700 -o1500 -g1500 ${q(fixture)}`, 'prepare-owned-fixture');
  const launch = `(timeout 120 runuser -u rudder -- /usr/bin/env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=/nonexistent RUDDER_HOME=/nonexistent /usr/bin/python3 /tmp/alpha9-compat-src/guest.py ${q(fixture)}; result=$?; echo "$result" >/tmp/alpha9-compat-done) >/tmp/alpha9-compat-output 2>&1 </dev/null & echo LAUNCHED`;
  assert((await remote(launch, 'launch-owned-qualification')).includes('LAUNCHED'));
  const deadline = Date.now() + 140000;
  let finished = false;
  while (Date.now() < deadline) {
    const out = await remote('if test -f /tmp/alpha9-compat-done; then cat /tmp/alpha9-compat-done; else echo PENDING; fi', 'poll-owned-qualification');
    if (!out.includes('PENDING')) { report.exitCode = Number(out.trim()); finished = true; break; }
    await delay(1500);
  }
  assert(finished);
  const encoded = (await remote('/usr/bin/base64 -w0 /tmp/alpha9-compat-output', 'retrieve-generated-results')).trim();
  assert(encoded && /^[A-Za-z0-9+/]+=*$/.test(encoded));
  const output = Buffer.from(encoded, 'base64').toString('utf8');
  await fs.writeFile(path.join(state.evidence, 'guest-output.log'), output);
  const result = output.split('\n').filter(line => line.startsWith('{')).map(line => JSON.parse(line)).at(-1);
  report.result = result;
  assert.equal(report.exitCode, 0); assert(result?.ok && result.fixtureAbsent && result.uid === 1500);
  report.ok = true;
} catch (error) {
  report.ok = false; report.failedPhase = phase; report.error = 'qualification failed at ' + phase;
  report.errorCode = error.code || 'assertion'; process.exitCode = 1; emit({step: 'failed', phase});
} finally {
  if (server) { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }
  try {
    for (const machine of (await machines()).filter(matches)) {
      await command(['machine', 'destroy', machine.id, '-a', app, '--force'], 'destroy-owned-canary');
    }
    const remaining = await machines();
    report.cleanupVerified = !remaining.some(matches) && !remaining.some(machine => machine.id === owned);
    // A second independent CLI invocation checks all ownership witnesses again.
    const independent = await machines();
    report.independentAbsence = !independent.some(matches) && !independent.some(machine => machine.id === owned);
    assert(report.cleanupVerified && report.independentAbsence);
  } catch { report.cleanupVerified = false; process.exitCode = 1; }
  report.verifiedAt = new Date().toISOString();
  await fs.writeFile(path.join(state.evidence, 'report.json'), JSON.stringify(report, null, 2) + '\n');
  const entries = (await fs.readdir(state.evidence)).filter(file => file !== 'SHA256SUMS').sort();
  const sums = [];
  for (const file of entries) { const full = path.join(state.evidence, file);
    if ((await fs.lstat(full)).isFile()) sums.push(sha(await fs.readFile(full)) + '  ' + file); }
  await fs.writeFile(path.join(state.evidence, 'SHA256SUMS'), sums.join('\n') + '\n');
  emit({step: 'finished', ok: report.ok, cleanupVerified: report.cleanupVerified,
    independentAbsence: report.independentAbsence, report: path.join(state.evidence, 'report.json')});
}
