// PRIVATE mock-only tests. No Fly, process launch, namespace transition, models,
// Cargo, personal profile/cache reads or controller --run. /proc status is the
// only real runtime read. Run: node --experimental-vm-modules selftest.mjs
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import crypto from 'node:crypto';
import {EventEmitter} from 'node:events';
import {
  options, provenanceFor, parseProcStatus, validateRemoteProof, validateGuestMarker,
  preloadSource, plan, ownershipFor, rootProbeSource, gateSource, proofReadSource, main,
  parseOwnedExecJson, preserveOwnedExecOutput, fixtureDiagnosticsSource, saveFixtureDiagnostics,
  attemptFailureDiagnostics, readWorkerPackage, heldNamespaceWitness, privateErrorDetail,
} from './qualify-remote-execution.mjs';

const hash = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const normalize = value => JSON.parse(JSON.stringify(value));
const clone = value => structuredClone(value);
const binaryBytes = Buffer.from('synthetic remote execution qualifier, never executed by selftest');
const packageBytes = Buffer.from(JSON.stringify({version: '4.0.0-alpha.12'}));
const argv = ['--run', '--image', 'registry.fly.io/rudder-workers-exla:alpha13-private-source-fixture',
  '--digest', 'sha256:' + '2'.repeat(64), '--base-worker-digest', 'sha256:' + '3'.repeat(64),
  '--qualifier-sha256', hash(binaryBytes), '--source-envelope-sha256', '4'.repeat(64),
  '--worker-package-sha256', hash(packageBytes),
  '--revision', '1'.repeat(40), '--version', '4.0.0-alpha.13',
  '--nonce', '00000000-0000-0000-0000-000000000001'];
const value = options(argv), prepared = plan(value), input = prepared.input;
assert(Buffer.byteLength(prepared.config.env.NODE_OPTIONS) < 64 * 1024);

// Pure parsing and default main cannot reach any provider or filesystem write.
assert.deepEqual(options([]), {run: false});
assert.deepEqual(options(['--describe']), {run: false});
assert.equal((await main([])).status, 'prepared-no-run');
assert.equal((await main(['--describe'])).providerCalls, 0);
for (const required of ['image', 'digest', 'base-worker-digest', 'qualifier-sha256',
  'source-envelope-sha256', 'worker-package-sha256', 'revision', 'version']) {
  const missing = [...argv], at = missing.indexOf('--' + required); missing.splice(at, 2);
  assert.throws(() => options(missing), required);
}
for (const [key, invalid] of [['image', 'registry.fly.io/rudder-workers-exla:4.0.0-alpha.12'],
  ['digest', value.baseWorkerDigest], ['revision', 'short'], ['source-kind', 'automatic'],
  ['source-envelope-sha256', '../not-a-digest'], ['nonce', '8edd225a7d0378']]) {
  const changed = [...argv], at = changed.indexOf('--' + key);
  if (at < 0) changed.push('--' + key, invalid); else changed[at + 1] = invalid;
  assert.throws(() => options(changed), key);
}
const provenance = provenanceFor(value);
assert.equal(provenance.sourceKind, 'candidate'); assert.equal(provenance.baseRevision, value.revision);
assert.equal(provenance.revision, null); assert(!Object.hasOwn(provenance, 'shippedRevision'));
assert.equal(provenance.workerVersion, '4.0.0-alpha.12'); assert.equal(provenance.proposedVersion, '4.0.0-alpha.13');
assert(!Object.hasOwn(provenance, 'workerPackageSha256')); assert.equal(input.workerPackageSha256, hash(packageBytes));
assert.equal(input.gate.workerPackageSha256, hash(packageBytes));
const committed = provenanceFor(options([...argv, '--source-kind', 'committed-release', '--direct-base-digest', 'sha256:' + 'a'.repeat(64)]));
assert.equal(committed.baseRevision, null); assert.equal(committed.revision, value.revision);
assert.notEqual(committed.directBaseDigest, committed.baseWorkerDigest);
assert(!Object.hasOwn(committed, 'shippedRevision'));

// Exact serialized parser, not a lookalike regex. A real current /proc sample
// verifies tabs/newlines survive toString + JSON transport + isolated compile.
const parserSource = JSON.parse(JSON.stringify(parseProcStatus.toString()));
const parser = new vm.Script('(' + parserSource + ')').runInNewContext();
const text = fs.readFileSync('/proc/self/status', 'utf8'), expected = {};
const wanted = ['Uid', 'Gid', 'Groups', 'NoNewPrivs', 'CapEff', 'CapPrm', 'CapInh', 'CapAmb', 'CapBnd'];
for (const line of text.split(String.fromCharCode(10))) {
  const colon = line.indexOf(':'); if (colon < 0) continue;
  const key = line.slice(0, colon); if (wanted.includes(key)) expected[key] = line.slice(colon + 1).trim();
}
assert.equal(Object.keys(expected).length, 9); assert.deepEqual(normalize(parser(text)), expected);
// Execute the exact serialized helper on real own /proc namespace FDs using
// the host's real supported Node API. No namespace transition is performed.
const actualWitness = new vm.Script('(' + JSON.parse(JSON.stringify(heldNamespaceWitness.toString())) + ')').runInNewContext();
let actualProcWitnesses = 0;
for (const kind of ['user', 'net', 'mnt']) {
  const witness = actualWitness(fs, assert, kind);
  try {
    const current = fs.statSync('/proc/self/ns/' + kind, {bigint: true});
    assert.equal(witness.device, current.dev.toString()); assert.equal(witness.inode, current.ino.toString());
    assert.equal(fs.statfsSync('/proc/self/fd/' + witness.fd).type, 0x6e736673);
    actualProcWitnesses++;
  } finally {fs.closeSync(witness.fd);}
}
const longStack = 'TypeError: meaningful private failure\n    at witness (data:text/javascript;base64,'
  + 'A'.repeat(90000) + ':226:33)\n    at preload (data:text/javascript;base64,' + 'B'.repeat(90000)
  + ':340:11)\n    at finalMeaningfulFrame (owned-fixture.mjs:12:5)';
const serializedDetail = new vm.Script('(' + JSON.parse(JSON.stringify(privateErrorDetail.toString())) + ')').runInNewContext({Buffer});
const compacted = serializedDetail({stack: longStack});
assert.equal(compacted.raw.length, Buffer.byteLength(longStack)); assert.equal(hash(compacted.raw), hash(Buffer.from(longStack)));
assert(compacted.formatted.length < 8192); assert(compacted.shortened);
assert(compacted.formatted.toString().includes('TypeError: meaningful private failure'));
assert(compacted.formatted.toString().includes('at witness (data:<inline-preload>:226:33)'));
assert(compacted.formatted.toString().includes('finalMeaningfulFrame'));
const tab = String.fromCharCode(9), lf = String.fromCharCode(10), cr = String.fromCharCode(13);
const zeroStatus = {Uid: [1500, 1500, 1500, 1500].join(tab), Gid: [1500, 1500, 1500, 1500].join(tab),
  Groups: '', NoNewPrivs: '1', CapEff: '0000000000000000', CapPrm: '0000000000000000',
  CapInh: '0000000000000000', CapAmb: '0000000000000000', CapBnd: '0000000000000000'};
for (const newline of [lf, cr + lf]) for (const groups of ['', '1500 1501']) {
  const fields = {...zeroStatus, Groups: groups, CapAmb: '0000000000000008'};
  const fixture = ['Name:' + tab + 'mock', ...wanted.map(key => key + ':' + tab + fields[key]),
    'Threads:' + tab + '1', 'Other:' + tab + 'must not spill'].join(newline) + newline;
  assert.deepEqual(normalize(parser(fixture)), fields);
}

const reference = (kind, character, size_bytes = 128) => ({kind, algorithm: 'sha256', digest: character.repeat(64), size_bytes});
const receipt = (pin, character, count) => ({format_version: 1, pin_id: pin,
  staging_id: '00000000-0000-0000-0000-000000000005', set_digest: character.repeat(64), generation: 1,
  object_count: count, total_bytes: 4096});
const pipe = () => ({bytes: 0, retained_bytes: 0, truncated: false, sha256: hash(Buffer.alloc(0))});
const summary = {
  kind: 'remote-execution-qualification', format_version: 1, status: 'passed', utc: '2026-10-31T00:00:00Z',
  fixture_root: input.fixtureBase + '/rreq-12345678', execution_id: '00000000-0000-0000-0000-000000000002',
  request_id: '00000000-0000-0000-0000-000000000003', execution_spec: 4, managed_binding: 3,
  network_readiness_protocol: 3, namespace_schema: 5, profile: 'offline-command-v1', network: 'actual_private_offline_namespace',
  io: 'owned_pipes', pty_qualified: false, synthetic_authority: true, provider_upload_qualified: false,
  source_only: true, environment_free: true, empty_topology: true, vcs_application: false,
  entries: 100101, inherited_file_logical_bytes: 208 * 1024 * 1024,
  synthetic_graph_objects: 400, synthetic_graph_bytes: 32 * 1024 * 1024,
  inherited_directory: reference('directory', 'a'), inherited_file: reference('file', 'b'),
  inherited_payload: reference('blob', 'c', 4 * 1024 * 1024),
  source: {root_admissions: 1, fetch_calls: 400, fetched_bytes: 30 * 1024 * 1024,
    file_metadata_requests: 1, unchanged_payload_requests: 0, accepted_seed_requests: 0, active_fetches: 0, maximum_active_fetches: 1},
  data: {record_format: 2, artifact: 'partial_remote', capture_format: 1, capture_consistency: 'live_best_effort',
    stop_nonce: '00000000-0000-0000-0000-000000000004', partial_directory: reference('directory', 'd'),
    local_objects: 4, producer_objects: 3, remote_boundaries: 3, new_file_bytes: 52, new_file_sha256: 'e'.repeat(64),
    freeze_intent: reference('blob', 'f'), freeze_bytes_sha256: 'f'.repeat(64),
    drain_record: reference('blob', '1'), drain_bytes_sha256: '1'.repeat(64),
    capture_retention: {format_version: 1, plan: reference('blob', '2'), local: receipt('capture', '3', 4)},
    evidence_retention: receipt('evidence', '4', 2), complete_pins: 0,
    gc: {generation: 1, retained_objects: 6, removed_objects: 0, removed_bytes: 0, blocked_by_staging: false}},
  backend_ready_authenticated: true, guest_marker_checked: true, mmap_and_open_fd_and_descendant: true,
  shutdown_typed_and_authenticated: true, record_sha256: '5'.repeat(64),
  historical_retry: {outcome: 'historical', factory_calls: 1, new_source: false, current_shutdown_authority: false, stdout: pipe(), stderr: pipe()},
  stdout: pipe(), stderr: pipe(), elapsed_ms: 2000,
  limits: {max_entries: 100101, case_timeout_seconds: 180, process_alarm_seconds: 240, source_fetch_calls: 4096,
    source_fetched_bytes: 64 * 1024 * 1024, complete_pin_or_acceptance: false, host_fallback: false},
};
const marker = {request_id: summary.request_id, entries: 100101, logical_bytes: 208 * 1024 * 1024,
  open_fd: true, mmap_open: true, descendant_started: true, zero_capabilities: true, no_new_privileges: true,
  interfaces: ['lo'], private_paths_absent: true};
const counts = {qualified: 1, unsupported: 0, failed: 0, physicalRemoteCasesPassed: 1, historicalReopensPassed: 1};
assert.deepEqual(validateRemoteProof(summary, input, assert), counts); validateGuestMarker(marker, summary, assert);
let proofRejections = 0;
for (const mutate of [
  x => x.status = 'unsupported', x => x.execution_spec = 3, x => x.managed_binding = 2,
  x => x.network_readiness_protocol = 2, x => x.namespace_schema = 4,
  x => x.data.record_format = 1, x => x.data.artifact = 'complete_local', x => x.data.capture_format = 2,
  x => x.data.capture_consistency = 'quiesced', x => x.data.complete_pins = 1,
  x => x.entries = 100000, x => x.inherited_file_logical_bytes--, x => x.source.unchanged_payload_requests = 1,
  x => x.source.accepted_seed_requests = 1, x => x.source.active_fetches = 1, x => x.source.fetched_bytes = 64 * 1024 * 1024 + 1,
  x => x.source.fetch_calls = 4097, x => x.source.root_admissions = 2, x => x.source.file_metadata_requests = 0,
  x => x.backend_ready_authenticated = false, x => x.shutdown_typed_and_authenticated = false,
  x => x.mmap_and_open_fd_and_descendant = false, x => x.pty_qualified = true, x => x.provider_upload_qualified = true,
  x => x.vcs_application = true, x => x.source_only = false, x => x.environment_free = false, x => x.empty_topology = false,
  x => x.data.remote_boundaries = 0, x => x.data.gc.blocked_by_staging = true,
  x => x.data.evidence_retention.object_count = 1, x => x.data.capture_retention.local.object_count = 3,
  x => x.data.freeze_bytes_sha256 = '0'.repeat(64), x => x.historical_retry.outcome = 'finalized',
  x => x.historical_retry.factory_calls = 2, x => x.historical_retry.new_source = true,
  x => x.historical_retry.current_shutdown_authority = true, x => x.fixture_root = '/workspace/unowned',
  x => x.limits.host_fallback = true, x => x.limits.complete_pin_or_acceptance = true,
]) {
  const changed = clone(summary); mutate(changed); assert.throws(() => validateRemoteProof(changed, input, assert)); proofRejections++;
}
for (const mutate of [x => x.zero_capabilities = false, x => x.interfaces = ['lo', 'eth0'],
  x => x.no_new_privileges = false, x => x.descendant_started = false, x => x.request_id = input.nonce]) {
  const changed = clone(marker); mutate(changed); assert.throws(() => validateGuestMarker(changed, summary, assert)); proofRejections++;
}

async function exercise(scenario) {
  let fdNext = 10, spawned = 0, net = '200';
  const descriptors = new Map(), files = new Map(), directories = new Set(['/workspace', '/opt/rudder/qualification']);
  const writes = new Map(), timers = new Map(); let timerNext = 0, resolveProof;
  const ready = new Promise(resolve => {resolveProof = resolve;});
  const file = (bytes, uid, mode) => ({bytes: Buffer.from(bytes), uid, mode, nlink: 1});
  files.set(input.gatePath, file(JSON.stringify(scenario === 'bad-gate' ? {...input.gate, digest: 'sha256:' + '9'.repeat(64)} : input.gate), 1500, 0o600));
  files.set(input.provenancePath, file(JSON.stringify(scenario === 'bad-provenance' ? {...input.provenance, sourceEnvelopeSha256: '9'.repeat(64)} : input.provenance), 0, 0o444));
  files.set(input.binary, file(scenario === 'bad-binary' ? Buffer.from('wrong-binary') : binaryBytes, 0, 0o555));
  files.set('/opt/rudder/package.json', file(JSON.stringify({version: scenario === 'wrong-worker-version' ? value.version : value.workerVersion}), 0, 0o644));
  if (scenario === 'approved-package-0664') files.get('/opt/rudder/package.json').mode = 0o664;
  const statFile = entry => ({isFile: () => true, isDirectory: () => false, isSymbolicLink: () => false,
    nlink: entry.nlink, uid: entry.uid, gid: entry.uid === 0 ? 0 : 1500, dev: 7, ino: 1, mode: entry.mode, size: entry.bytes.length});
  const metadata = name => {
    if (directories.has(name)) return {isDirectory: () => true, isFile: () => false, isSymbolicLink: () => false,
      uid: name.startsWith('/workspace') ? 1500 : 0, mode: name.startsWith('/workspace') ? 0o700 : 0o555};
    if (files.has(name)) return statFile(files.get(name));
    if (name === '/dev/fuse') return {isCharacterDevice: () => true, uid: scenario === 'bad-fuse' ? 0 : 1500, gid: 1500, mode: 0o600};
    if (name === '/usr/bin/python3') {
      if (scenario === 'missing-python') throw Object.assign(Error('mock only'), {code: 'ENOENT'});
      return {isFile: () => true, uid: 0, mode: 0o755};
    }
    throw Object.assign(Error('unexpected mocked stat'), {code: 'ENOENT'});
  };
  const fsMock = {
    constants: fs.constants,
    openSync(name, flags, mode) {
      const fd = fdNext++;
      if (name.startsWith('/proc/self/ns/')) {
        descriptors.set(fd, {name, inode: name.endsWith('/user') ? '100' : name.endsWith('/net') ? net : '300'}); return fd;
      }
      if (name === '/dev/fuse') {descriptors.set(fd, {name}); return fd;}
      if (flags & fs.constants.O_CREAT) {
        assert(flags & fs.constants.O_EXCL); assert(flags & fs.constants.O_NOFOLLOW); assert.equal(mode, 0o600);
        assert(directories.has(name.slice(0, name.lastIndexOf('/'))));
        assert(!files.has(name)); files.set(name, file('', 1500, mode));
      } else {
        assert(flags & fs.constants.O_NOFOLLOW); assert(flags & fs.constants.O_NONBLOCK);
      }
      if (!files.has(name)) throw Object.assign(Error('mock missing'), {code: 'ENOENT'});
      descriptors.set(fd, {name, offset: 0}); return fd;
    },
    closeSync(fd) {assert(descriptors.delete(fd));},
    fstatSync(fd, options) {
      const entry = descriptors.get(fd); assert(entry);
      if (entry.name.startsWith('/proc/self/ns/')) {assert.equal(options.bigint, true); return {dev: 4n, ino: BigInt(entry.inode)};}
      return metadata(entry.name);
    },
    statfsSync(file) {
      const match = /^\/proc\/self\/fd\/([0-9]+)$/.exec(file); assert(match);
      const entry = descriptors.get(Number(match[1])); assert(entry.name.startsWith('/proc/self/ns/'));
      if (scenario === 'witness-rebound') entry.inode = '99999';
      if (scenario === 'data-url-error') {const error = new Error('meaningful private failure'); error.stack = longStack; throw error;}
      return {type: scenario === 'wrong-nsfs' ? 0xef53 : 0x6e736673};
    },
    lstatSync: metadata, statSync: metadata,
    readSync(fd, buffer, offset, length) {
      const entry = descriptors.get(fd), content = files.get(entry.name).bytes;
      const count = Math.min(length, content.length - entry.offset);
      content.copy(buffer, offset, entry.offset, entry.offset + count); entry.offset += count; return count;
    },
    readFileSync(name, encoding) {
      assert.equal(name, '/proc/self/status'); assert.equal(encoding, 'utf8');
      const fields = {...zeroStatus}; if (scenario === 'bad-boot') fields.CapBnd = '0000000000000001';
      return wanted.map(key => key + ':' + tab + fields[key]).join(lf) + lf;
    },
    writeFileSync(fd, bytes) {
      const entry = descriptors.get(fd); assert(entry); const data = Buffer.from(bytes);
      files.get(entry.name).bytes = data; writes.set(entry.name, data);
      if (entry.name === input.proofPath) resolveProof(JSON.parse(data));
    },
    fsyncSync(fd) {assert(descriptors.has(fd));},
    existsSync(name) {return scenario === 'existing-fixture' && name === input.fixtureBase || directories.has(name) || files.has(name);},
    mkdirSync(name, options) {assert.equal(options.mode, 0o700); assert(!directories.has(name)); directories.add(name);},
    accessSync(name, mode) {assert.equal(name, '/usr/bin/python3'); assert.equal(mode, fs.constants.X_OK);},
    realpathSync(name) {assert.equal(name, '/usr/bin/python3'); return scenario === 'foreign-python' ? '/untrusted/python' : '/usr/bin/python3.11';},
  };
  const cpMock = {spawn(binary, args, launch) {
    spawned++; assert.equal(binary, input.binary);
    assert.deepEqual(normalize(args), ['qualify', '--base', input.fixtureBase, '--entries', '100101']);
    assert.equal(launch.cwd, input.home); assert.equal(launch.env.HOME, input.home);
    assert(!Object.hasOwn(launch.env, 'NODE_OPTIONS'));
    assert.equal(Object.keys(launch.env).filter(key => /TOKEN|SECRET|PASSWORD|API_KEY|CREDENTIAL/i.test(key)).length, 0);
    for (const key of ['RUDDER_HOME', 'CLAUDE_CONFIG_DIR', 'CODEX_HOME', 'TMPDIR']) assert(launch.env[key].startsWith(input.home + '/'));
    assert.deepEqual(normalize(launch.stdio), ['ignore', 'pipe', 'pipe']);
    const child = new EventEmitter(); child.stdout = new EventEmitter(); child.stderr = new EventEmitter();
    child.stdout.destroy = () => {}; child.stderr.destroy = () => {}; child.unref = () => {};
    let killed = false; child.kill = signal => {assert.equal(signal, 'SIGKILL'); killed = true; return true;};
    queueMicrotask(() => {
      if (scenario === 'hanging-child') return;
      const proof = clone(summary);
      if (scenario === 'payload-read') proof.source.unchanged_payload_requests = 1;
      if (scenario === 'false-physical-proof') proof.shutdown_typed_and_authenticated = false;
      if (scenario === 'fake-complete') proof.data.artifact = 'complete_local';
      if (scenario === 'historical-launch') proof.historical_retry.new_source = true;
      const readback = clone(marker); if (scenario === 'guest-has-capability') readback.zero_capabilities = false;
      if (scenario === 'guest-host-network') readback.interfaces.push('eth0');
      const markerPath = proof.fixture_root + '/executions/runs/' + proof.execution_id + '/scratch/remote-guest-ready.json';
      files.set(markerPath, file(JSON.stringify(readback), scenario === 'foreign-marker-owner' ? 0 : 1500, 0o600));
      directories.add(proof.fixture_root);
      let stdout = JSON.stringify(proof) + '\n', stderr = '';
      if (scenario === 'bad-json') stdout = 'DO_NOT_COPY_SYNTHETIC_SECRET\n';
      if (scenario === 'oversize-stdout') stdout = 'x'.repeat(1024 * 1024 + 1);
      if (scenario === 'oversize-stderr') stderr = 'x'.repeat(8193);
      if (scenario === 'private-error') stderr = 'DO_NOT_COPY_SYNTHETIC_SECRET';
      if (scenario === 'parent-change') net = '999';
      if (scenario === 'spawn-failure') child.emit('error', Object.assign(Error('private only'), {code: 'ENOENT'}));
      child.stdout.emit('data', Buffer.from(stdout)); child.stderr.emit('data', Buffer.from(stderr));
      const status = killed ? null : scenario === 'unsupported' ? 77 : ['private-error', 'nonzero'].includes(scenario) ? 1 : 0;
      child.emit('exit', status, killed ? 'SIGKILL' : null);
      if (scenario !== 'exit-with-open-pipe') child.emit('close');
    });
    return child;
  }};
  const env = {RUDDER_WORKER_MANAGED_EXECUTION: '1', RUDDER_WORKER_SKIP_AGENT_UPDATE: '1', RUDDER_WORKER_FUSE_READY: '1'};
  if (scenario === 'credential-environment') env.AWS_ACCESS_KEY_ID = 'DO_NOT_COPY_SYNTHETIC_SECRET';
  const context = vm.createContext({Buffer, Date, queueMicrotask, setTimeout(fn, ms) {
    const id = ++timerNext; timers.set(id, ms);
    if (ms <= 100 || ['hanging-child', 'exit-with-open-pipe'].includes(scenario) && [255000, 2000].includes(ms)) queueMicrotask(fn);
    return id;
  }, clearTimeout(id) {timers.delete(id);}, process: {env, getuid: () => 1500, getgid: () => 1500,
    version: 'mock-only', exit() {throw Error('mock lifetime timer must not execute');}}});
  const mocks = {'node:fs': fsMock, 'node:child_process': cpMock, 'node:crypto': {createHash: crypto.createHash},
    'node:assert/strict': {default: assert}, 'node:os': {release: () => 'mock-only'}};
  const modules = new Map();
  const script = new vm.SourceTextModule(preloadSource(input), {context, importModuleDynamically: async name => {
    if (!modules.has(name)) {
      const mock = mocks[name]; assert(mock, name);
      const module = new vm.SyntheticModule(Object.keys(mock), function () {
        for (const [key, item] of Object.entries(mock)) this.setExport(key, item);
      }, {context});
      await module.link(() => {}); await module.evaluate(); modules.set(name, module);
    }
    return modules.get(name);
  }});
  await script.link(() => {throw Error('only explicit dynamic mock imports allowed');});
  script.evaluate().catch(error => resolveProof({unexpectedMockFailure: String(error)}));
  let timer;
  try {
    const proof = await Promise.race([ready, new Promise((_, reject) => {timer = setTimeout(() => reject(Error('mock preload did not settle')), 1000);})]);
    assert(!proof.unexpectedMockFailure, proof.unexpectedMockFailure); assert.equal(descriptors.size, 0);
    assert(Buffer.byteLength(JSON.stringify(proof)) <= 1024 * 1024);
    assert(!JSON.stringify(proof).includes('DO_NOT_COPY_SYNTHETIC_SECRET'));
    assert(timers.has(1)); assert.equal(timers.get(1), 420000);
    return {proof, spawned, writes};
  } finally {clearTimeout(timer);}
}
const positive = await exercise('positive');
assert.equal(positive.spawned, 1); assert.equal(positive.proof.completed, true); assert.equal(positive.proof.qualified, true);
assert.deepEqual(positive.proof.counts, counts); assert.equal(positive.proof.boot.uid, 1500);
assert.equal(positive.proof.guest.zeroFiveCapabilitySets, true); assert.deepEqual(positive.proof.guest.interfaces, ['lo']);
const approvedArchiveMode = await exercise('approved-package-0664');
assert.equal(approvedArchiveMode.proof.qualified, true); assert.equal(approvedArchiveMode.spawned, 1);
assert.equal(approvedArchiveMode.proof.boot.workerPackage.mode, 0o664);
assert.equal(approvedArchiveMode.proof.boot.workerPackage.sha256, input.workerPackageSha256);
const scenarios = ['bad-gate', 'bad-provenance', 'bad-binary', 'wrong-worker-version', 'bad-boot',
  'credential-environment', 'bad-fuse', 'missing-python', 'foreign-python', 'existing-fixture',
  'payload-read', 'false-physical-proof', 'fake-complete', 'historical-launch', 'guest-has-capability',
  'guest-host-network', 'foreign-marker-owner', 'parent-change', 'bad-json', 'oversize-stdout',
  'oversize-stderr', 'spawn-failure', 'unsupported', 'private-error', 'nonzero', 'hanging-child', 'exit-with-open-pipe',
  'wrong-nsfs', 'witness-rebound', 'data-url-error'];
for (const scenario of scenarios) {
  const tested = await exercise(scenario), proof = tested.proof;
  assert.equal(proof.completed, false, scenario); assert.equal(proof.qualified, false, scenario);
  assert.equal(proof.counts.physicalRemoteCasesPassed, 0); assert.equal(proof.counts.historicalReopensPassed, 0);
  assert.equal(proof.counts.unsupported, scenario === 'unsupported' ? 1 : 0);
  assert.equal(proof.counts.failed, scenario === 'unsupported' ? 0 : 1);
  if (scenarios.indexOf(scenario) <= scenarios.indexOf('existing-fixture')) assert.equal(tested.spawned, 0, scenario);
  if (scenario === 'private-error') assert.equal(tested.writes.get(input.home + '/qualifier.stderr.private').toString(), 'DO_NOT_COPY_SYNTHETIC_SECRET');
  if (scenario === 'oversize-stderr') {
    assert.equal(tested.writes.get(input.home + '/qualifier.stderr.private').length, 8192);
    assert.equal(proof.child.stderr.truncated, true);
  }
  if (['hanging-child', 'exit-with-open-pipe'].includes(scenario)) assert.equal(proof.child.settled, false);
  if (['wrong-nsfs', 'witness-rebound', 'data-url-error'].includes(scenario)) assert.equal(tested.spawned, 0);
  if (scenario === 'data-url-error') {
    assert.equal(proof.privateFailure.bytes, Buffer.byteLength(longStack));
    assert.equal(proof.privateFailure.sha256, hash(Buffer.from(longStack)));
    assert(proof.privateFailure.dataUrlFramesShortened); assert(proof.privateFailure.retainedBytes < 8192);
    const saved = tested.writes.get(input.failurePath);
    assert.equal(hash(saved), proof.privateFailure.retainedSha256); assert(saved.toString().includes('finalMeaningfulFrame'));
  }
  assert(tested.writes.has(input.failurePath), scenario + ': private diagnostic missing');
}

const observed = 'sha256:' + 'a'.repeat(64), another = 'sha256:' + 'b'.repeat(64);
const machine = {id: '1234567890abcd', name: prepared.name, state: 'started', image_ref: {digest: observed}, config: clone(prepared.config)};
const ownership = ownershipFor(prepared, value);
assert(ownership.matches(machine)); ownership.remember(machine);
assert.notEqual(observed, value.digest); assert.equal(ownership.descriptor().observedDigest, observed);
assert(ownership.matches(machine), 'wrong successful digest expectation must not prevent exact-owned cleanup');
const normalized = clone(machine); normalized.config.image += '@' + observed; assert(ownership.matches(normalized));
const firstNormalized = ownershipFor(prepared, value); assert(firstNormalized.matches(normalized)); firstNormalized.remember(normalized);
let ownershipRejections = 0;
for (const mutate of [
  x => x.id = '8edd225a7d0378', x => x.id = '22222222222222', x => x.name = 'not-owned',
  x => x.image_ref.digest = another, x => x.image_ref = {}, x => x.config.image = 'wrong-image',
  x => x.config.image += '@' + another, x => x.config.image += '@not-a-digest',
  x => x.config.guest.memory_mb = 4096, x => x.config.env.UNEXPECTED_SECRET = 'mock-only',
  x => x.config.env.PRIMARY_REGION = 'wrong-region', x => x.config.env.NODE_OPTIONS = 'other-code',
]) {
  const changed = clone(machine); mutate(changed); assert.equal(ownership.matches(changed), false); ownershipRejections++;
}
for (const key of Object.keys(prepared.config.metadata)) {
  const changed = clone(machine); changed.config.metadata[key] = 'foreign';
  assert.equal(ownership.matches(changed), false); ownershipRejections++;
}
for (const key of ['services', 'mounts', 'files']) for (const invalid of [[{}], {}, null]) {
  const changed = clone(machine); changed.config[key] = invalid;
  assert.equal(ownership.matches(changed), false); ownershipRejections++;
}
assert.throws(() => ownership.remember({...machine, image_ref: {digest: another}}));
assert.equal(ownership.descriptor().observedDigest, observed);
const unresolved = ownershipFor(prepared, value), notYetResolved = {...machine, image_ref: {}};
unresolved.remember(notYetResolved); assert.equal(unresolved.descriptor().observedDigest, null);
unresolved.remember(machine); assert.equal(unresolved.descriptor().observedDigest, observed);
assert(!unresolved.matches({...machine, image_ref: {digest: another}}));
const regionNormalized = clone(machine); regionNormalized.config.env.PRIMARY_REGION = 'sjc'; assert(ownership.matches(regionNormalized));

// Compile exact serialized root scripts without running the root image probe.
new vm.Script(rootProbeSource(input)); new vm.Script(gateSource(input)); new vm.Script(proofReadSource(input));
function exerciseGate(existing) {
  const files = new Map(), descriptors = new Map(); let next = 1, published;
  if (existing) files.set(input.gatePath, {bytes: Buffer.from('already-owned-other-gate'), nlink: 1});
  const mock = {constants: fs.constants,
    openSync(name, flags, mode) {
      const fd = next++;
      if (name === '/workspace') {descriptors.set(fd, {name}); return fd;}
      assert(flags & fs.constants.O_EXCL); assert(flags & fs.constants.O_NOFOLLOW); assert(flags & fs.constants.O_NONBLOCK); assert.equal(mode, 0o600);
      assert(!files.has(name)); const value = {bytes: Buffer.alloc(0), uid: 0, gid: 0, mode, nlink: 1}; files.set(name, value);
      descriptors.set(fd, {name, value}); return fd;
    },
    writeFileSync(fd, bytes) {descriptors.get(fd).value.bytes = Buffer.from(bytes);},
    fchownSync(fd, uid, gid) {Object.assign(descriptors.get(fd).value, {uid, gid});},
    fsyncSync(fd) {assert(descriptors.has(fd));}, closeSync(fd) {assert(descriptors.delete(fd));},
    linkSync(from, to) {
      if (files.has(to)) throw Object.assign(Error('no replacement'), {code: 'EEXIST'});
      const entry = files.get(from); files.set(to, entry); entry.nlink++;
    },
    unlinkSync(name) {files.get(name).nlink--; files.delete(name);},
  };
  const run = () => new vm.Script(gateSource(input)).runInNewContext({require(name) {
    if (name === 'node:fs') return mock; if (name === 'node:crypto') return {randomUUID: () => input.nonce};
    throw Error('unexpected gate import');
  }, console: {log(bytes) {published = JSON.parse(bytes);}}});
  if (existing) {assert.throws(run); assert.equal(files.get(input.gatePath).bytes.toString(), 'already-owned-other-gate');}
  else {
    run(); const gate = files.get(input.gatePath); assert.equal(gate.uid, 1500); assert.equal(gate.gid, 1500);
    assert.equal(gate.nlink, 1); assert.equal(gate.mode, 0o600); assert.deepEqual(JSON.parse(gate.bytes), input.gate);
    assert.deepEqual(published, {nonce: input.nonce, published: true});
  }
  assert.equal(files.size, 1); assert.equal(descriptors.size, 0);
}
exerciseGate(false); exerciseGate(true);

// Actual pipe persistence helper runs BEFORE the actual owned-exec parser.
// No error message or pipe body is copied into report fields or console.
const pipeWrites = new Map(), stdoutError = Buffer.from('Exit code: 1\n');
const stderrError = Buffer.from('synthetic-private-probe-error\n'.padEnd(781, 'x'));
const stored = await preserveOwnedExecOutput(stdoutError, stderrError, '/private/step.stdout', '/private/step.stderr',
  async (file, bytes, options) => {assert.equal(options.mode, 0o600); assert.equal(options.flag, 'wx'); pipeWrites.set(file, Buffer.from(bytes));});
assert.deepEqual(pipeWrites.get('/private/step.stdout'), stdoutError);
assert.deepEqual(pipeWrites.get('/private/step.stderr'), stderrError); assert.equal(stderrError.length, 781);
const failedStep = {ok: true, status: 0, ...stored};
assert.throws(() => parseOwnedExecJson(stdoutError.toString(), failedStep), {code: 'OWNED_GUEST_EXIT_NONZERO'});
assert.equal(failedStep.ok, false); assert.equal(failedStep.guestExitCode, 1);
assert.equal(failedStep.failure, 'OWNED_GUEST_EXIT_NONZERO');
const invalidStep = {ok: true, ...stored};
assert.throws(() => parseOwnedExecJson('synthetic-private-probe-error', invalidStep), {code: 'OWNED_EXEC_INVALID_JSON'});
assert.equal(invalidStep.ok, false); assert(!JSON.stringify(invalidStep).includes('synthetic-private-probe-error'));
const reportedStep = {ok: true};
assert.throws(() => parseOwnedExecJson('{"status":"failed"}', reportedStep), {code: 'OWNED_GUEST_REPORTED_FAILURE'});
assert.deepEqual(parseOwnedExecJson('{"status":"passed"}\n', {}), {status: 'passed'});

function exerciseRootProbe(packageMode, {binaryMode = 0o555, provenanceMode = 0o444, packageFields = {}, expectedPackageSha = input.workerPackageSha256} = {}) {
  const records = new Map(), descriptors = new Map(); let next = 1, stdout = '', stderr = '';
  const put = (name, bytes, mode) => records.set(name, {bytes: Buffer.from(bytes), mode, uid: 0, gid: 0, nlink: 1, dev: 7, ino: records.size + 1});
  put(input.binary, binaryBytes, binaryMode);
  put(input.provenancePath, JSON.stringify(input.provenance), provenanceMode);
  put('/opt/rudder/package.json', JSON.stringify({version: input.provenance.workerVersion}), packageMode);
  Object.assign(records.get('/opt/rudder/package.json'), packageFields);
  const stat = item => ({...item, isFile: () => item.type !== 'directory', isSymbolicLink: () => false, size: item.bytes.length});
  const fake = {constants: fs.constants,
    lstatSync(name) {if (records.has(name)) return stat(records.get(name));
      assert(['/usr/local/bin', '/opt/rudder/qualification'].includes(name));
      return {isDirectory: () => true, isSymbolicLink: () => false, uid: 0, mode: 0o755};},
    openSync(name, flags) {assert(flags & fs.constants.O_NOFOLLOW); assert(flags & fs.constants.O_NONBLOCK);
      assert(records.has(name)); const fd = next++; descriptors.set(fd, {record: records.get(name), offset: 0}); return fd;},
    fstatSync(fd) {return stat(descriptors.get(fd).record);},
    readSync(fd, buffer, offset, length) {
      const state = descriptors.get(fd), count = Math.min(length, state.record.bytes.length - state.offset);
      state.record.bytes.copy(buffer, offset, state.offset, state.offset + count); state.offset += count; return count;
    },
    closeSync(fd) {assert(descriptors.delete(fd));},
    statSync(name) {assert.equal(name, '/usr/bin/python3'); return {isFile: () => true, uid: 0, mode: 0o755};},
    accessSync(name, mode) {assert.equal(name, '/usr/bin/python3'); assert.equal(mode, fs.constants.X_OK);},
  };
  const processMock = {getuid: () => 0, stderr: {write(bytes) {assert(bytes.length <= 8192); stderr += bytes.toString();}}};
  new vm.Script(rootProbeSource({...input, workerPackageSha256: expectedPackageSha})).runInNewContext({require(name) {
    if (name === 'node:fs') return fake; if (name === 'node:crypto') return crypto;
    if (name === 'node:assert/strict') return assert; throw Error('unexpected mock probe import');
  }, Buffer: {alloc: size => Buffer.alloc(Math.min(size, 65537)), from: Buffer.from},
  process: processMock, console: {log(text) {stdout += text;}}});
  assert.equal(descriptors.size, 0);
  return {proof: JSON.parse(stdout), stderr, exitCode: processMock.exitCode};
}
const packageProbe = exerciseRootProbe(0o664);
assert.equal(packageProbe.proof.status, 'passed');
assert.equal(packageProbe.proof.packageMetadata.mode, 0o664); assert.equal(packageProbe.proof.packageMetadata.uid, 0);
assert.equal(packageProbe.proof.packageMetadata.gid, 0); assert.equal(packageProbe.proof.packageMetadata.nlink, 1);
assert.equal(packageProbe.proof.packageMetadata.sha256, hash(Buffer.from(JSON.stringify({version: input.provenance.workerVersion}))));
assert.equal(packageProbe.exitCode, undefined); assert.equal(packageProbe.stderr.length, 0);
assert.equal(exerciseRootProbe(0o644).proof.status, 'passed');
assert.equal(exerciseRootProbe(0o644, {binaryMode: 0o755}).proof.status, 'failed');
assert.equal(exerciseRootProbe(0o644, {provenanceMode: 0o644}).proof.status, 'failed');
let packageFixtureRejections = 0;
for (const [mode, fields, expectedPackageSha] of [
  [0o664, {}, '0'.repeat(64)], [0o664, {gid: 1500}], [0o664, {uid: 1500}],
  [0o666, {}], [0o674, {}], [0o755, {}], [0o4664, {}], [0o2664, {}], [0o1664, {}],
  [0o664, {nlink: 2}], [0o664, {type: 'directory'}],
]) {
  assert.equal(exerciseRootProbe(mode, {packageFields: fields, expectedPackageSha}).proof.status, 'failed');
  packageFixtureRejections++;
}

// The exact serialized diagnostic reader is mocked at its filesystem boundary.
// Only five explicit generated error paths may be read; directory enumeration
// is limited to two entries and file opens use NOFOLLOW/NONBLOCK before fstat.
function exerciseDiagnostics(scenario) {
  const entries = new Map(), descriptors = new Map(), actualReads = []; let next = 10, inode = 1, output;
  const fixture = input.fixtureBase + '/rreq-12345678';
  const add = (name, type, bytes = '', mode = type === 'directory' ? 0o700 : 0o600) => {
    entries.set(name, {name, type, bytes: Buffer.from(bytes), mode, uid: 1500, gid: 1500, nlink: 1, dev: 7, ino: inode++});
  };
  add('/workspace', 'directory', '', 0o755); add(input.home, 'directory'); add(input.fixtureBase, 'directory'); add(fixture, 'directory');
  add(input.failurePath, 'file', 'synthetic-private-preload-failure');
  add(input.home + '/qualifier.stderr.private', 'file', 'synthetic-private-qualifier-stderr');
  add(fixture + '/failure.private.txt', 'file', 'synthetic-private-helper-failure');
  add(fixture + '/initial.stderr', 'file', 'synthetic-private-initial-stderr');
  const target = entries.get(fixture + '/failure.private.txt');
  if (scenario === 'symlink-file') target.type = 'symlink';
  if (scenario === 'fifo-file') target.type = 'fifo';
  if (scenario === 'oversize-file') target.bytes = Buffer.alloc(65537);
  if (scenario === 'foreign-owner') target.uid = 0;
  if (scenario === 'wrong-mode') target.mode = 0o644;
  if (scenario === 'hardlink-file') target.nlink = 2;
  if (scenario === 'symlink-base') entries.get(input.fixtureBase).type = 'symlink';
  if (scenario === 'foreign-home') entries.get(input.home).uid = 0;
  if (scenario === 'missing-fixture') entries.delete(fixture);
  const resolve = name => {
    const match = /^\/proc\/self\/fd\/([0-9]+)(?:\/([A-Za-z0-9_.-]+))?$/.exec(name);
    if (match) {assert(descriptors.has(Number(match[1]))); return descriptors.get(Number(match[1])).entry.name + (match[2] ? '/' + match[2] : '');}
    assert.equal(name, '/workspace'); return name;
  };
  const metadata = item => ({...item, size: item.bytes.length,
    isFile: () => item.type === 'file', isDirectory: () => item.type === 'directory', isSymbolicLink: () => item.type === 'symlink'});
  const fake = {constants: fs.constants,
    openSync(name, flags) {
      assert(flags & fs.constants.O_NOFOLLOW); assert(flags & fs.constants.O_NONBLOCK);
      const resolved = resolve(name), item = entries.get(resolved);
      if (!item) throw Object.assign(Error('mock missing'), {code: 'ENOENT'});
      if (item.type === 'symlink') throw Object.assign(Error('mock symlink'), {code: 'ELOOP'});
      if (flags & fs.constants.O_DIRECTORY) assert.equal(item.type, 'directory');
      const fd = next++; descriptors.set(fd, {entry: item, offset: 0}); return fd;
    },
    fstatSync(fd) {return metadata(descriptors.get(fd).entry);},
    lstatSync(name) {return metadata(entries.get(resolve(name)));},
    readSync(fd, buffer, offset, length) {
      const state = descriptors.get(fd), item = state.entry;
      actualReads.push(item.name); const count = Math.min(length, item.bytes.length - state.offset);
      item.bytes.copy(buffer, offset, state.offset, state.offset + count); state.offset += count;
      if (scenario === 'growing-file' && item === target && count > 0) item.bytes = Buffer.concat([item.bytes, Buffer.from('x')]);
      return count;
    },
    closeSync(fd) {assert(descriptors.delete(fd));},
    opendirSync(name, options) {
      assert.equal(resolve(name), input.fixtureBase); assert.equal(options.bufferSize, 2);
      const children = scenario === 'extra-fixture' ? [{name: 'rreq-12345678'}, {name: 'unapproved'}]
        : scenario === 'missing-fixture' ? [] : [{name: 'rreq-12345678'}];
      return {readSync() {return children.shift() ?? null;}, closeSync() {}};
    },
  };
  const altered = clone(input);
  if (scenario === 'wrong-nonce-path') altered.fixtureBase = '/workspace/qe-foreign';
  const run = () => new vm.Script(fixtureDiagnosticsSource(altered)).runInNewContext({Buffer,
    require(name) {if (name === 'node:fs') return fake; if (name === 'node:crypto') return crypto;
      if (name === 'node:assert/strict') return assert; throw Error('unexpected mock diagnostic import');},
    console: {log(bytes) {output = JSON.parse(bytes);}}});
  if (scenario === 'wrong-nonce-path') {assert.throws(run); assert.equal(descriptors.size, 0); return {rejectedBeforeRead: true};}
  run(); assert.equal(descriptors.size, 0); assert(Buffer.byteLength(JSON.stringify(output)) <= 512 * 1024);
  const allowedReads = [input.failurePath, input.home + '/qualifier.stderr.private', fixture + '/failure.private.txt',
    fixture + '/initial.stderr', fixture + '/historical.stderr'];
  for (const name of actualReads) assert(allowedReads.includes(name));
  if (['symlink-file', 'fifo-file', 'oversize-file', 'foreign-owner', 'wrong-mode', 'hardlink-file'].includes(scenario)) {
    assert(!actualReads.includes(fixture + '/failure.private.txt'));
    assert.equal(output.files.find(file => file.label === 'helper-failure').status, 'rejected');
  }
  if (['symlink-base', 'extra-fixture', 'missing-fixture'].includes(scenario)) assert(!actualReads.some(name => name.startsWith(fixture + '/')));
  if (scenario === 'growing-file') assert.equal(output.files.find(file => file.label === 'helper-failure').status, 'rejected');
  return {output};
}
const diagnosticPositive = exerciseDiagnostics('positive').output;
const savedDiagnostics = new Map();
const savedManifest = await saveFixtureDiagnostics(diagnosticPositive, input, '/private/fixture', async (name, bytes, opts) => {
  assert.equal(opts.mode, 0o600); assert.equal(opts.flag, 'wx'); savedDiagnostics.set(name, Buffer.from(bytes));
});
assert.equal(savedManifest.completed, true); assert.equal(savedDiagnostics.size, 5); // four existing bodies + manifest
assert.equal(savedDiagnostics.get('/private/fixture/failure-diagnostics-helper-failure.private').toString(), 'synthetic-private-helper-failure');
assert(!JSON.stringify(savedManifest).includes('synthetic-private-helper-failure'));
const diagnosticScenarios = ['symlink-file', 'fifo-file', 'oversize-file', 'foreign-owner', 'wrong-mode',
  'hardlink-file', 'symlink-base', 'foreign-home', 'extra-fixture', 'missing-fixture', 'growing-file', 'wrong-nonce-path'];
for (const scenario of diagnosticScenarios) exerciseDiagnostics(scenario);
for (const mutate of [x => x.nonce = 'foreign', x => x.fixtureBase = '/home/real',
  x => x.files[0].source = '/home/real/auth.json', x => x.files[0].label = 'provider-auth',
  x => x.files[0].sha256 = '0'.repeat(64), x => x.files[0].metadata.uid = 0,
  x => x.files[0].bodyBase64 += '====', x => x.files.push(clone(x.files[0]))]) {
  const forged = clone(diagnosticPositive); mutate(forged); let writes = 0;
  await assert.rejects(() => saveFixtureDiagnostics(forged, input, '/private/fixture', async () => {writes++;}));
  assert.equal(writes, 0);
}
let cleanupRequired = 0;
for (const mode of ['exec-failed', 'invalid-bundle', 'save-failed']) {
  try {
    const outcome = await attemptFailureDiagnostics(async () => {
      if (mode === 'exec-failed') throw Object.assign(Error('private-only'), {code: 'OWNED_GUEST_EXIT_NONZERO'});
      return mode === 'invalid-bundle' ? {nonce: 'foreign'} : diagnosticPositive;
    }, bundle => saveFixtureDiagnostics(bundle, input, '/private/fixture', async () => {throw Error('private disk failure');}));
    assert.equal(outcome.completed, false); assert(!JSON.stringify(outcome).includes('private-only'));
  } finally {cleanupRequired++;}
}
assert.equal(cleanupRequired, 3);

console.log(JSON.stringify({kind: 'alpha13-remote-execution-controller-selftest',
  defaultAndDescribeNoRun: true, candidateIdentityHonest: true, directBaseVsWorkerAncestrySeparate: true,
  actualSerializedProcStatusPassed: true, realTabNewlineFixtures: 4,
  actualSerializedHeldFdStatfsWitnesses: actualProcWitnesses, dataUrlFramesCompactedBeforePrivateCap: true,
  proofMismatchRejections: proofRejections, mockedPreloadScenarios: scenarios.length + 2,
  positiveMockCounts: counts, ownershipMismatchRejections: ownershipRejections,
  flyNormalizedTagDigestAccepted: true, wrongExpectedDigestStillOwned: true,
  atomicGateNoReplacement: true, privateFailureNotExposed: true,
  ownedExecPipesPersistBeforeParse: true, guestNonzeroExitClassified: true,
  packageMode0664RootGroupExactHashPasses: true, packageFixtureRejections, binaryAndProvenancePinsUnchanged: true,
  privateDiagnosticReaderScenarios: diagnosticScenarios.length + 1, diagnosticFailureStillRequiresCleanup: cleanupRequired,
  controllerMainRunExecuted: false, providerCalls: 0, actualRemoteGuestQualification: false}));
