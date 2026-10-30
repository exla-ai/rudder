// Runs only baked alpha9 modules against a generated disposable project.
import fs from 'node:fs';
import cp from 'node:child_process';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import tls from 'node:tls';
import {syncBuiltinESMExports} from 'node:module';
import assert from 'node:assert/strict';

const root = process.argv[2], mode = process.argv[3];
assert(root && ['fabric', 'malformed', 'unsupported', 'missing'].includes(mode));
process.chdir(root);
const attempts = [];
const forbidden = label => (...args) => {
  attempts.push(label);
  throw new Error('qualification blocked ' + label);
};
globalThis.fetch = forbidden('fetch');
http.request = forbidden('http.request'); http.get = forbidden('http.get');
https.request = forbidden('https.request'); https.get = forbidden('https.get');
net.connect = forbidden('net.connect'); net.createConnection = forbidden('net.createConnection');
net.Socket.prototype.connect = forbidden('socket.connect'); tls.connect = forbidden('tls.connect');
// findRepoRoot uses a read-only git probe. No backend or history command may run.
for (const name of ['spawn', 'spawnSync', 'exec', 'execSync', 'execFile', 'execFileSync', 'fork']) {
  const original = cp[name];
  cp[name] = (...args) => {
    const command = String(args[0]);
    const argv = Array.isArray(args[1]) ? args[1] : [];
    if (command.endsWith('git') && argv[0] === 'rev-parse'
      && argv.every(arg => ['rev-parse', '--show-toplevel', '--git-common-dir'].includes(arg))) {
      return original(...args);
    }
    return forbidden('child_process.' + name)(...args);
  };
}
syncBuiltinESMExports();
const authority = await import('/opt/rudder/dist/engine-authority.js');
let guard;
try { authority.assertLegacyWriterAllowed(root); guard = 'allowed'; }
catch (error) { guard = String(error.message); }
if (mode === 'missing') {
  assert.equal(guard, 'allowed');
  console.log(JSON.stringify({mode, guard, limitation: 'alpha9 TS missing owner projection fails open; no writer invoked', attempts}));
  process.exit(0);
}
assert.notEqual(guard, 'allowed');
const {continueRun, workerRun} = await import('/opt/rudder/dist/run-manager.js');
const {scheduleTick} = await import('/opt/rudder/dist/scheduler.js');
const results = [];
for (const [name, action] of [
  ['board_continueRun', () => continueRun({runId: 'managed-fixture', prompt: 'inert fixture', silent: true})],
  ['detached_workerRun', () => workerRun(root, 'managed-fixture', 'old-attempt')],
  ['board_scheduler_tick', () => scheduleTick(root, {})],
]) {
  let error;
  try { await action(); } catch (failure) { error = String(failure.message); }
  assert.equal(error, guard, name + ' did not stop at the authority guard');
  results.push({name, refused: true, error});
}
assert.deepEqual(attempts, []);
console.log(JSON.stringify({mode, guard, results, attempts,
  // This is the same function the board mutation delegates to, without opening
  // an HTTP service or allowing a scheduler/backend to execute.
  scope: 'exact alpha9 board writer functions; no HTTP server or model call'}));
