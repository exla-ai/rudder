import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';

// Independent offline assertions over preserved observations. This does not
// change the failed original wrapper outcome or claim another live execution.
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), 'evidence/2026-10-30-alpha10-netns');
const json = async file => JSON.parse(await fs.readFile(path.join(root, file), 'utf8'));
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const report = await json('report.json'), proof = await json('guest-proof.json');
const manifest = await json('source-manifest.json');
assert.equal(report.proofSha256, sha(await fs.readFile(path.join(root, 'guest-proof.json'))));
assert.equal(report.sourceArchiveSha256, sha(await fs.readFile(path.join(root, 'source.tar.gz'))));
assert.equal(report.sourceManifestSha256, sha(await fs.readFile(path.join(root, 'source-manifest.json'))));
assert.equal(report.configSha256, sha(await fs.readFile(path.join(root, 'machine-config.json'))));
assert.equal(proof.probeSourceSha256, manifest.files['qualification/probe.c'].sha256);
assert.equal(report.expectedDigest, 'sha256:ae1d4bf448c05dc71373efb1b9efd6510dfed4526f76838bc53e1a5c00a446ff');
assert.equal(report.machine.digest, report.expectedDigest);
assert.equal(report.ok, false); assert.equal(proof.ok, false);
assert.equal(report.errorCode, 'ERR_ASSERTION'); assert.equal(proof.errorCode, 'ERR_ASSERTION');
assert.equal(proof.probe.status, 0); assert.equal(proof.probe.signal, null); assert.equal(proof.probe.stderr, '');
assert.equal(proof.result.completed, true); assert.equal(proof.credentialEnvironmentFields, 0);
const events = proof.events;
const one = (kind, stage) => {
  const matches = events.filter(event => event.kind === kind && (stage === undefined || event.stage === stage));
  assert.equal(matches.length, 1); return matches[0];
};
const statuses = events.filter(event => event.kind === 'process-status');
assert.equal(statuses.length, 5);
for (const event of statuses) {
  for (const key of ['uid', 'euid', 'gid', 'egid']) assert.equal(event[key], 1500);
  assert.equal(event.fields.NoNewPrivs, '1');
}
const initial = one('process-status', 'fresh-exec-before-userns');
for (const key of ['CapEff', 'CapPrm', 'CapInh', 'CapAmb', 'CapBnd']) assert.match(initial.fields[key], /^0+$/);
const user = one('process-status', 'in-process-after-userns');
const net = one('process-status', 'in-process-after-newnet');
for (const event of [user, net]) {
  const effective = BigInt('0x' + event.fields.CapEff);
  assert((effective & (1n << 12n)) !== 0n && (effective & (1n << 21n)) !== 0n);
  assert.equal(event.fields.CapPrm, event.fields.CapEff);
  assert.equal(event.fields.CapBnd, event.fields.CapEff);
  assert.match(event.fields.CapInh, /^0+$/); assert.match(event.fields.CapAmb, /^0+$/);
}
for (const stage of ['after-external-exec', 'in-process-after-explicit-cap-drop']) {
  const event = one('process-status', stage);
  for (const key of ['CapEff', 'CapPrm', 'CapInh', 'CapAmb']) assert.match(event.fields[key], /^0+$/);
  assert.equal(event.fields.CapBnd, user.fields.CapBnd);
}
const identity = event => ({device: event.device, inode: event.inode});
const ns = (name, stage) => {
  const matches = events.filter(event => event.kind === 'owned-namespace-fd' && event.namespace === name && event.stage === stage);
  assert.equal(matches.length, 1); assert.equal(matches[0].cloexec, true); return matches[0];
};
const oldUser = ns('user', 'before-userns'), newUser = ns('user', 'after-userns');
const oldNet = ns('net', 'before-userns'), newNet = ns('net', 'after-newnet');
assert.deepEqual(identity(oldUser), proof.parentBefore.user); assert.deepEqual(identity(oldNet), proof.parentBefore.net);
assert.notDeepEqual(identity(oldUser), identity(newUser)); assert.notDeepEqual(identity(oldNet), identity(newNet));
assert.equal(newUser.type, 0x10000000); assert.equal(newNet.type, 0x40000000);
assert.deepEqual(identity(ns('net', 'after-external-exec')), identity(newNet));
const owner = one('netns-owner-witness'); assert.equal(owner.matchesCreatedUserns, true);
assert.deepEqual(identity(owner), identity(newUser));
assert.equal(one('in-process-loopback-up').ok, true);
for (const kind of ['after-exec-loopback-down', 'after-cap-drop-loopback-up']) {
  const event = one(kind); assert.equal(event.result, -1); assert.equal(event.errno, 1); assert.equal(event.expectedEperm, true);
}
const device = one('tun-device'); assert.equal(device.character, true); assert.equal(device.statResult, 0);
assert.equal(device.major, 10); assert.equal(device.minor, 200); assert.equal(device.mode, '0600');
assert.equal(device.uid, 65534); assert.equal(device.gid, 65534);
const opened = one('tun-open'); assert.equal(opened.result, -1); assert.equal(opened.errno, 13);
assert.equal(one('private-tap').attempted, false); assert.equal(one('private-tap').persistRequested, false);
for (const key of ['usernsAvailable', 'newnetAvailable', 'loopbackInProcess', 'afterExecEperm', 'afterCapDropEperm',
  'ownedFdsClosed', 'loopbackRestored']) assert.equal(proof.result[key], true);
assert.equal(proof.result.privateTapCreated, false); assert.equal(proof.result.tunOpenErrno, 13);
assert.equal(report.cleanupVerified, true); assert.equal(report.independentAbsence, true);
const audit = {formatVersion: 1, kind: 'offline-preserved-netns-evidence-audit', assertionsPassed: true,
  originalWrapperPassed: false, originalWrapperFailure: 'Incorrectly required CapBnd=0 after creation of a new user namespace.',
  liveProbeExitCode: proof.probe.status, measuredResult: proof.result, kernel: one('kernel').release,
  capabilityObservation: {initialBounding: initial.fields.CapBnd, createdUserNamespaceBounding: user.fields.CapBnd,
    afterExecEffective: one('process-status', 'after-external-exec').fields.CapEff,
    afterExecBounding: one('process-status', 'after-external-exec').fields.CapBnd},
  missingMeasurements: ['Post-probe parent namespace/status and generated-file removal checks were skipped by the wrapper assertion; canary deletion removed all runtime data.'],
  limitations: ['TUNSETIFF/TAP creation was not attempted because UID1500 open failed EACCES; kernel TUN driver functionality is not established.',
    'No public/private connectivity, egress proxy, packet helper, guest preview, model/provider or default activation was qualified.'],
  sourceArchiveSha256: report.sourceArchiveSha256, guestProofSha256: report.proofSha256,
  verifiedAt: new Date().toISOString()};
await fs.writeFile(path.join(root, 'offline-audit.json'), JSON.stringify(audit, null, 2) + '\n');
console.log(JSON.stringify({assertionsPassed: true, originalWrapperPassed: false,
  newnetAvailable: true, loopbackInProcess: true, tunOpenErrno: 13, privateTapAttempted: false}));
