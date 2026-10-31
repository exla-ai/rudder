// PRIVATE opt-in controller. Import, --describe and default invocation create
// no resources. Root alone supplies a built image and explicitly invokes --run.
import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
import {createHash, randomUUID} from 'node:crypto';
import {spawn} from 'node:child_process';
import {parseArgs} from 'node:util';
import {fileURLToPath} from 'node:url';

const APP = 'rudder-workers-exla';
const FLY = '/workspace/home/.fly/bin/flyctl';
const ROLE = 'alpha13-remote-execution-qualification';
const FORBIDDEN = '8edd225a7d0378';
const BINARY = '/usr/local/bin/rudder-remote-execution-qualifier';
const PROVENANCE = '/opt/rudder/qualification/remote-execution-provenance.json';
const MAX_PROOF = 1024 * 1024;
const MAX_STDERR = 8 * 1024;
const MAIN_MS = 450_000;
const LIFETIME_MS = 540_000;
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const digestPattern = /^sha256:[0-9a-f]{64}$/;
const shaPattern = /^[0-9a-f]{64}$/;
const uuidPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

export function options(argv) {
  const {values} = parseArgs({args: argv, strict: true, allowPositionals: false, options: {
    run: {type: 'boolean'}, describe: {type: 'boolean'}, image: {type: 'string'}, digest: {type: 'string'},
    'base-worker-digest': {type: 'string'}, 'direct-base-digest': {type: 'string'},
    'qualifier-sha256': {type: 'string'}, 'source-envelope-sha256': {type: 'string'}, 'worker-package-sha256': {type: 'string'},
    revision: {type: 'string'}, version: {type: 'string'}, 'worker-version': {type: 'string'},
    'source-kind': {type: 'string'}, nonce: {type: 'string'},
  }});
  if (!values.run) return {run: false};
  assert(!values.describe, '--describe cannot also run');
  assert.match(values.version || '', /^\d+\.\d+\.\d+-alpha\.\d+$/);
  assert.match(values.revision || '', /^[0-9a-f]{40}$/);
  assert.match(values.image || '', /^registry\.fly\.io\/rudder-workers-exla:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$/);
  for (const key of ['digest', 'base-worker-digest']) assert.match(values[key] || '', digestPattern);
  for (const key of ['qualifier-sha256', 'source-envelope-sha256', 'worker-package-sha256']) assert.match(values[key] || '', shaPattern);
  const sourceKind = values['source-kind'] || 'candidate';
  assert(['candidate', 'committed-release'].includes(sourceKind));
  const workerVersion = values['worker-version'] || '4.0.0-alpha.12';
  assert.match(workerVersion, /^\d+\.\d+\.\d+-alpha\.\d+$/);
  const directBaseDigest = values['direct-base-digest'] || values['base-worker-digest'];
  assert.match(directBaseDigest, digestPattern);
  assert.notEqual(values.digest, directBaseDigest, 'qualification image must differ from direct base');
  assert.notEqual(values.digest, values['base-worker-digest'], 'qualification image must differ from stock worker');
  assert.notEqual(values.image, 'registry.fly.io/rudder-workers-exla:' + values.version);
  assert.notEqual(values.image, 'registry.fly.io/rudder-workers-exla:' + workerVersion);
  const nonce = values.nonce || randomUUID();
  assert.match(nonce, uuidPattern);
  return {
    run: true, image: values.image, digest: values.digest, nonce, sourceKind, workerVersion,
    version: values.version, revision: values.revision, directBaseDigest,
    baseWorkerDigest: values['base-worker-digest'], qualifierSha256: values['qualifier-sha256'],
    sourceEnvelopeSha256: values['source-envelope-sha256'], workerPackageSha256: values['worker-package-sha256'],
  };
}

export function provenanceFor(value) {
  return {
    formatVersion: 1, kind: ROLE, proposedVersion: value.version, workerVersion: value.workerVersion,
    sourceKind: value.sourceKind,
    baseRevision: value.sourceKind === 'candidate' ? value.revision : null,
    revision: value.sourceKind === 'committed-release' ? value.revision : null,
    sourceEnvelopeSha256: value.sourceEnvelopeSha256,
    baseWorkerDigest: value.baseWorkerDigest, directBaseDigest: value.directBaseDigest,
    qualifierSha256: value.qualifierSha256,
  };
}

// Kept pure so the *serialized actual parser* can be exercised on /proc and
// real tab/newline fixtures without running a preload or controller main.
export function parseProcStatus(text) {
  const wanted = ['Uid', 'Gid', 'Groups', 'NoNewPrivs', 'CapEff', 'CapPrm', 'CapInh', 'CapAmb', 'CapBnd'];
  const fields = new Map();
  for (const match of text.matchAll(/^([A-Za-z]+):[ \t]*([^\r\n]*)$/gm)) {
    if (wanted.includes(match[1])) fields.set(match[1], match[2].trim());
  }
  return Object.fromEntries(wanted.map(key => [key, fields.get(key)]));
}

// No outer state: this exact validator is included in the UID1500 preload.
// These checks validate the pinned qualifier's report; data cannot recreate
// its typed process/drain/State authority in this JavaScript controller.
export function validateRemoteProof(value, input, assert) {
  const integer = (value, min = 0, max = Number.MAX_SAFE_INTEGER) => {
    assert(Number.isSafeInteger(value) && value >= min && value <= max);
  };
  const digest = value => assert.match(value, /^[0-9a-f]{64}$/);
  const uuid = value => assert.match(value, /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
  const object = (value, kind) => {
    assert(value && value.kind === kind && value.algorithm === 'sha256'); digest(value.digest);
    integer(value.size_bytes, 0, 64 * 1024 * 1024);
  };
  const receipt = value => {
    assert.equal(value.format_version, 1); digest(value.set_digest); uuid(value.staging_id);
    assert.equal(typeof value.pin_id, 'string'); assert(value.pin_id.length > 0 && value.pin_id.length <= 4096);
    integer(value.generation, 1); integer(value.object_count, 1, 16384); integer(value.total_bytes, 1, 4 * 1024 ** 3);
  };
  const pipe = value => {
    digest(value.sha256); integer(value.bytes, 0, 1024 * 1024);
    integer(value.retained_bytes, 0, 64 * 1024);
    assert.equal(value.truncated, value.bytes > value.retained_bytes);
    assert(value.retained_bytes <= value.bytes);
  };
  assert(value && value.kind === 'remote-execution-qualification');
  assert.equal(value.format_version, 1); assert.equal(value.status, 'passed');
  assert.match(value.utc, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/);
  uuid(value.execution_id); uuid(value.request_id);
  assert.equal(typeof value.fixture_root, 'string');
  assert.match(value.fixture_root, new RegExp('^' + input.fixtureBase + '/rreq-[0-9a-f]{8}$'));
  assert.equal(value.execution_spec, 4); assert.equal(value.managed_binding, 3);
  assert.equal(value.network_readiness_protocol, 3); assert.equal(value.namespace_schema, 5);
  assert.equal(value.profile, 'offline-command-v1'); assert.equal(value.network, 'actual_private_offline_namespace');
  assert.equal(value.io, 'owned_pipes'); assert.equal(value.pty_qualified, false);
  for (const key of ['synthetic_authority', 'source_only', 'environment_free', 'empty_topology',
    'backend_ready_authenticated', 'guest_marker_checked', 'mmap_and_open_fd_and_descendant',
    'shutdown_typed_and_authenticated']) assert.equal(value[key], true);
  for (const key of ['provider_upload_qualified', 'vcs_application']) assert.equal(value[key], false);
  assert.equal(value.entries, 100101); assert.equal(value.inherited_file_logical_bytes, 208 * 1024 * 1024);
  integer(value.synthetic_graph_objects, 1, 4096); integer(value.synthetic_graph_bytes, 4 * 1024 * 1024, 64 * 1024 * 1024);
  object(value.inherited_directory, 'directory'); object(value.inherited_file, 'file'); object(value.inherited_payload, 'blob');
  assert.equal(value.inherited_payload.size_bytes, 4 * 1024 * 1024);
  const source = value.source;
  assert.equal(source.root_admissions, 1); integer(source.fetch_calls, 1, 4096);
  integer(source.fetched_bytes, 1, 64 * 1024 * 1024); integer(source.file_metadata_requests, 1, 4096);
  assert.equal(source.unchanged_payload_requests, 0); assert.equal(source.accepted_seed_requests, 0);
  assert.equal(source.active_fetches, 0); integer(source.maximum_active_fetches, 1, 8);
  const data = value.data;
  assert.equal(data.record_format, 2); assert.equal(data.artifact, 'partial_remote');
  assert.equal(data.capture_format, 1); assert.equal(data.capture_consistency, 'live_best_effort');
  uuid(data.stop_nonce); assert.notEqual(data.stop_nonce, value.request_id);
  object(data.partial_directory, 'directory'); object(data.freeze_intent, 'blob'); object(data.drain_record, 'blob');
  assert.notEqual(data.freeze_intent.digest, data.drain_record.digest);
  digest(data.freeze_bytes_sha256); digest(data.drain_bytes_sha256);
  assert.equal(data.freeze_bytes_sha256, data.freeze_intent.digest);
  assert.equal(data.drain_bytes_sha256, data.drain_record.digest);
  integer(data.local_objects, 3, 16384); integer(data.producer_objects, 2, 16000);
  assert.equal(data.local_objects, data.producer_objects + 1);
  integer(data.remote_boundaries, 1, 4096); integer(data.new_file_bytes, 1, 4096); digest(data.new_file_sha256);
  assert.equal(data.capture_retention.format_version, 1); object(data.capture_retention.plan, 'blob');
  receipt(data.capture_retention.local); assert.equal(data.capture_retention.local.object_count, data.local_objects);
  receipt(data.evidence_retention); assert.equal(data.evidence_retention.object_count, 2);
  assert.notEqual(data.evidence_retention.pin_id, data.capture_retention.local.pin_id);
  assert.equal(data.complete_pins, 0); assert.equal(data.gc.blocked_by_staging, false);
  integer(data.gc.retained_objects, data.local_objects + 2); integer(data.gc.removed_objects); integer(data.gc.removed_bytes);
  digest(value.record_sha256); integer(value.elapsed_ms, 0, 240000);
  assert.equal(value.historical_retry.outcome, 'historical'); assert.equal(value.historical_retry.factory_calls, 1);
  assert.equal(value.historical_retry.new_source, false); assert.equal(value.historical_retry.current_shutdown_authority, false);
  for (const pipeProof of [value.stdout, value.stderr, value.historical_retry.stdout, value.historical_retry.stderr]) pipe(pipeProof);
  const limits = value.limits;
  assert.equal(limits.max_entries, 100101); assert.equal(limits.case_timeout_seconds, 180);
  assert.equal(limits.process_alarm_seconds, 240); assert.equal(limits.source_fetch_calls, 4096);
  assert.equal(limits.source_fetched_bytes, 64 * 1024 * 1024);
  assert.equal(limits.complete_pin_or_acceptance, false); assert.equal(limits.host_fallback, false);
  return {qualified: 1, unsupported: 0, failed: 0, physicalRemoteCasesPassed: 1, historicalReopensPassed: 1};
}

export function validateGuestMarker(marker, summary, assert) {
  assert.equal(marker.request_id, summary.request_id); assert.equal(marker.entries, 100101);
  assert.equal(marker.logical_bytes, 208 * 1024 * 1024);
  for (const key of ['open_fd', 'mmap_open', 'descendant_started', 'zero_capabilities',
    'no_new_privileges', 'private_paths_absent']) assert.equal(marker[key], true);
  assert.deepEqual(marker.interfaces, ['lo']);
}

// Public, non-executable version fixture only. Its expected content hash comes
// from explicit trusted input, never from an observed file or baked manifest.
export function readWorkerPackage(input, fs, crypto, assert, metadata = {}) {
  assert.match(input.workerPackageSha256, /^[0-9a-f]{64}$/);
  const file = '/opt/rudder/package.json';
  const fd = fs.openSync(file, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW | fs.constants.O_NONBLOCK);
  try {
    const stat = fs.fstatSync(fd);
    Object.assign(metadata, {uid: stat.uid, gid: stat.gid, mode: stat.mode & 0o7777, nlink: stat.nlink, size: stat.size});
    assert(stat.isFile() && stat.nlink === 1 && stat.uid === 0 && stat.gid === 0
      && [0o644, 0o664].includes(stat.mode & 0o7777) && stat.size > 0 && stat.size <= 65536);
    const bytes = Buffer.alloc(65537); let count = 0;
    while (count < bytes.length) {const next = fs.readSync(fd, bytes, count, bytes.length - count, null);
      if (!next) break; count += next;}
    assert(count <= 65536 && count === stat.size);
    const end = fs.fstatSync(fd), named = fs.lstatSync(file);
    for (const current of [end, named]) assert(current.isFile() && !current.isSymbolicLink()
      && current.dev === stat.dev && current.ino === stat.ino && current.size === stat.size
      && current.uid === 0 && current.gid === 0 && current.nlink === 1 && current.mode === stat.mode);
    metadata.sha256 = crypto.createHash('sha256').update(bytes.subarray(0, count)).digest('hex');
    assert.equal(metadata.sha256, input.workerPackageSha256);
    const value = JSON.parse(bytes.subarray(0, count));
    assert.equal(value.version, input.provenance.workerVersion);
    return {version: value.version, metadata};
  } finally {fs.closeSync(fd);}
}

// Node22 exposes statfsSync(path), not fstatfsSync(fd). Resolve only the held
// descriptor through procfs and compare fstat identity across the nsfs check.
export function heldNamespaceWitness(fs, assert, kind) {
  assert(['user', 'net', 'mnt'].includes(kind));
  const fd = fs.openSync('/proc/self/ns/' + kind, fs.constants.O_RDONLY);
  try {
    const before = fs.fstatSync(fd, {bigint: true});
    assert.equal(fs.statfsSync('/proc/self/fd/' + fd).type, 0x6e736673);
    const after = fs.fstatSync(fd, {bigint: true});
    assert.equal(after.dev, before.dev); assert.equal(after.ino, before.ino);
    return {fd, device: before.dev.toString(), inode: before.ino.toString()};
  } catch (error) {fs.closeSync(fd); throw error;}
}

export function privateErrorDetail(error) {
  const source = String(error?.stack || error);
  const compact = source.replace(/data:[^\s,]+;base64,[A-Za-z0-9+/=]+/g, 'data:<inline-preload>');
  return {raw: Buffer.from(source), formatted: Buffer.from(compact), shortened: compact !== source};
}

export async function preload(input, validateRemoteProof, parseProcStatus, validateGuestMarker, readWorkerPackage, heldNamespaceWitness, privateErrorDetail) {
  // This independent VM-side fuse also ends a controller-disconnect canary.
  // The asynchronous child runner cannot block this event-loop timer.
  setTimeout(() => process.exit(124), 420000);
  const fs = await import('node:fs'), cp = await import('node:child_process');
  const crypto = await import('node:crypto'), {default: assert} = await import('node:assert/strict');
  const os = await import('node:os');
  const result = {formatVersion: 1, kind: 'alpha13-remote-execution-qualification', nonce: input.nonce,
    provenance: input.provenance, supervisorModuleEvaluated: false, productionWired: false,
    providerUploadQualified: false, actualBrokerGraceQualified: false, acceptanceQualified: false,
    ptyQualified: false, completed: false, qualified: false, attempted: 0};
  let stage = 'admission-gate', parentUser, parentNet, parentMnt;
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  const status = () => parseProcStatus(fs.readFileSync('/proc/self/status', 'utf8'));
  const read = (file, max, uid, mode) => {
    const fd = fs.openSync(file, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW | fs.constants.O_NONBLOCK);
    try {
      const stat = fs.fstatSync(fd);
      assert(stat.isFile() && stat.nlink === 1 && stat.uid === uid && (stat.mode & 0o777) === mode);
      assert(stat.size >= 0 && stat.size <= max);
      const bytes = Buffer.alloc(max + 1); let count = 0;
      while (count < bytes.length) {
        const next = fs.readSync(fd, bytes, count, bytes.length - count, null);
        if (!next) break; count += next;
      }
      assert(count <= max); return bytes.subarray(0, count);
    } finally { fs.closeSync(fd); }
  };
  const witness = kind => heldNamespaceWitness(fs, assert, kind);
  const identity = value => ({device: value.device, inode: value.inode});
  const current = kind => {const value = witness(kind); fs.closeSync(value.fd); return identity(value);};
  const digest = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
  const digestFile = file => {
    const fd = fs.openSync(file, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW | fs.constants.O_NONBLOCK);
    try {
      const stat = fs.fstatSync(fd);
      assert(stat.isFile() && stat.nlink === 1 && stat.uid === 0 && (stat.mode & 0o777) === 0o555);
      assert(stat.size > 0 && stat.size <= 256 * 1024 * 1024);
      const hash = crypto.createHash('sha256'), buffer = Buffer.alloc(64 * 1024); let count = 0;
      for (;;) {const next = fs.readSync(fd, buffer, 0, buffer.length, null); if (!next) break;
        count += next; assert(count <= stat.size && count <= 256 * 1024 * 1024); hash.update(buffer.subarray(0, next));}
      assert.equal(count, stat.size); return hash.digest('hex');
    } finally { fs.closeSync(fd); }
  };
  const writePrivate = (file, bytes) => {
    const fd = fs.openSync(file, fs.constants.O_WRONLY | fs.constants.O_CREAT | fs.constants.O_EXCL | fs.constants.O_NOFOLLOW, 0o600);
    try {fs.writeFileSync(fd, bytes); fs.fsyncSync(fd);} finally {fs.closeSync(fd);}
  };
  const pipeSummary = value => ({bytes: value.count, sha256: value.hash.digest('hex'),
    retainedBytes: value.bytes.length, truncated: value.count > value.bytes.length});
  const run = () => new Promise(resolve => {
    const child = cp.spawn(input.binary, ['qualify', '--base', input.fixtureBase, '--entries', '100101'], {
      cwd: input.home, env: {PATH: '/usr/local/bin:/usr/bin:/bin', HOME: input.home,
        RUDDER_HOME: input.home + '/rudder', CLAUDE_CONFIG_DIR: input.home + '/claude', CODEX_HOME: input.home + '/codex',
        TMPDIR: input.home + '/tmp', LANG: 'C', LC_ALL: 'C'}, stdio: ['ignore', 'pipe', 'pipe'],
    });
    const stdout = {bytes: Buffer.alloc(0), count: 0, hash: crypto.createHash('sha256')};
    const stderr = {bytes: Buffer.alloc(0), count: 0, hash: crypto.createHash('sha256')};
    let failure = null, status = null, signal = null, ended = false, settleTimer;
    const stop = reason => {
      if (ended) return;
      failure ||= reason; child.kill('SIGKILL');
      settleTimer ||= setTimeout(() => {failure ||= 'QUALIFIER_SETTLEMENT_DEADLINE'; finish(false);}, 2000);
    };
    const timer = setTimeout(() => stop('QUALIFIER_DEADLINE'), 255000);
    const collect = (target, max) => chunk => {
      if (ended) return;
      target.hash.update(chunk); target.count += chunk.length;
      const keep = chunk.subarray(0, Math.max(0, max - target.bytes.length));
      target.bytes = Buffer.concat([target.bytes, keep]);
      if (target.count > max) stop('QUALIFIER_OUTPUT_BOUND');
    };
    child.stdout.on('data', collect(stdout, 1024 * 1024)); child.stderr.on('data', collect(stderr, 8 * 1024));
    child.on('error', () => {failure ||= 'QUALIFIER_SPAWN';});
    child.on('exit', (code, sig) => {status = code; signal = sig;});
    const finish = settled => {
      if (ended) return;
      ended = true; clearTimeout(timer); if (settleTimer) clearTimeout(settleTimer);
      if (!settled) {
        child.stdout.destroy(); child.stderr.destroy(); child.unref();
      }
      try {
        writePrivate(input.home + '/qualifier.stdout.private', stdout.bytes);
        writePrivate(input.home + '/qualifier.stderr.private', stderr.bytes);
      } catch {failure ||= 'SAVE_PRIVATE_CHILD_LOGS';}
      resolve({status, signal, failure, settled, stdoutBytes: stdout.bytes,
        stdout: pipeSummary(stdout), stderr: pipeSummary(stderr)});
    };
    child.on('close', () => finish(true));
  });
  try {
    const deadline = Date.now() + 120000; let admitted = false;
    while (Date.now() < deadline) {
      try {
        // link+unlink publication may briefly have nlink2. It cannot admit
        // effects until publication has settled to the exact single-link file.
        let gate;
        try {gate = read(input.gatePath, 4096, 1500, 0o600);} catch (error) {
          if (error.code === 'ENOENT') throw error;
          const meta = fs.lstatSync(input.gatePath);
          if (meta.isFile() && meta.uid === 1500 && meta.nlink === 2) {await sleep(20); continue;}
          throw error;
        }
        assert.deepEqual(JSON.parse(gate), input.gate); admitted = true; break;
      } catch (error) {if (error.code !== 'ENOENT') throw error; await sleep(100);}
    }
    assert(admitted); result.admittedAfterResolvedDigest = true;
    stage = 'boot-identity';
    assert.equal(Object.keys(process.env).filter(key => /TOKEN|SECRET|PASSWORD|API_KEY|CREDENTIAL|AUTH_KEY|SSH_AUTH_SOCK|AWS_ACCESS_KEY/i.test(key)).length, 0);
    assert.equal(process.getuid(), 1500); assert.equal(process.getgid(), 1500);
    const initial = status();
    assert.equal(initial.Uid, '1500\t1500\t1500\t1500'); assert.equal(initial.Gid, '1500\t1500\t1500\t1500');
    assert.equal(initial.Groups, ''); assert.equal(initial.NoNewPrivs, '1');
    for (const key of ['CapEff', 'CapPrm', 'CapInh', 'CapAmb', 'CapBnd']) assert.match(initial[key], /^0+$/);
    assert.equal(process.env.RUDDER_WORKER_MANAGED_EXECUTION, '1'); assert.equal(process.env.RUDDER_WORKER_SKIP_AGENT_UPDATE, '1');
    assert.equal(process.env.RUDDER_WORKER_FUSE_READY, '1');
    const workerPackage = readWorkerPackage(input, fs, crypto, assert), workerVersion = workerPackage.version;
    result.boot = {uid: process.getuid(), gid: process.getgid(), fields: initial, node: process.version,
      kernel: os.release(), workerVersion, workerPackage: workerPackage.metadata, credentialEnvironmentFields: 0};
    stage = 'baked-provenance';
    const directory = fs.lstatSync('/opt/rudder/qualification');
    assert(directory.isDirectory() && !directory.isSymbolicLink() && directory.uid === 0 && !(directory.mode & 0o022));
    const manifest = read(input.provenancePath, 4096, 0, 0o444);
    assert.deepEqual(JSON.parse(manifest), input.provenance); result.provenanceSha256 = digest(manifest);
    assert.equal(digestFile(input.binary), input.provenance.qualifierSha256);
    result.qualifierSha256 = input.provenance.qualifierSha256;
    stage = 'public-runtime-and-fuse';
    const python = fs.statSync('/usr/bin/python3');
    assert(python.isFile() && python.uid === 0 && !(python.mode & 0o022)); fs.accessSync('/usr/bin/python3', fs.constants.X_OK);
    assert(fs.realpathSync('/usr/bin/python3').startsWith('/usr/'));
    const fuse = fs.lstatSync('/dev/fuse');
    assert(fuse.isCharacterDevice() && fuse.uid === 1500 && fuse.gid === 1500 && (fuse.mode & 0o777) === 0o600);
    const fuseFd = fs.openSync('/dev/fuse', 'r+'); fs.closeSync(fuseFd); result.fuseOpenedAsUid1500 = true;
    assert.equal(fs.statSync('/workspace').uid, 1500);
    stage = 'private-fixtures';
    for (const file of [input.fixtureBase, input.home]) {
      assert.match(file, /^\/workspace\/(qe|qh)-[0-9a-f]{8}$/); assert(!fs.existsSync(file));
      fs.mkdirSync(file, {mode: 0o700});
      const stat = fs.lstatSync(file); assert(stat.isDirectory() && !stat.isSymbolicLink() && stat.uid === 1500 && (stat.mode & 0o777) === 0o700);
    }
    for (const name of ['rudder', 'claude', 'codex', 'tmp']) fs.mkdirSync(input.home + '/' + name, {mode: 0o700});
    parentUser = witness('user'); parentNet = witness('net'); parentMnt = witness('mnt');
    result.parentBefore = {user: identity(parentUser), net: identity(parentNet), mnt: identity(parentMnt)};
    stage = 'actual-remote-execution'; result.attempted = 1;
    const child = await run(); result.child = {status: child.status, signal: child.signal, failure: child.failure, settled: child.settled,
      stdout: child.stdout, stderr: child.stderr, privateDiagnosticsDirectory: input.home};
    result.parentAfter = {user: current('user'), net: current('net'), mnt: current('mnt')};
    assert.deepEqual(result.parentAfter, result.parentBefore); assert.deepEqual(status(), initial);
    result.parentNamespacesAndStatusUnchanged = true;
    assert(!child.failure); assert.equal(child.settled, true); assert.equal(child.signal, null); assert.equal(child.status, 0);
    const lines = child.stdoutBytes.toString('utf8').trim().split('\n').filter(Boolean); assert.equal(lines.length, 1);
    const summary = JSON.parse(lines[0]);
    assert(Buffer.byteLength(JSON.stringify(summary, null, 2)) <= 512 * 1024);
    stage = 'validate-typed-qualification-report';
    result.counts = validateRemoteProof(summary, input, assert);
    stage = 'owned-guest-readback';
    const markerPath = summary.fixture_root + '/executions/runs/' + summary.execution_id + '/scratch/remote-guest-ready.json';
    const marker = JSON.parse(read(markerPath, 4096, 1500, 0o600)); validateGuestMarker(marker, summary, assert);
    const owned = fs.lstatSync(summary.fixture_root);
    assert(owned.isDirectory() && !owned.isSymbolicLink() && owned.uid === 1500 && (owned.mode & 0o777) === 0o700);
    result.guest = {uid: owned.uid, zeroFiveCapabilitySets: marker.zero_capabilities,
      noNewPrivileges: marker.no_new_privileges, interfaces: marker.interfaces, marker};
    result.summary = summary; result.summarySha256 = digest(Buffer.from(lines[0]));
    result.completed = true; result.qualified = true;
  } catch (error) {
    result.failedStage = stage; result.errorCode = typeof error.code === 'string' ? error.code : 'ASSERTION';
    result.completed = false; result.qualified = false;
    const detail = privateErrorDetail(error), retained = detail.formatted.subarray(0, 8192);
    result.privateFailure = {path: input.failurePath, sha256: digest(detail.raw), bytes: detail.raw.length,
      formattedBytes: detail.formatted.length, formattedSha256: digest(detail.formatted),
      retainedBytes: retained.length, retainedSha256: digest(retained), dataUrlFramesShortened: detail.shortened};
    try {writePrivate(input.failurePath, retained);} catch {}
  } finally {
    if (parentUser) fs.closeSync(parentUser.fd); if (parentNet) fs.closeSync(parentNet.fd); if (parentMnt) fs.closeSync(parentMnt.fd);
    if (!result.qualified) result.counts = {qualified: 0, unsupported: result.child?.status === 77 ? 1 : 0,
      failed: result.child?.status === 77 ? 0 : 1, physicalRemoteCasesPassed: 0, historicalReopensPassed: 0};
    result.verifiedAt = new Date().toISOString();
    const bytes = Buffer.from(JSON.stringify(result, null, 2) + '\n'); assert(bytes.length <= 1024 * 1024);
    writePrivate(input.proofPath, bytes);
  }
  // Never return to evaluate supervisor/home, even after failed qualification.
  await new Promise(() => {});
}

export function preloadSource(input) {
  return 'await (' + preload.toString() + ')(' + JSON.stringify(input) + ',' + validateRemoteProof.toString() + ','
    + parseProcStatus.toString() + ',' + validateGuestMarker.toString() + ',' + readWorkerPackage.toString() + ','
    + heldNamespaceWitness.toString() + ',' + privateErrorDetail.toString() + ');\n';
}

export function plan(value) {
  const nonce = value.nonce, name = 'rudder-alpha13-remotequal-' + nonce.slice(0, 8);
  const provenance = provenanceFor(value);
  const input = {nonce, digest: value.digest, provenance, workerPackageSha256: value.workerPackageSha256, binary: BINARY, provenancePath: PROVENANCE,
    fixtureBase: '/workspace/qe-' + nonce.slice(0, 8), home: '/workspace/qh-' + nonce.slice(0, 8),
    gatePath: '/workspace/remote-execution-gate-' + nonce + '.json', proofPath: '/workspace/remote-execution-proof-' + nonce + '.json',
    failurePath: '/workspace/remote-execution-failure-' + nonce + '.private'};
  input.gate = {formatVersion: 1, nonce, digest: value.digest, revision: value.revision,
    sourceKind: value.sourceKind, sourceEnvelopeSha256: value.sourceEnvelopeSha256, workerPackageSha256: value.workerPackageSha256};
  const source = preloadSource(input);
  const config = {image: value.image, guest: {cpu_kind: 'performance', cpus: 1, memory_mb: 2048},
    restart: {policy: 'no'}, auto_destroy: false, services: [], mounts: [],
    env: {RUDDER_WORKER_MANAGED_EXECUTION: '1', RUDDER_HOME: '1', RUDDER_WORKER_SKIP_AGENT_UPDATE: '1',
      NODE_OPTIONS: '--import=data:text/javascript;base64,' + Buffer.from(source).toString('base64')},
    metadata: {rudder_role: ROLE, qualification: nonce, qualification_revision: value.revision,
      qualification_source_kind: value.sourceKind, qualification_source_envelope: value.sourceEnvelopeSha256}};
  return {nonce, name, role: ROLE, provenance, input, source, config};
}

// Ownership is bound to actual observed creation, not the expected successful
// digest. A wrong expected image must still permit exact-owned cleanup; a later
// different observed image/configuration must not inherit that ownership.
export function ownershipFor(prepared, value) {
  let owned, createdDescriptor;
  const empty = item => item === undefined || Array.isArray(item) && item.length === 0;
  const requested = machine => {
    if (!machine || !/^[0-9a-f]{14}$/.test(machine.id) || machine.id === FORBIDDEN || machine.name !== prepared.name) return false;
    const config = machine.config;
    if (!config || !Object.entries(prepared.config.metadata).every(([key, item]) => config.metadata?.[key] === item)) return false;
    const image = config.image, digest = machine.image_ref?.digest;
    if (!(image === prepared.config.image || typeof image === 'string' && image.startsWith(prepared.config.image + '@')
      && /^sha256:[0-9a-f]{64}$/.test(image.slice(prepared.config.image.length + 1))
      && (!digest || image === prepared.config.image + '@' + digest))) return false;
    if (digest !== undefined && !/^sha256:[0-9a-f]{64}$/.test(digest)) return false;
    if (!['services', 'mounts', 'files'].every(key => empty(config[key]))) return false;
    if (!Object.entries(prepared.config.guest).every(([key, item]) => config.guest?.[key] === item)) return false;
    if (!Object.entries(prepared.config.env).every(([key, item]) => config.env?.[key] === item)) return false;
    if (!Object.entries(config.env || {}).every(([key, item]) => Object.hasOwn(prepared.config.env, key)
      || ['PRIMARY_REGION', 'FLY_PRIMARY_REGION'].includes(key) && item === 'sjc')) return false;
    return true;
  };
  const matches = machine => requested(machine) && (!owned || machine.id === owned)
    && (!createdDescriptor?.observedDigest || machine.image_ref?.digest === createdDescriptor.observedDigest);
  const remember = machine => {
    assert(matches(machine), 'exact machine ownership mismatch');
    if (owned) assert.equal(machine.id, owned); else owned = machine.id;
    if (!createdDescriptor) createdDescriptor = {id: owned, name: prepared.name, nonce: prepared.nonce, role: ROLE,
      revision: value.revision, sourceKind: value.sourceKind, sourceEnvelopeSha256: value.sourceEnvelopeSha256,
      requestedImage: prepared.config.image, observedDigest: machine.image_ref?.digest ?? null, services: 0, mounts: 0, files: 0};
    else if (!createdDescriptor.observedDigest && machine.image_ref?.digest) createdDescriptor.observedDigest = machine.image_ref.digest;
    return {...createdDescriptor};
  };
  return {matches, remember, id: () => owned, descriptor: () => createdDescriptor && {...createdDescriptor}};
}

export function rootProbeSource(input) {
  return `const fs=require('node:fs'),crypto=require('node:crypto'),assert=require('node:assert/strict');
const input=${JSON.stringify(input)},result={formatVersion:1,nonce:input.nonce,uid:process.getuid()};let stage='root-identity';
const readWorkerPackage=${readWorkerPackage.toString()};
try{assert.equal(process.getuid(),0);
for(const p of ['/usr/local/bin','/opt/rudder/qualification']){const m=fs.lstatSync(p);assert(m.isDirectory()&&!m.isSymbolicLink()&&m.uid===0&&!(m.mode&0o022));}
const read=(p,max,mode)=>{const fd=fs.openSync(p,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);try{const m=fs.fstatSync(fd);assert(m.isFile()&&m.nlink===1&&m.uid===0&&(m.mode&0o777)===mode&&m.size<=max);const b=Buffer.alloc(max+1);let n=0;for(;;){const q=fs.readSync(fd,b,n,b.length-n,null);if(!q)break;n+=q;assert(n<=max);}return b.subarray(0,n);}finally{fs.closeSync(fd);}};
stage='provenance';
const manifest=read(input.provenancePath,4096,0o444);assert.deepEqual(JSON.parse(manifest),input.provenance);
stage='qualifier-binary';
const binary=read(input.binary,256*1024*1024,0o555);assert(binary.length>0);const digest=crypto.createHash('sha256').update(binary).digest('hex');assert.equal(digest,input.provenance.qualifierSha256);
stage='package-fixture-metadata';
result.packageMetadata={};readWorkerPackage(input,fs,crypto,assert,result.packageMetadata);
stage='public-python';
const python=fs.statSync('/usr/bin/python3');assert(python.isFile()&&python.uid===0&&!(python.mode&0o022));fs.accessSync('/usr/bin/python3',fs.constants.X_OK);
Object.assign(result,{status:'passed',qualifierSha256:digest,provenanceSha256:crypto.createHash('sha256').update(manifest).digest('hex'),provenance:input.provenance});
}catch(error){result.status='failed';result.failedStage=stage;result.errorCode=typeof error.code==='string'?error.code:'ASSERTION';process.stderr.write(Buffer.from(String(error?.stack||error)).subarray(0,8192));process.exitCode=1;}
console.log(JSON.stringify(result));`;
}

export function gateSource(input) {
  return `const fs=require('node:fs'),crypto=require('node:crypto');const target=${JSON.stringify(input.gatePath)},tmp=target+'.pending-'+crypto.randomUUID();
const fd=fs.openSync(tmp,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK,0o600);
try{fs.writeFileSync(fd,${JSON.stringify(JSON.stringify(input.gate))});fs.fchownSync(fd,1500,1500);fs.fsyncSync(fd);}finally{fs.closeSync(fd);}
try{fs.linkSync(tmp,target);}finally{fs.unlinkSync(tmp);}const directory=fs.openSync('/workspace',fs.constants.O_RDONLY|fs.constants.O_DIRECTORY);try{fs.fsyncSync(directory);}finally{fs.closeSync(directory);}
console.log(JSON.stringify({nonce:${JSON.stringify(input.nonce)},published:true}));`;
}

export function proofReadSource(input) {
  return `const fs=require('node:fs');const p=${JSON.stringify(input.proofPath)};let fd;
try{fd=fs.openSync(p,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);const m=fs.fstatSync(fd);if(!m.isFile()||m.nlink!==1||m.uid!==1500||(m.mode&0o777)!==0o600||m.size>1048576)throw Error('unsafe-proof');
const b=Buffer.alloc(1048577);let n=0;for(;;){const q=fs.readSync(fd,b,n,b.length-n,null);if(!q)break;n+=q;if(n>1048576)throw Error('oversize-proof');}console.log(JSON.stringify({status:'ready',sha256:require('node:crypto').createHash('sha256').update(b.subarray(0,n)).digest('hex'),proof:JSON.parse(b.subarray(0,n))}));}
catch(e){console.log(JSON.stringify({status:e.code==='ENOENT'?'pending':'rejected'}));}finally{if(fd!==undefined)fs.closeSync(fd);}`;
}

// Every owned exec saves its exact bounded pipes before these checks run.
// Fly may return CLI exit0 while reporting the guest exit on stdout.
export function parseOwnedExecJson(stdout, step) {
  const fail = (code, guestExitCode) => {
    step.ok = false; step.failure = code; step.validationFailure = code;
    if (guestExitCode !== undefined) step.guestExitCode = guestExitCode;
    throw Object.assign(new Error('owned exec rejected; inspect its private pipe files'), {code});
  };
  for (const match of stdout.matchAll(/^Exit code:[ \t]*(-?[0-9]{1,10})[ \t]*\r?$/gm)) {
    const code = Number(match[1]); if (code !== 0) fail('OWNED_GUEST_EXIT_NONZERO', code);
  }
  let value;
  try {value = JSON.parse(stdout.trim());} catch {fail('OWNED_EXEC_INVALID_JSON');}
  if (value?.status === 'failed') fail('OWNED_GUEST_REPORTED_FAILURE');
  step.jsonValidated = true; return value;
}

export async function preserveOwnedExecOutput(stdout, stderr, stdoutPath, stderrPath, write = fs.writeFile) {
  await write(stdoutPath, stdout, {mode: 0o600, flag: 'wx'});
  await write(stderrPath, stderr, {mode: 0o600, flag: 'wx'});
  return {privateStdoutFile: stdoutPath, privateStderrFile: stderrPath,
    retainedStdoutBytes: stdout.length, retainedStderrBytes: stderr.length};
}

// Runs only on the exact owned disposable after failure. It opens fixed nonce
// paths by held directory FDs and never reads a general HOME, source context,
// provider profile, environment, journal or arbitrary guest-supplied pathname.
export function collectFixtureDiagnostics(input) {
  const fs = require('node:fs'), crypto = require('node:crypto'), assert = require('node:assert/strict');
  assert.match(input.nonce, /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
  assert.equal(input.fixtureBase, '/workspace/qe-' + input.nonce.slice(0, 8));
  assert.equal(input.home, '/workspace/qh-' + input.nonce.slice(0, 8));
  assert.equal(input.failurePath, '/workspace/remote-execution-failure-' + input.nonce + '.private');
  const result = {formatVersion: 1, kind: 'owned-remote-execution-private-diagnostics', nonce: input.nonce,
    fixtureBase: input.fixtureBase, home: input.home, fixtureName: null, files: []};
  const flags = fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW | fs.constants.O_NONBLOCK;
  const beneath = (fd, leaf) => {
    assert(/^[A-Za-z0-9_.-]+$/.test(leaf) && !['.', '..'].includes(leaf));
    return '/proc/self/fd/' + fd + '/' + leaf;
  };
  const directory = (name, root = false) => {
    const fd = fs.openSync(name, flags | fs.constants.O_DIRECTORY);
    try {
      const stat = fs.fstatSync(fd);
      assert(stat.isDirectory() && stat.uid === 1500 && (root || (stat.mode & 0o777) === 0o700));
      return fd;
    } catch (error) {fs.closeSync(fd); throw error;}
  };
  const missing = (label, source, status = 'missing-parent') => result.files.push({label, source, status});
  const read = (parent, leaf, label, source, max) => {
    let fd;
    try {
      const name = beneath(parent, leaf);
      fd = fs.openSync(name, flags);
      const stat = fs.fstatSync(fd);
      assert(stat.isFile() && stat.nlink === 1 && stat.uid === 1500 && (stat.mode & 0o777) === 0o600);
      assert(stat.size >= 0 && stat.size <= max);
      const bytes = Buffer.alloc(max + 1); let count = 0;
      while (count < bytes.length) {const next = fs.readSync(fd, bytes, count, bytes.length - count, null);
        if (!next) break; count += next;}
      assert(count <= max && count === stat.size);
      const end = fs.fstatSync(fd), named = fs.lstatSync(name);
      for (const current of [end, named]) assert(current.isFile() && !current.isSymbolicLink()
        && current.dev === stat.dev && current.ino === stat.ino && current.uid === 1500
        && (current.mode & 0o777) === 0o600 && current.nlink === 1 && current.size === stat.size);
      const body = bytes.subarray(0, count);
      result.files.push({label, source, status: 'read', bytes: count,
        sha256: crypto.createHash('sha256').update(body).digest('hex'),
        metadata: {uid: stat.uid, gid: stat.gid, mode: stat.mode & 0o777, nlink: stat.nlink, size: stat.size},
        bodyBase64: body.toString('base64')});
    } catch (error) {
      result.files.push({label, source, status: error.code === 'ENOENT' ? 'missing' : 'rejected',
        errorCode: typeof error.code === 'string' ? error.code : 'UNSAFE_OR_OVERSIZE'});
    } finally {if (fd !== undefined) fs.closeSync(fd);}
  };
  let workspace, home, base, fixture;
  const sourceLabels = ['helper-failure', 'helper-initial-stderr', 'helper-historical-stderr'];
  try {
    workspace = directory('/workspace', true);
    read(workspace, input.failurePath.slice('/workspace/'.length), 'preload-failure', input.failurePath, 8192);
    try {
      home = directory(beneath(workspace, input.home.slice('/workspace/'.length)));
      read(home, 'qualifier.stderr.private', 'qualifier-stderr', input.home + '/qualifier.stderr.private', 8192);
    } catch (error) {
      missing('qualifier-stderr', input.home + '/qualifier.stderr.private', error.code === 'ENOENT' ? 'missing-parent' : 'rejected-parent');
    }
    try {
      base = directory(beneath(workspace, input.fixtureBase.slice('/workspace/'.length)));
      const entries = fs.opendirSync('/proc/self/fd/' + base, {bufferSize: 2});
      let first, second;
      try {first = entries.readSync(); second = entries.readSync();} finally {entries.closeSync();}
      assert(first && !second && /^rreq-[0-9a-f]{8}$/.test(first.name));
      fixture = directory(beneath(base, first.name)); result.fixtureName = first.name;
      const prefix = input.fixtureBase + '/' + first.name + '/';
      read(fixture, 'failure.private.txt', sourceLabels[0], prefix + 'failure.private.txt', 65536);
      read(fixture, 'initial.stderr', sourceLabels[1], prefix + 'initial.stderr', 65536);
      read(fixture, 'historical.stderr', sourceLabels[2], prefix + 'historical.stderr', 65536);
    } catch (error) {
      for (const label of sourceLabels) missing(label, null, error.code === 'ENOENT' ? 'missing-parent' : 'rejected-parent');
    }
    result.completed = true;
  } catch (error) {
    result.completed = false; result.errorCode = typeof error.code === 'string' ? error.code : 'UNSAFE_WORKSPACE';
  } finally {
    for (const fd of [fixture, base, home, workspace]) if (fd !== undefined) fs.closeSync(fd);
  }
  const encoded = JSON.stringify(result); assert(Buffer.byteLength(encoded) <= 512 * 1024);
  console.log(encoded);
}

export function fixtureDiagnosticsSource(input) {
  return '(' + collectFixtureDiagnostics.toString() + ')(' + JSON.stringify(input) + ');';
}

export async function saveFixtureDiagnostics(bundle, input, directory, write = fs.writeFile) {
  assert.equal(bundle?.formatVersion, 1); assert.equal(bundle.kind, 'owned-remote-execution-private-diagnostics');
  assert.equal(bundle.nonce, input.nonce); assert.equal(bundle.fixtureBase, input.fixtureBase); assert.equal(bundle.home, input.home);
  assert(Array.isArray(bundle.files) && bundle.files.length <= 5);
  if (bundle.fixtureName !== null) assert.match(bundle.fixtureName, /^rreq-[0-9a-f]{8}$/);
  const prefix = bundle.fixtureName === null ? null : input.fixtureBase + '/' + bundle.fixtureName + '/';
  const allowed = {
    'preload-failure': [input.failurePath, 8192],
    'qualifier-stderr': [input.home + '/qualifier.stderr.private', 8192],
    'helper-failure': [prefix && prefix + 'failure.private.txt', 65536],
    'helper-initial-stderr': [prefix && prefix + 'initial.stderr', 65536],
    'helper-historical-stderr': [prefix && prefix + 'historical.stderr', 65536],
  };
  const seen = new Set(), normalized = [], pending = [];
  for (const file of bundle.files) {
    assert(Object.hasOwn(allowed, file.label) && !seen.has(file.label)); seen.add(file.label);
    const [source, max] = allowed[file.label]; assert.equal(file.source, source);
    assert(['read', 'missing', 'missing-parent', 'rejected', 'rejected-parent'].includes(file.status));
    const metadata = {label: file.label, source, status: file.status};
    if (file.status === 'read') {
      assert(source); assert.equal(typeof file.bodyBase64, 'string'); assert(file.bodyBase64.length <= 4 * Math.ceil(max / 3));
      const bytes = Buffer.from(file.bodyBase64, 'base64'); assert.equal(bytes.toString('base64'), file.bodyBase64);
      assert(bytes.length <= max); assert.equal(file.bytes, bytes.length); assert.equal(file.sha256, sha(bytes));
      assert.equal(file.metadata.uid, 1500); assert.equal(file.metadata.mode, 0o600); assert.equal(file.metadata.nlink, 1);
      assert.equal(file.metadata.size, bytes.length);
      const destination = path.join(directory, 'failure-diagnostics-' + file.label + '.private');
      Object.assign(metadata, {bytes: bytes.length, sha256: file.sha256, file: destination});
      pending.push([destination, bytes]);
    } else {assert(!Object.hasOwn(file, 'bodyBase64'));}
    normalized.push(metadata);
  }
  // Validate the entire returned allowlist before writing even one body.
  for (const [destination, bytes] of pending) await write(destination, bytes, {mode: 0o600, flag: 'wx'});
  const manifest = {attempted: true, completed: bundle.completed === true, files: normalized};
  await write(path.join(directory, 'failure-diagnostics-manifest.json'), Buffer.from(JSON.stringify(manifest, null, 2) + '\n'), {mode: 0o600, flag: 'wx'});
  return manifest;
}

export async function attemptFailureDiagnostics(collect, save) {
  try {return await save(await collect());}
  catch (error) {return {attempted: true, completed: false, errorCode: typeof error.code === 'string' ? error.code : 'DIAGNOSTICS_REJECTED'};}
}

// Pipes are bounded separately. Never print raw provider stdout/stderr or
// numeric-PGID signal after a leader could have been reaped. The owned Child
// handle alone is terminated; uncertain creates reconcile by exact metadata.
async function command(args, {timeout, stdoutLimit, diagnosticPath, ownedStdoutPath}) {
  return new Promise(resolve => {
    const child = spawn(FLY, args, {stdio: ['ignore', 'pipe', 'pipe']});
    let status, signal, failure, ended = false, settleTimer, stdout = Buffer.alloc(0), stderr = Buffer.alloc(0), stdoutBytes = 0, stderrBytes = 0;
    const stdoutHash = createHash('sha256'), stderrHash = createHash('sha256');
    const stop = code => {
      if (ended) return;
      failure ||= code; child.kill('SIGKILL');
      settleTimer ||= setTimeout(() => {failure ||= 'CLI_SETTLEMENT_DEADLINE'; finish(false);}, 2000);
    };
    const timer = setTimeout(() => stop('DEADLINE'), timeout);
    child.stdout.on('data', bytes => {
      if (ended) return;
      stdoutHash.update(bytes); stdoutBytes += bytes.length;
      const keep = bytes.subarray(0, Math.max(0, stdoutLimit - stdout.length)); stdout = Buffer.concat([stdout, keep]);
      if (stdoutBytes > stdoutLimit) stop('STDOUT_BOUND');
    });
    child.stderr.on('data', bytes => {
      if (ended) return;
      stderrHash.update(bytes); stderrBytes += bytes.length;
      stderr = Buffer.concat([stderr, bytes.subarray(0, Math.max(0, MAX_STDERR - stderr.length))]);
      if (stderrBytes > MAX_STDERR) stop('STDERR_BOUND');
    });
    child.on('error', () => {failure ||= 'SPAWN';});
    child.on('exit', (code, sig) => {status = code; signal = sig;});
    const finish = async settled => {
      if (ended) return;
      ended = true; clearTimeout(timer); if (settleTimer) clearTimeout(settleTimer);
      if (!settled) {child.stdout.destroy(); child.stderr.destroy(); child.unref();}
      const diagnostics = {status, signal, failure: failure || null, settled, stdoutBytes, stderrBytes,
        stdoutSha256: stdoutHash.digest('hex'), stderrSha256: stderrHash.digest('hex'),
        retainedStdoutBytes: stdout.length, retainedStderrBytes: stderr.length};
      try {
        if (ownedStdoutPath) Object.assign(diagnostics,
          await preserveOwnedExecOutput(stdout, stderr, ownedStdoutPath, diagnosticPath));
        else if (failure || status !== 0) {
          await fs.writeFile(diagnosticPath, stderr, {mode: 0o600, flag: 'wx'});
          diagnostics.privateStderrFile = diagnosticPath;
        }
        resolve({ok: settled && !failure && status === 0 && !signal, stdout: stdout.toString('utf8'), diagnostics});
      } catch {resolve({ok: false, stdout: '', diagnostics: {...diagnostics, failure: 'SAVE_PRIVATE_DIAGNOSTICS'}});}
    };
    child.on('close', () => finish(true));
  });
}

export async function main(argv) {
  const value = options(argv);
  if (!value.run) return {kind: ROLE, status: 'prepared-no-run', providerCalls: 0,
    usage: '--run --image TAG --digest sha256:... --base-worker-digest sha256:... --qualifier-sha256 HEX --source-envelope-sha256 HEX --worker-package-sha256 HEX --revision SHA40 --version VERSION [--source-kind candidate|committed-release] [--worker-version VERSION] [--direct-base-digest sha256:...] [--nonce UUID]',
    productionWired: false};
  const started = Date.now(), mainDeadline = started + MAIN_MS, lifetimeDeadline = started + LIFETIME_MS;
  const prepared = plan(value), ownership = ownershipFor(prepared, value);
  const directory = path.join(path.dirname(fileURLToPath(import.meta.url)), 'remote-execution-' + value.version + '-' + value.nonce);
  await fs.mkdir(directory, {mode: 0o700});
  const reportPath = path.join(directory, 'report.json');
  const report = {formatVersion: 1, kind: ROLE, name: prepared.name, nonce: prepared.nonce,
    requestedImage: value.image, expectedDigest: value.digest, provenance: prepared.provenance, workerPackageSha256: value.workerPackageSha256,
    startedAt: new Date(started).toISOString(), productionWired: false, sourceAcceptanceQualified: false,
    actualBrokerGraceQualified: false, providerUploadQualified: false, steps: [], maxCanaryLifetimeSeconds: 540,
    limits: {proofBytes: MAX_PROOF, errorStderrBytes: MAX_STDERR, childTimeoutSeconds: 255, preloadFuseSeconds: 420,
      baseAncestry: 'requires Root retained pinned-FROM and source-envelope build evidence'}};
  const emit = step => console.log(JSON.stringify({step, name: prepared.name, nonce: prepared.nonce,
    role: ROLE, expectedDigest: value.digest, sourceIdentity: prepared.provenance, report: reportPath}));
  await fs.writeFile(path.join(directory, 'preload.mjs'), prepared.source, {mode: 0o600, flag: 'wx'});
  const configPath = path.join(directory, 'machine.json');
  await fs.writeFile(configPath, JSON.stringify(prepared.config, null, 2) + '\n', {mode: 0o600, flag: 'wx'});
  report.controllerSha256 = sha(await fs.readFile(fileURLToPath(import.meta.url)));
  report.preloadSha256 = sha(Buffer.from(prepared.source)); report.configSha256 = sha(await fs.readFile(configPath));
  let phase = 'preflight', serial = 0, cleanup = false, diagnosing = false;
  const call = async (args, label, timeout = 15000, stdoutLimit = 4 * MAX_PROOF, ownedOutput = false) => {
    phase = label;
    const remaining = (cleanup ? lifetimeDeadline : diagnosing ? Math.min(lifetimeDeadline - 70000, Date.now() + 20000) : mainDeadline) - Date.now();
    assert(remaining > 0, 'controller lifetime exhausted');
    assert.match(label, /^[a-z][a-z0-9-]{0,80}$/);
    const prefix = path.join(directory, String(++serial).padStart(3, '0') + '-' + label);
    const began = Date.now(), response = await command(args, {timeout: Math.min(timeout, remaining), stdoutLimit,
      diagnosticPath: prefix + '-stderr.private', ownedStdoutPath: ownedOutput ? prefix + '-stdout.private' : undefined});
    const step = {step: label, ms: Date.now() - began, ok: response.ok, ...response.diagnostics};
    report.steps.push(step);
    assert(response.ok, 'bounded provider command failed');
    if (ownedOutput) return parseOwnedExecJson(response.stdout, step);
    return response.stdout;
  };
  const inventory = async () => {
    const result = JSON.parse(await call(['machine', 'list', '-a', APP, '--json'], 'filtered-inventory'));
    assert(Array.isArray(result)); assert(result.length <= 10000); return result;
  };
  const noSecrets = async label => {
    assert.deepEqual(JSON.parse(await call(['secrets', 'list', '-a', APP, '--json'], label)), []);
  };
  const ownedExec = async (script, label, stdoutLimit = 4 * MAX_PROOF) => {
    const id = ownership.id(); assert(id && id !== FORBIDDEN);
    const current = (await inventory()).filter(machine => machine.id === id); assert.equal(current.length, 1);
    assert(ownership.matches(current[0]));
    return call(['machine', 'exec', id, '-a', APP, '/bin/sh -c ' + quote(script), '--timeout', '15'], label, 20000, stdoutLimit, true);
  };
  try {
    await noSecrets('assert-empty-app-secrets');
    const before = await inventory();
    assert(!before.some(machine => machine.name === prepared.name || machine.config?.metadata?.qualification === prepared.nonce));
    report.noAppSecretsAtCreation = true; emit('creating-exact-owned-remote-qualification');
    try {
      await call(['machine', 'run', value.image, '-a', APP, '--machine-config', configPath, '--name', prepared.name,
        '--region', 'sjc', '--detach', '--skip-dns-registration'], 'create-owned-remote-qualification', 70000);
    } catch {report.createReplyUncertain = true;}
    let machine, startRequested = false;
    const startDeadline = Math.min(mainDeadline, Date.now() + 90000);
    while (Date.now() < startDeadline) {
      const matches = (await inventory()).filter(ownership.matches); assert(matches.length <= 1);
      if (matches.length) {
        machine = matches[0]; report.createdDescriptor = ownership.remember(machine);
        if (machine.state === 'started') break;
        if (machine.state === 'created' && !startRequested) {
          startRequested = true;
          try {await call(['machine', 'start', ownership.id(), '-a', APP], 'start-reconciled-owned-qualification', 30000);}
          catch {report.startReplyUncertain = true;}
        }
      }
      await delay(1000);
    }
    assert(machine && ownership.matches(machine)); assert.equal(machine.state, 'started');
    assert.equal(machine.image_ref?.digest, value.digest);
    report.machine = {id: ownership.id(), name: prepared.name, nonce: prepared.nonce, role: ROLE,
      region: machine.region, instance: machine.instance_id, digest: machine.image_ref.digest};
    await noSecrets('recheck-empty-app-secrets'); report.noAppSecretsBeforeAdmission = true;
    const rootCommand = '/usr/bin/env -i PATH=/usr/local/bin:/usr/bin:/bin /usr/local/bin/node -e ';
    const rootProof = await ownedExec(rootCommand + quote(rootProbeSource(prepared.input)), 'root-owned-image-provenance-probe', 16384);
    assert.equal(rootProof.status, 'passed');
    assert.equal(rootProof.nonce, value.nonce); assert.equal(rootProof.uid, 0);
    assert.equal(rootProof.packageMetadata.sha256, value.workerPackageSha256);
    assert.deepEqual(rootProof.provenance, prepared.provenance); assert.equal(rootProof.qualifierSha256, value.qualifierSha256);
    assert.match(rootProof.provenanceSha256, shaPattern); report.rootImageProbe = rootProof;
    const admitted = await ownedExec(rootCommand + quote(gateSource(prepared.input)), 'publish-atomic-admission-gate', 4096);
    assert.equal(admitted.nonce, value.nonce); assert.equal(admitted.published, true); report.admitted = true;
    emit('owned-image-verified-and-admitted');
    let fetched;
    while (Date.now() < mainDeadline) {
      fetched = await ownedExec(rootCommand + quote(proofReadSource(prepared.input)), 'read-bounded-owned-proof', MAX_PROOF + 16384);
      if (fetched.status === 'ready') break;
      assert.equal(fetched.status, 'pending', 'owned proof rejected'); await delay(1500);
    }
    assert(fetched?.status === 'ready'); const proof = fetched.proof;
    assert.equal(proof.nonce, value.nonce); assert.deepEqual(proof.provenance, prepared.provenance);
    assert.equal(proof.kind, ROLE); assert.equal(proof.formatVersion, 1); assert.match(fetched.sha256, shaPattern);
    const bytes = Buffer.from(JSON.stringify(proof, null, 2) + '\n'); assert(bytes.length <= MAX_PROOF);
    assert.equal(sha(bytes), fetched.sha256, 'exact original proof bytes changed');
    await fs.writeFile(path.join(directory, 'guest-proof.json'), bytes, {mode: 0o600, flag: 'wx'});
    report.guestProofSha256 = sha(bytes); report.originalGuestProofSha256 = fetched.sha256;
    report.completed = proof.completed === true; report.qualified = proof.qualified === true; report.counts = proof.counts;
    assert.equal(proof.admittedAfterResolvedDigest, true); assert.equal(proof.supervisorModuleEvaluated, false);
    for (const key of ['productionWired', 'providerUploadQualified', 'actualBrokerGraceQualified', 'acceptanceQualified', 'ptyQualified']) assert.equal(proof[key], false);
    assert.equal(proof.qualifierSha256, value.qualifierSha256); assert.equal(proof.fuseOpenedAsUid1500, true);
    assert.equal(proof.parentNamespacesAndStatusUnchanged, true); assert.equal(proof.boot.uid, 1500); assert.equal(proof.boot.gid, 1500);
    assert.equal(proof.boot.credentialEnvironmentFields, 0); assert.equal(proof.boot.workerVersion, value.workerVersion);
    assert.equal(proof.boot.workerPackage.sha256, value.workerPackageSha256);
    assert.equal(proof.guest?.uid, 1500); assert.equal(proof.guest.zeroFiveCapabilitySets, true);
    assert.equal(proof.guest.noNewPrivileges, true); assert.deepEqual(proof.guest.interfaces, ['lo']);
    assert.equal(proof.completed, true); assert.equal(proof.qualified, true); assert.equal(proof.attempted, 1);
    assert.deepEqual(validateRemoteProof(proof.summary, prepared.input, assert), proof.counts);
    validateGuestMarker(proof.guest.marker, proof.summary, assert);
  } catch (error) {
    report.completed = false; report.qualified = false; report.failedPhase = phase;
    report.errorCode = typeof error.code === 'string' ? error.code : 'ASSERTION';
    report.controllerFailureSha256 = sha(Buffer.from(String(error?.stack || error)));
    try {await fs.writeFile(path.join(directory, 'controller-failure.private'), Buffer.from(String(error?.stack || error)).subarray(0, MAX_STDERR), {mode: 0o600, flag: 'wx'});}
    catch {report.controllerFailureSaveFailed = true;}
    if (ownership.id() && report.machine?.id === ownership.id() && report.machine?.digest === value.digest) {
      diagnosing = true;
      const rootCommand = '/usr/bin/env -i PATH=/usr/local/bin:/usr/bin:/bin /usr/local/bin/node -e ';
      report.failureDiagnostics = await attemptFailureDiagnostics(
        () => ownedExec(rootCommand + quote(fixtureDiagnosticsSource(prepared.input)), 'collect-exact-owned-private-failure-diagnostics', 512 * 1024),
        bundle => saveFixtureDiagnostics(bundle, prepared.input, directory));
      diagnosing = false;
    } else report.failureDiagnostics = {attempted: false, completed: false, reason: 'NO_DIGEST_VERIFIED_OWNED_MACHINE'};
    emit('qualification-failed-private-diagnostics-retained');
  } finally {
    cleanup = true;
    try {
      const matches = (await inventory()).filter(ownership.matches); assert(matches.length <= 1);
      for (const machine of matches) {
        report.createdDescriptor = ownership.remember(machine); assert(ownership.matches(machine));
        try {await call(['machine', 'destroy', ownership.id(), '-a', APP, '--force'], 'destroy-exact-owned-qualification', 15000);}
        catch {report.destroyReplyUncertain = true;}
      }
      const absent = machines => !machines.some(machine => machine.id === ownership.id() || machine.name === prepared.name
        || machine.config?.metadata?.qualification === prepared.nonce);
      const absenceDeadline = Math.min(lifetimeDeadline - 20000, Date.now() + 25000);
      let remaining = await inventory();
      while (!absent(remaining) && Date.now() < absenceDeadline) {await delay(500); remaining = await inventory();}
      report.cleanupVerified = absent(remaining); report.independentAbsence = absent(await inventory());
      assert(report.cleanupVerified && report.independentAbsence);
      assert(!(report.createReplyUncertain && !ownership.id()), 'unobserved uncertain create needs explicit reconciliation');
      const cleanupProof = {formatVersion: 1, id: ownership.id() ?? null, name: prepared.name, nonce: prepared.nonce, role: ROLE,
        absentByIdNameNonce: true, independentInventoryAbsent: true, verifiedAt: new Date().toISOString()};
      const bytes = Buffer.from(JSON.stringify(cleanupProof, null, 2) + '\n');
      await fs.writeFile(path.join(directory, 'cleanup.json'), bytes, {mode: 0o600, flag: 'wx'}); report.cleanupProofSha256 = sha(bytes);
    } catch {report.cleanupVerified = false; report.independentAbsence = false;}
    report.elapsedMs = Date.now() - started; report.withinLifetimeBound = report.elapsedMs < LIFETIME_MS;
    report.verifiedAt = new Date().toISOString();
    await fs.writeFile(reportPath, JSON.stringify(report, null, 2) + '\n', {mode: 0o600, flag: 'wx'});
    emit('qualification-finished');
  }
  return {reportPath, qualified: report.qualified === true, cleanupVerified: report.cleanupVerified === true,
    independentAbsence: report.independentAbsence === true, withinLifetimeBound: report.withinLifetimeBound};
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const result = await main(process.argv.slice(2)); console.log(JSON.stringify(result));
    if (result.status !== 'prepared-no-run' && !(result.qualified && result.cleanupVerified && result.independentAbsence && result.withinLifetimeBound)) process.exitCode = 1;
  } catch (error) {
    console.log(JSON.stringify({kind: ROLE, status: 'rejected-before-execution', errorCode: typeof error.code === 'string' ? error.code : 'ASSERTION'}));
    process.exitCode = 1;
  }
}
