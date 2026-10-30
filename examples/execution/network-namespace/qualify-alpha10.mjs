import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
import {createHash, randomUUID} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {fileURLToPath} from 'node:url';

const exec = promisify(execFile), directory = path.dirname(fileURLToPath(import.meta.url));
const fly = '/workspace/home/.fly/bin/flyctl', app = 'rudder-workers-exla';
const role = 'alpha10-private-netns-tun', forbidden = '8edd225a7d0378';
const imageTag = 'registry.fly.io/rudder-workers-exla:4.0.0-alpha.10';
const digest = 'sha256:ae1d4bf448c05dc71373efb1b9efd6510dfed4526f76838bc53e1a5c00a446ff';
const image = 'registry.fly.io/rudder-workers-exla@' + digest;
const revision = '56f825ea3c1d6136b1a20b3031f835ef33f870cd';
const frozen = '/workspace/home/.cache/rudder-release-build/alpha10-56f825e';
const repo = path.resolve(directory, '../../..');
const evidence = path.join(directory, 'evidence', '2026-10-30-alpha10-netns');
const ownFiles = ['probe.c', 'preload.mjs', 'qualify-alpha10.mjs'];
const frozenFiles = ['cloud/worker/root-bootstrap.sh', 'cloud/worker/entrypoint.sh',
  'cloud/worker/cache-env.sh', 'cloud/worker/Dockerfile'];
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const emit = value => console.log(JSON.stringify({at: new Date().toISOString(), ...value}));

async function sourceManifest() {
  const files = {};
  for (const [root, prefix, names] of [[directory, 'qualification', ownFiles], [frozen, 'alpha10', frozenFiles]]) {
    for (const name of names) {
      const full = path.join(root, name), stat = await fs.lstat(full);
      assert(stat.isFile() && !stat.isSymbolicLink() && stat.size < 128 * 1024);
      const bytes = await fs.readFile(full);
      if (root === frozen) {
        const tagged = await exec('git', ['show', revision + ':' + name],
          {cwd: repo, encoding: 'buffer', timeout: 10_000, maxBuffer: 128 * 1024});
        assert.deepEqual(bytes, tagged.stdout, 'frozen source must match exact alpha10 revision');
      }
      files[prefix + '/' + name] = {sha256: sha(bytes), bytes: bytes.length};
    }
  }
  return {formatVersion: 1, revision, imageTag, expectedDigest: digest, files};
}

if (process.argv[2] === 'prepare') {
  const nonce = randomUUID(), name = 'rudder-alpha10-netns-' + nonce.slice(0, 8);
  await fs.mkdir(path.dirname(evidence), {recursive: true}); await fs.mkdir(evidence);
  const manifest = await sourceManifest();
  await fs.writeFile(path.join(evidence, 'source-manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
  await exec('tar', ['-czf', path.join(evidence, 'source.tar.gz'), '--no-recursion',
    '-C', directory, ...ownFiles, '-C', frozen, ...frozenFiles], {timeout: 10_000, maxBuffer: 64 * 1024});
  const source = await fs.readFile(path.join(directory, 'probe.c'));
  const preload = 'globalThis.__RUDDER_NETNS_QUALIFICATION=' + JSON.stringify({nonce,
    sourceBase64: source.toString('base64'), sourceSha256: sha(source)}) + ';\n'
    + await fs.readFile(path.join(directory, 'preload.mjs'), 'utf8');
  const config = {image, guest: {cpu_kind: 'shared', cpus: 2, memory_mb: 2048},
    restart: {policy: 'no'}, auto_destroy: false, services: [], mounts: [],
    env: {RUDDER_WORKER_MANAGED_EXECUTION: '1', RUDDER_HOME: '1', RUDDER_WORKER_SKIP_AGENT_UPDATE: '1',
      NODE_OPTIONS: '--import=data:text/javascript;base64,' + Buffer.from(preload).toString('base64')},
    metadata: {rudder_role: role, qualification: nonce, purpose: 'owned-process-only-user-newnet-private-tap'}};
  await fs.writeFile(path.join(evidence, 'machine-config.json'), JSON.stringify(config, null, 2) + '\n');
  const state = {nonce, name, app, role, image, imageTag, digest, revision,
    sourceManifestSha256: sha(await fs.readFile(path.join(evidence, 'source-manifest.json'))),
    sourceArchiveSha256: sha(await fs.readFile(path.join(evidence, 'source.tar.gz'))),
    configSha256: sha(await fs.readFile(path.join(evidence, 'machine-config.json')))};
  await fs.writeFile(path.join(evidence, 'state.json'), JSON.stringify(state, null, 2) + '\n');
  emit({step: 'prepared-no-resources', name, nonce, role, image, state: path.join(evidence, 'state.json')});
  process.exit(0);
}

assert.equal(process.argv[2], 'run');
const state = JSON.parse(await fs.readFile(path.join(evidence, 'state.json'), 'utf8'));
assert.match(state.nonce, /^[0-9a-f-]{36}$/);
assert.equal(state.name, 'rudder-alpha10-netns-' + state.nonce.slice(0, 8));
assert.equal(state.app, app); assert.equal(state.role, role); assert.equal(state.image, image);
assert.equal(state.imageTag, imageTag); assert.equal(state.digest, digest); assert.equal(state.revision, revision);
assert.deepEqual(await sourceManifest(), JSON.parse(await fs.readFile(path.join(evidence, 'source-manifest.json'), 'utf8')));
for (const [file, expected] of [['source-manifest.json', state.sourceManifestSha256],
  ['source.tar.gz', state.sourceArchiveSha256], ['machine-config.json', state.configSha256]]) {
  assert.equal(sha(await fs.readFile(path.join(evidence, file))), expected);
}
const report = {formatVersion: 1, kind: role, revision, requestedImageTag: imageTag, requestedImage: image,
  expectedDigest: digest, nonce: state.nonce, name: state.name, sourceManifestSha256: state.sourceManifestSha256,
  sourceArchiveSha256: state.sourceArchiveSha256, configSha256: state.configSha256,
  startedAt: new Date().toISOString(), steps: [], restrictions: [
    'No product source edits or product builds. Only the small archived C qualification program is compiled.',
    'No services, volumes, app secrets, account/provider/workspace credentials, supervisor module evaluation or models.',
    'No connect/bind/listen/setns, host listener scan, external network target or persistent TUN interface.',
    'Only current-process namespace/capability metadata, /dev/net/tun and newly owned lo/TAP are probed.',
    'This qualifies one kernel/image/UID execution only, not general egress, preview routing or default managed networking.'
  ]};
let owned, phase = 'preflight';
function matches(machine) {
  return machine.id !== forbidden && machine.name === state.name
    && machine.config?.metadata?.qualification === state.nonce && machine.config?.metadata?.rudder_role === role;
}
async function command(args, label, timeout = 25_000) {
  phase = label; const start = Date.now();
  const result = await exec(fly, args, {timeout, maxBuffer: 1024 * 1024});
  report.steps.push({step: label, ms: Date.now() - start}); return result.stdout;
}
async function machines() { return JSON.parse(await command(['machine', 'list', '-a', app, '--json'], 'filtered-inventory')); }
try {
  assert.deepEqual(JSON.parse(await command(['secrets', 'list', '-a', app, '--json'], 'assert-no-app-secrets')), []);
  const before = await machines();
  assert(!before.some(machine => machine.name === state.name || machine.config?.metadata?.qualification === state.nonce));
  report.credentialIsolationPreflight = {appSecrets: 0, configuredCredentials: 0,
    supervisorBlockedByPreload: true, updatesDisabled: true, noServices: true, noMounts: true};
  emit({step: 'creating-owned-canary', name: state.name, role, nonce: state.nonce, image});
  try {
    const output = await command(['machine', 'run', image, '-a', app, '--machine-config',
      path.join(evidence, 'machine-config.json'), '--name', state.name, '--region', 'sjc', '--detach',
      '--skip-dns-registration'], 'create-owned-canary', 120_000);
    owned = /Machine ID:\s*([0-9a-f]+)/.exec(output)?.[1];
  } catch {
    report.createReplyUncertain = true;
  }
  const deadline = Date.now() + 180_000;
  let machine, startRequested = false;
  while (Date.now() < deadline) {
    const candidates = (await machines()).filter(matches); assert(candidates.length <= 1);
    if (candidates.length) {
      machine = candidates[0];
      if (owned) assert.equal(machine.id, owned);
      owned = machine.id; assert(owned !== forbidden);
      if (machine.state === 'started') break;
      if (machine.state === 'created' && !startRequested) {
        startRequested = true;
        try { await command(['machine', 'start', owned, '-a', app], 'start-reconciled-owned-canary', 90_000); }
        catch { report.startReplyUncertain = true; }
      }
    }
    await delay(2000);
  }
  assert(machine && matches(machine)); assert.equal(machine.state, 'started');
  assert.equal(machine.image_ref.digest, digest);
  assert(!machine.config.mounts?.length && !machine.config.services?.length);
  const allowed = ['RUDDER_WORKER_MANAGED_EXECUTION', 'RUDDER_HOME', 'RUDDER_WORKER_SKIP_AGENT_UPDATE',
    'NODE_OPTIONS', 'PRIMARY_REGION', 'FLY_PRIMARY_REGION'];
  assert(Object.keys(machine.config.env || {}).every(key => allowed.includes(key)));
  const requested = JSON.parse(await fs.readFile(path.join(evidence, 'machine-config.json'), 'utf8'));
  for (const [key, value] of Object.entries(requested.env)) assert.equal(machine.config.env[key], value);
  report.machine = {id: owned, name: machine.name, instance: machine.instance_id, region: machine.region,
    digest: machine.image_ref.digest, role, nonce: state.nonce, services: 0, mounts: 0};
  emit({step: 'owned-image-verified', machine: owned, name: machine.name, digest});
  let proof;
  for (let i = 0; i < 35; i++) {
    try {
      const output = await command(['machine', 'exec', owned, '-a', app,
        '/bin/cat /workspace/netns-proof.json', '--timeout', '10'], 'read-generated-proof', 20_000);
      assert(output.length < 256 * 1024); proof = JSON.parse(output.trim()); break;
    } catch { await delay(2000); }
  }
  assert(proof && proof.nonce === state.nonce);
  await fs.writeFile(path.join(evidence, 'guest-proof.json'), JSON.stringify(proof, null, 2) + '\n');
  report.proofSha256 = sha(await fs.readFile(path.join(evidence, 'guest-proof.json')));
  assert.equal(proof.ok, true); assert.equal(proof.result.completed, true);
  assert.equal(proof.supervisorModuleEvaluated, false); assert.equal(proof.credentialEnvironmentFields, 0);
  assert.equal(proof.parentNamespacesUnchanged, true); assert.equal(proof.probeRuntimeRemoved, true);
  report.result = proof.result; report.ok = true;
} catch (error) {
  report.ok = false; report.failedPhase = phase; report.error = 'qualification failed at ' + phase;
  report.errorCode = typeof error?.code === 'string' ? error.code : 'ASSERTION'; process.exitCode = 1;
  emit({step: 'qualification-failed', phase});
} finally {
  try {
    const candidates = (await machines()).filter(matches);
    assert(candidates.length <= 1);
    for (const machine of candidates) {
      if (owned) assert.equal(machine.id, owned);
      owned = machine.id;
      await command(['machine', 'destroy', owned, '-a', app, '--force'], 'destroy-exact-owned-canary');
    }
    const absent = inventory => !inventory.some(machine => machine.id === owned || machine.name === state.name
      || machine.config?.metadata?.qualification === state.nonce);
    report.cleanupVerified = absent(await machines());
    report.independentAbsence = absent(await machines());
    assert(report.cleanupVerified && report.independentAbsence);
    await fs.writeFile(path.join(evidence, 'cleanup.json'), JSON.stringify({formatVersion: 1,
      machine: owned, name: state.name, nonce: state.nonce, role, deleted: true,
      firstInventoryAbsent: report.cleanupVerified, secondIndependentInventoryAbsent: report.independentAbsence,
      verifiedAt: new Date().toISOString()}, null, 2) + '\n');
  } catch { report.cleanupVerified = false; process.exitCode = 1; }
  report.verifiedAt = new Date().toISOString();
  await fs.writeFile(path.join(evidence, 'report.json'), JSON.stringify(report, null, 2) + '\n');
  emit({step: 'finished', ok: report.ok, cleanupVerified: report.cleanupVerified,
    independentAbsence: report.independentAbsence, report: path.join(evidence, 'report.json')});
}
