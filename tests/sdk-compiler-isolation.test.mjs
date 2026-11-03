// Actual local Linux kernel boundary subprocesses. No Fly, SDK role or release gate.
import test from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';

test('compiler proc-less isolation adversaries pass on the actual local kernel', {
  skip: process.platform !== 'linux' ? 'requires Linux namespace/seccomp kernel' :
    process.getuid() !== 1500 || process.getgid() !== 1500 ?
      'actual user namespace fixture requires original UID/GID 1500' : false,
  timeout: 120000,
}, () => {
  const script = fileURLToPath(new URL('../native/src/compiler_sdk_confinement_tests.py', import.meta.url));
  const result = spawnSync('python3', ['-I', script, '-v'], {
    encoding: 'utf8', timeout: 110000, maxBuffer: 1024 * 1024,
  });
  assert.ifError(result.error);
  assert.equal(result.signal, null, result.stderr);
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.match(result.stderr, /Ran 11 tests/);
  if (process.arch === 'x64') assert.doesNotMatch(result.stderr, /skipped=/);
});
