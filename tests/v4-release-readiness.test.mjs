import assert from 'node:assert/strict';
import test from 'node:test';
import fs from 'node:fs/promises';
import path from 'node:path';
import { atomicJson, digestFile, evaluate, verifyReceipt, testCounts, isCandidateInput } from '../scripts/v4-release-readiness/readiness.mjs';
import { checks, gates } from '../scripts/v4-release-readiness/catalog.mjs';
import { v4ScratchPrefix } from './helpers/v4-scratch.mjs';
import { validateMacHost, validateMacManifest, validateMacAppProof, qualifyMac } from '../scripts/v4-release-readiness/mac-qualify.mjs';

const candidate = { version: 1, revision: 'a'.repeat(40), tree_sha256: 'b'.repeat(64) };
async function fixture(t) {
  const root = await fs.mkdtemp(v4ScratchPrefix('readiness'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  await fs.writeFile(path.join(root, 'observation.log'), 'test-only fixture; no real qualification\n');
  const receipt = { version: 1, source: { ...candidate },
    started_at: '2026-11-04T00:00:00Z', finished_at: '2026-11-04T00:01:00Z',
    result: 'passed', exit_code: 0, platform: { os: 'linux', arch: 'x64', kernel: 'fixture-kernel' },
    requirements: ['cache-soundness'], scope: 'local-process', samples: 1, failed: 0, skipped: 0,
    review: { owner: 'fixture-reviewer', accepted: true, limits: 'Synthetic unit test only; never persist as actual evidence.' },
    artifacts: [{ path: 'observation.log', ...await digestFile(path.join(root, 'observation.log')) }] };
  return { root, receipt };
}

test('all E0-E14 requirements fail closed with no evidence', async t => {
  const { root } = await fixture(t);
  const report = await evaluate(candidate, [], root);
  assert.equal(report.ready, false);
  assert.deepEqual(report.gates.map(g => g.id), Array.from({ length: 15 }, (_, i) => `E${i}`));
  assert(report.gates.every(g => g.status === 'blocked' && g.missing.length));
  assert(report.delivery_missing.includes('registry-install'));
});

test('a passed real helper test cannot qualify Root, runtime, Fly, models or complete v4', async t => {
  const { root, receipt } = await fixture(t);
  Object.assign(receipt, { check: 'sdk-local', command: checks['sdk-local'].command,
    scope: 'local-helper', requirements: [], tests: { passed: 1, failed: 0, skipped: 0 } });
  const report = await evaluate(candidate, [receipt], root);
  assert.deepEqual(report.observations, ['sdk-local']); assert.deepEqual(report.rejected, []);
  assert.equal(report.ready, false); assert(report.gates.every(g => g.status === 'blocked'));
  receipt.requirements = ['managed-sdk-runtime'];
  await assert.rejects(verifyReceipt(receipt, candidate, root), /fixture cannot qualify/);
});

test('source changes, failed runs, skipped cases and absent human review cannot qualify', async t => {
  const { root, receipt } = await fixture(t);
  for (const mutation of [
    { source: { ...candidate, revision: 'c'.repeat(40) } },
    { source: { ...candidate, tree_sha256: 'c'.repeat(64) } },
    { result: 'pending' }, { exit_code: 1 }, { skipped: 1 }, { failed: 1 }, { samples: 0 },
    { review: { ...receipt.review, accepted: false } }, { review: { ...receipt.review, limits: '' } },
    { finished_at: '2026-11-03T00:00:00Z' }, { finished_at: 'yesterday' },
  ]) await assert.rejects(verifyReceipt({ ...receipt, ...mutation }, candidate, root));
});

test('receipt bytes must match a contained regular evidence file', async t => {
  const { root, receipt } = await fixture(t);
  for (const name of ['../observation.log', '/observation.log', './observation.log']) {
    await assert.rejects(verifyReceipt({ ...receipt, artifacts: [{ ...receipt.artifacts[0], path: name }] }, candidate, root));
  }
  await fs.symlink('observation.log', path.join(root, 'alias.log'));
  await assert.rejects(verifyReceipt({ ...receipt, artifacts: [{ ...receipt.artifacts[0], path: 'alias.log' }] }, candidate, root));
  await fs.writeFile(path.join(root, 'observation.log'), 'substituted');
  await assert.rejects(verifyReceipt(receipt, candidate, root), /digest\/size mismatch/);
});

test('two local processes or a reused volume cannot stand in for two workers', async t => {
  const { root, receipt } = await fixture(t);
  receipt.requirements = ['two-worker-distribution'];
  await assert.rejects(verifyReceipt(receipt, candidate, root), /wrong scope/);
  receipt.scope = 'two-fly-workers';
  await assert.rejects(verifyReceipt(receipt, candidate, root), /independent/);
  receipt.workers = [{ machine: 'a', volume: 'v' }, { machine: 'a', volume: 'v', initially_empty: true }];
  await assert.rejects(verifyReceipt(receipt, candidate, root), /shared machine/);
  receipt.workers[1] = { machine: 'b', volume: 'different', initially_empty: true };
  await verifyReceipt(receipt, candidate, root);
});

test('Linux and Rosetta observations cannot pass both physical Mac architecture gates', async t => {
  const { root, receipt } = await fixture(t);
  receipt.requirements = ['mac-install-arm64'];
  await assert.rejects(verifyReceipt(receipt, candidate, root), /Darwin/);
  receipt.platform.os = 'darwin';
  receipt.platform.hardware_arch = 'x64'; receipt.platform.translated = false;
  await assert.rejects(verifyReceipt(receipt, candidate, root), /arm64/);
  receipt.platform.arch = 'arm64';
  receipt.platform.hardware_arch = 'arm64';
  await verifyReceipt(receipt, candidate, root);
  receipt.requirements.push('mac-install-x64');
  await assert.rejects(verifyReceipt(receipt, candidate, root), /Intel/);
});

test('Mac observations cannot qualify Linux baseline, fork, filesystem or VCS requirements', async t => {
  const { root, receipt } = await fixture(t);
  receipt.requirements = ['baseline-linux', 'fork-linux', 'filesystem-linux', 'vcs-linux'];
  await verifyReceipt(receipt, candidate, root);
  receipt.platform.os = 'darwin';
  for (const requirement of receipt.requirements) {
    await assert.rejects(verifyReceipt({ ...receipt, requirements: [requirement] }, candidate, root), /Linux proof requires Linux/);
  }
});

test('supported observations do not imply registry publication or Cloud deployment', async t => {
  const { root, receipt } = await fixture(t);
  const report = await evaluate(candidate, [receipt], root);
  assert.deepEqual(report.rejected, []);
  assert(!report.gates.find(g => g.id === 'E3').missing.includes('cache-soundness'));
  assert(report.gates.find(g => g.id === 'E3').missing.includes('managed-sdk-runtime'));
  assert(report.delivery_missing.includes('registry-install'));
  assert(report.delivery_missing.includes('cloud-deployment'));
});

test('test summaries refuse zero tests and retain skipped/failed counts', () => {
  assert.deepEqual(testCounts('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;'), { passed: 1, failed: 0, skipped: 0 });
  assert.deepEqual(testCounts('# pass 0\n# fail 0\n# skipped 1\n'), { passed: 0, failed: 0, skipped: 1 });
  assert.deepEqual(testCounts('test result: FAILED. 2 passed; 1 failed; 3 ignored;'), { passed: 2, failed: 1, skipped: 3 });
  assert.equal(testCounts('build finished'), null);
});

test('fixture storage works on Macs without creating /workspace or leaking shared parent dirs', () => {
  assert.equal(v4ScratchPrefix('test', 'darwin', false, '/private/tmp'), '/private/tmp/rudder-test-');
  assert.equal(v4ScratchPrefix('test', 'linux', true, '/tmp'), '/workspace/rudder-test-');
  assert.equal(v4ScratchPrefix('test', 'linux', false, '/tmp'), '/tmp/rudder-test-');
});

test('Mac runner refuses Linux, unsupported OS and translated architecture before installation', async () => {
  validateMacHost({ platform: 'darwin', arch: 'arm64', hardwareArm: true, translated: false, major: 14 });
  validateMacHost({ platform: 'darwin', arch: 'x64', hardwareArm: false, translated: false, major: 15 });
  for (const override of [{ platform: 'linux' }, { arch: 'x64' }, { translated: true }, { major: 13 }]) {
    assert.throws(() => validateMacHost({ platform: 'darwin', arch: 'arm64', hardwareArm: true, translated: false, major: 14, ...override }));
  }
  if (process.platform === 'linux') await assert.rejects(qualifyMac({ tarball: '/absent.tgz', version: '4.0.0', revision: 'a'.repeat(40), expectedArch: process.arch, output: '/absent' }), /cannot run on Linux/);
});

test('Mac probes require both candidate-bound native executable digests', () => {
  const revision = 'a'.repeat(40), platform = 'darwin-arm64';
  const manifest = {formatVersion:1,sourceRevision:revision,platform,buildProfile:'release',features:['rudder-vcs-bridge/cli'],
    files:{'rudder-native':{sha256:'b'.repeat(64),size:1},'rudder-jj':{sha256:'c'.repeat(64),size:2}}};
  validateMacManifest(manifest,platform,revision);
  for (const change of [{files:{}},{sourceRevision:'d'.repeat(40)},{platform:'darwin-x64'},
    {files:{...manifest.files,'rudder-jj':undefined}},{files:{...manifest.files,'rudder-jj':{size:2}}}]) {
    assert.throws(()=>validateMacManifest({...manifest,...change},platform,revision));
  }
});

test('Mac app probes require reviewed exact-source CI provenance and executable/plist identities', () => {
  const revision = 'a'.repeat(40);
  const proof = { version: 1, source_revision: revision,
    workflow_run: 'https://github.com/exla-ai/rudder/actions/runs/123', artifact_id: 'fixture-only',
    app_version: '1.0.2', app_build: '3', review: { accepted: true, owner: 'fixture-reviewer' },
    files: { 'Contents/MacOS/Rudder': { sha256: 'b'.repeat(64), bytes: 1 },
      'Contents/Info.plist': { sha256: 'c'.repeat(64), bytes: 2 } } };
  validateMacAppProof(proof, revision);
  for (const change of [{ source_revision: 'd'.repeat(40) }, { workflow_run: '' }, { artifact_id: '' },
    { app_version: '' }, { app_build: '' }, { review: { accepted: false, owner: 'fixture-reviewer' } },
    { files: {} }, { files: { ...proof.files, 'Contents/Info.plist': { bytes: 2 } } }]) {
    assert.throws(() => validateMacAppProof({ ...proof, ...change }, revision));
  }
});

test('product/test/release inputs invalidate evidence; delivery prose does not', () => {
  for (const name of ['src/main.ts', 'native/src/main.rs', 'tests/new.test.mjs', 'scripts/release-reuse.mjs', 'Cargo.lock', '.github/workflows/release.yml']) assert(isCandidateInput(name));
  for (const name of ['docs/architecture/v4-release-readiness.md', 'RUDDER.md', 'DECISIONS.md']) assert(!isCandidateInput(name));
  assert.equal(gates.length, 15);
});

test('atomic evidence writes leave one complete private JSON document', async t => {
  const { root } = await fixture(t), file = path.join(root, 'receipt.json');
  await atomicJson(file, { ready: false });
  assert.deepEqual(JSON.parse(await fs.readFile(file, 'utf8')), { ready: false });
  assert.equal((await fs.stat(file)).mode & 0o777, 0o600);
  assert.deepEqual((await fs.readdir(root)).sort(), ['observation.log', 'receipt.json']);
});
