// Adapter DATA boundary only. These do not fabricate PG routes or qualify
// provisioning, native ingress, profiles, model execution, or deployment.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseManagedMachineInput } from '../dist/managed-machine-api.js';
const selection = { workspace: 'mp-123', command_key: 'client-intent-7', backend: 'codex', model: 'gpt-5.4', profile_id: 'selected-api' };
test('managed caller requires a stable explicit client key, never an RPC/default key', () => {
  for (const command_key of [undefined, '', 'unsafe/key', ' '.repeat(2)]) assert.throws(() => parseManagedMachineInput('open-prepared-project', { ...selection, command_key }));
  assert.equal(parseManagedMachineInput('open-prepared-project', selection).commandKey, 'client-intent-7');
});
test('managed caller preserves exact multiline task and selected model/profile', () => {
  const text = '  first line\n\tsecond line\r\n';
  const result = parseManagedMachineInput('start-task', { ...selection, text });
  assert.equal(result.task, text); assert.deepEqual(result.selection, { backend: 'codex', model: 'gpt-5.4', profileId: 'selected-api' });
  assert(Object.isFrozen(result)); assert(Object.isFrozen(result.selection));
});
test('managed caller rejects implicit selection, arbitrary host/runtime and credential fields', () => {
  for (const field of ['backend', 'model', 'profile_id']) assert.throws(() => parseManagedMachineInput('open-prepared-project', { ...selection, [field]: undefined }));
  for (const field of ['project_alias', 'checkout', 'api_key', 'oauth', 'env', 'program']) assert.throws(() => parseManagedMachineInput('open-prepared-project', { ...selection, [field]: 'anything' }));
});
test('managed task checks UTF8 bytes and NUL without silently trimming/rekeying', () => {
  for (const text of ['', '\n \t', 'a\0b', '界'.repeat(8193)]) assert.throws(() => parseManagedMachineInput('start-task', { ...selection, text }));
  assert.equal(parseManagedMachineInput('start-task', { ...selection, text: '界'.repeat(8192) }).task.length, 8192);
});
test('receipt query only names original account/project-bound key', () => {
  const query = parseManagedMachineInput('receipt', { workspace: selection.workspace, command_key: 'query-8', lookup_command_key: selection.command_key });
  assert.equal(query.lookupKey, selection.command_key); assert.equal(query.selection, undefined);
  assert.throws(() => parseManagedMachineInput('receipt', { ...selection, lookup_command_key: selection.command_key }));
});
