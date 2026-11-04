// Explicit local kernel/toolchain gate. Ordinary npm tests skip this execution
// and cannot be cited as SDK qualification. No credentials or provider calls.
import test from 'node:test';
import assert from 'node:assert/strict';
import {execFileSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
test('real local SDK helper executes selected TypeScript and settles denied/cancelled children', {
  skip: process.env.SDK_HELPER_LOCAL_E2E !== '1'
    ? 'Explicit gate: SDK_HELPER_LOCAL_E2E=1 /workspace/rudder-build/rnpm exec -- node --test tests/sdk-helper-local-e2e.test.mjs; no SDK execution tested'
    : false,
  timeout: 1_200_000,
}, () => {
  const output = execFileSync('bash', ['scripts/test-sdk-helper-local.sh', '--real'], {
    cwd: root, encoding: 'utf8', timeout: 1_190_000, maxBuffer: 2 * 1024 * 1024,
  });
  assert.match(output, /RUDDER_LOCAL_REAL_SDK_E2E_V1=passed/);
  const cases = output.split('\n').filter(line => line.startsWith('LOCAL_SDK_REAL_CASE='))
    .map(line => JSON.parse(line.slice('LOCAL_SDK_REAL_CASE='.length)));
  assert.deepEqual(cases.map(record => record.case), [
    'compile', 'compiler-error', 'denied-proc', 'denied-host-input',
    'cancel-before-start', 'cancel-running', 'original-deadline',
  ]);
  for (const record of cases) {
    assert.equal(record.children_joined, true);
    assert.equal(record.original_deadline_preserved, true);
    assert.equal(record.sdk_admission, false);
    assert.equal(record.root_custody_qualified, false);
    assert.equal(record.parent, 'test-only-uid1500-stand-in');
  }
});
