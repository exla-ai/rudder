import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {fileURLToPath} from 'node:url';

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), 'evidence/2026-10-30-alpha10-netns');
const state = JSON.parse(await fs.readFile(path.join(root, 'state.json'), 'utf8'));
const report = JSON.parse(await fs.readFile(path.join(root, 'report.json'), 'utf8'));
assert.equal(state.role, 'alpha10-private-netns-tun');
assert.equal(report.machine.id, '86510dbe935228');
assert.notEqual(report.machine.id, '8edd225a7d0378');
const result = await promisify(execFile)('/workspace/home/.fly/bin/flyctl',
  ['machine', 'list', '-a', 'rudder-workers-exla', '--json'], {timeout: 25000, maxBuffer: 1024 * 1024});
const inventory = JSON.parse(result.stdout);
const absent = !inventory.some(machine => machine.id === report.machine.id || machine.name === state.name
  || machine.config?.metadata?.qualification === state.nonce);
assert(absent);
const proof = {formatVersion: 1, kind: 'separate-process-provider-inventory-absence', app: state.app,
  machine: report.machine.id, name: state.name, nonce: state.nonce, role: state.role,
  absentByIdNameNonce: true, noControlOrHomeExec: true, verifiedAt: new Date().toISOString()};
await fs.writeFile(path.join(root, 'independent-cleanup.json'), JSON.stringify(proof, null, 2) + '\n');
console.log(JSON.stringify(proof));
