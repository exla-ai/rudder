import fs from 'node:fs/promises';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import assert from 'node:assert/strict';
import http from 'node:http';
import os from 'node:os';
import {fileURLToPath} from 'node:url';

const exec = promisify(execFile);
const fly = process.env.FLYCTL || 'flyctl';
const app = process.env.RUDDER_QUALIFICATION_APP || 'rudder-workers-exla';
const state = JSON.parse(await fs.readFile(process.env.RUDDER_QUALIFICATION_STATE || 'current.json', 'utf8'));
const scriptDirectory = path.dirname(fileURLToPath(import.meta.url));
const role = 'vfs-capture-qualification';
const forbiddenMachine = '8edd225a7d0378';
const report = {format_version: 1, provider: 'Fly.io', app,
  data_plane: 'remote_partial_capture_v1', name: state.name,
  qualification: state.nonce, startedAt: new Date().toISOString(), steps: []};
const emit = value => console.log(JSON.stringify({at: new Date().toISOString(), ...value}));
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const q = value => "'" + value.replaceAll("'", "'\\''") + "'";
let owned, server, phase = 'preflight';
async function command(args, label, timeout = 60000) {
  phase = label;
  const started = Date.now();
  const result = await exec(fly, args, {timeout, maxBuffer: 8 * 1024 * 1024});
  report.steps.push({step: label, ms: Date.now() - started});
  return result.stdout;
}
async function machines() {
  return JSON.parse(await command(['machine', 'list', '-a', app, '--json'], 'machine-inventory'));
}
function matches(machine) {
  return machine.id !== forbiddenMachine && machine.name === state.name
    && machine.config?.metadata?.qualification === state.nonce
    && machine.config?.metadata?.rudder_role === role;
}
async function remote(cmd, label) {
  assert(owned && owned !== forbiddenMachine);
  return command(['machine', 'exec', owned, '-a', app, cmd, '--timeout', '15'], label, 25000);
}

try {
  assert.match(state.nonce, /^[0-9a-f-]{36}$/);
  assert.equal(state.name, 'vfs-capture-' + state.nonce.slice(0, 12));
  const ready = JSON.parse(await fs.readFile(path.join(state.directory, 'ready-driver.json'), 'utf8'));
  assert.equal(ready.sourceManifestSha256, state.sourceManifestSha256);
  assert.equal(ready.buildInfo.build_source_sha256, state.sourceManifestSha256);
  assert.equal(ready.buildInfo.qualification, 'remote_partial_capture_v1');
  assert(ready.frozenSourceUnchangedAfterBuild && !ready.workspaceManifestChanged);
  const gzip = await fs.readFile(ready.payloadPath);
  assert.equal(createHash('sha256').update(gzip).digest('hex'), ready.payloadSha256);
  const guest = await fs.readFile(path.join(scriptDirectory, 'guest-canary.sh'));
  await fs.writeFile(path.join(state.directory, 'guest-canary.sh'), guest);
  assert.deepEqual(JSON.parse(await command(['secrets', 'list', '-a', app, '--json'], 'assert-no-app-secrets')), []);
  assert(!(await machines()).some(matches), 'a canary for this run already exists');
  const config = {init: {exec: ['/bin/sleep', '1200']},
    guest: {cpu_kind: 'shared', cpus: 2, memory_mb: 2048}, env: {}, services: [],
    restart: {policy: 'no'}, metadata: {rudder_role: role, qualification: state.nonce,
      purpose: 'remote-partial-capture-qualification'}};
  await fs.writeFile(state.machineConfig, JSON.stringify(config, null, 2) + '\n');
  report.requestedConfig = config;
  try {
    const out = await command(['machine', 'run', 'registry.fly.io/rudder-workers-exla:4.0.0-alpha.9', '-a', app,
      '--machine-config', state.machineConfig, '--name', state.name, '--region', 'iad',
      '--detach', '--skip-dns-registration'], 'create-disposable-canary', 120000);
    owned = /Machine ID:\s*([0-9a-f]+)/.exec(out)?.[1];
  } catch (failure) {
    const candidates = (await machines()).filter(matches);
    if (candidates.length !== 1) throw failure;
    owned = candidates[0].id;
  }
  assert(owned && owned !== forbiddenMachine, 'no owned canary identity');
  report.machine = owned;
  emit({step: 'created-disposable-canary', machine: owned, name: state.name});
  let machine;
  const bootDeadline = Date.now() + 180000;
  while (Date.now() < bootDeadline) {
    machine = (await machines()).find(candidate => candidate.id === owned && matches(candidate));
    assert(machine, 'owned machine not found');
    if (machine.state === 'started') break;
    await delay(3000);
  }
  assert.equal(machine.state, 'started');
  assert(!machine.config?.services?.length && !machine.config?.mounts?.length);
  assert(Object.keys(machine.config?.env || {}).every(key => ['PRIMARY_REGION', 'FLY_PRIMARY_REGION'].includes(key)));
  assert.equal(machine.image_ref?.digest?.replace(/^sha256:/, ''), '939c5593c5ed1232577d575f7ff382448c466a9033874c94a1a684c4ec5d599a', 'worker image digest differs from verified alpha9');
  report.instance = machine.instance_id;
  report.region = machine.region;
  report.image = machine.image_ref;
  report.guest = machine.config.guest;
  report.services = 0;
  report.volumes = 0;
  report.environmentKeys = Object.keys(machine.config?.env || {});
  report.artifacts = ready;

  // Serve only generated public fixtures, bound to private 6PN and gated to
  // this exact newly created machine's source IP and random per-run nonce.
  const address = Object.values(os.networkInterfaces()).flat()
    .find(value => value?.family === 'IPv6' && value.address.startsWith('fdaa:'))?.address;
  assert(address, 'private Fly network address unavailable');
  const routes = new Map([['/' + state.nonce + '/payload.tar.gz', gzip], ['/' + state.nonce + '/guest.sh', guest]]);
  server = http.createServer((req, res) => {
    const bytes = routes.get(req.url);
    if (req.method !== 'GET' || req.socket.remoteAddress !== machine.private_ip || !bytes) {
      res.writeHead(404); res.end(); return;
    }
    res.writeHead(200, {'Content-Length': bytes.length, 'Content-Type': 'application/octet-stream', Connection: 'close'});
    res.end(bytes);
  });
  server.requestTimeout = 30000;
  await new Promise((resolve, reject) => {server.once('error', reject); server.listen(0, address, resolve);});
  const port = server.address().port;
  const download = (route, destination) => `exec 3<>/dev/tcp/${address}/${port}; printf 'GET ${route} HTTP/1.0\\r\\nHost: fixture\\r\\nConnection: close\\r\\n\\r\\n' >&3; IFS= read -r status <&3; [[ "$status" == 'HTTP/1.1 200 '* || "$status" == 'HTTP/1.0 200 '* ]] || exit 24; while IFS= read -r line <&3; do [[ "$line" == $'\\r' ]] && break; done; cat <&3 >${destination}; exec 3<&-; exec 3>&-`;
  for (const [route, destination] of [['/' + state.nonce + '/payload.tar.gz', '/tmp/vfs-remote-payload.tar.gz'],
    ['/' + state.nonce + '/guest.sh', '/tmp/vfs-canary.sh']]) {
    await remote('/bin/bash -c ' + q(download(route, destination)), 'private-network-fixture-download');
  }
  const verification = await remote('/bin/sh -c ' + q("printf '%s  %s\\n' '" + ready.payloadSha256
    + "' /tmp/vfs-remote-payload.tar.gz | sha256sum -c -"), 'verify-downloaded-fixture');
  assert(verification.includes('OK'));
  report.fixtureTransport = 'private Fly6PN HTTP; exact VM source/nonce; compressed artifact SHA256 verified before execution; no credentials';
  const launch = '(timeout 750 /bin/sh /tmp/vfs-canary.sh; result=$?; echo "$result" >/tmp/vfs-done) >/tmp/vfs-output 2>&1 </dev/null & echo LAUNCHED';
  assert((await remote('/bin/sh -c ' + q(launch), 'launch-contained-qualification')).includes('LAUNCHED'));
  const deadline = Date.now() + 780000;
  let finished = false, lastNotice = 0;
  while (Date.now() < deadline) {
    const out = await remote("/bin/sh -c 'if test -f /tmp/vfs-done; then printf FINISHED:; cat /tmp/vfs-done; else printf PENDING; fi'", 'poll-contained-qualification');
    if (out.includes('FINISHED:')) {
      report.exitCode = Number(/FINISHED:(\d+)/.exec(out)?.[1]);
      finished = true; break;
    }
    if (Date.now() - lastNotice > 30000) {
      emit({step: 'mounted-qualification-running', machine: owned}); lastNotice = Date.now();
    }
    await delay(3000);
  }
  assert(finished, 'qualification deadline exceeded');
  const encoded = (await remote('/usr/bin/base64 -w0 /tmp/vfs-output', 'read-owned-report')).trim();
  assert(encoded.length && /^[A-Za-z0-9+/]+=*$/.test(encoded));
  const output = Buffer.from(encoded, 'base64').toString('utf8');
  await fs.writeFile(path.join(state.directory, 'mounted-output.log'), output);
  report.results = output.split('\n').filter(line => line.startsWith('{')).map(line => JSON.parse(line));
  assert.equal(report.exitCode, 0, 'mounted qualification returned failure; see owned output');
  const passes = report.results.filter(result => result.ok);
  assert.deepEqual(passes.map(result => result.uid), [0, 1500]);
  for (const result of passes) {
    assert.equal(result.build_source_sha256, state.sourceManifestSha256);
    assert.equal(result.entries, 100101);
    assert.equal(result.profile, 'remote_partial_capture_v1');
    assert(result.partial_capture.data_ack_without_head && result.partial_capture.same_nonce_after_live_write_and_gc
      && result.partial_capture.head_cas_and_retirement && result.partial_capture.current_head_release_rejected
      && result.partial_capture.baseline_unchanged && result.partial_capture.live_best_effort_only
      && result.retained_data_after_revocation);
    assert.equal(result.partial_capture.new_complete_pins, 0);
    assert.equal(result.partial_capture.namespace_format, 4);
    assert.equal(result.partial_capture.complete_rpc_errno, 95);
    assert.equal(result.readdir.file_or_blob_requests_during_listing, 0);
    assert(result.readdir.stable_seek_rewind && result.readdir.namespace_mutation_fixed_snapshot
      && result.readdir.close_reopen_current_snapshot && result.readdir.mid_page_cookie
      && result.readdir.kernel_cursor_cleanup);
  }
  assert(report.results.some(result => result.outer_initial_mount_namespace_unchanged
    && result.owned_fixture_directories_absent));
  report.ok = true;
} catch (error) {
  report.ok = false;
  report.failedPhase = phase;
  report.error = 'qualification failed at ' + phase;
  report.errorCode = error.code || 'assertion';
  process.exitCode = 1;
  emit({step: 'qualification-failed', phase});
} finally {
  if (server) {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  }
  try {
    // Never act on an inventory row without all ownership witnesses matching.
    for (const machine of (await machines()).filter(matches)) {
      await command(['machine', 'destroy', machine.id, '-a', app, '--force'], 'destroy-owned-canary');
    }
    const remaining = await machines();
    report.cleanupVerified = !remaining.some(matches) && !remaining.some(machine => machine.id === owned);
  } catch {
    report.cleanupVerified = false; process.exitCode = 1;
  }
  report.verifiedAt = new Date().toISOString();
  await fs.writeFile(path.join(state.directory, 'report.json'), JSON.stringify(report, null, 2) + '\n', {mode: 0o600});
  emit({step: 'qualification-finished', ok: report.ok, cleanupVerified: report.cleanupVerified,
    report: path.join(state.directory, 'report.json')});
}
