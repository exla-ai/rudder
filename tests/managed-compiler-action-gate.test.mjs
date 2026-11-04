import test from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';

const driver = fileURLToPath(new URL('../engine/src/build/frontend_driver.py', import.meta.url));

test('managed compiler rules reject remote configuration and disable every upload switch', () => {
  const result = spawnSync('python3', ['-I', '-B', '-c', `
import importlib.util, sys
spec = importlib.util.spec_from_file_location('managed_frontend', sys.argv[1])
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)
platform = {'os': 'linux', 'architecture': 'x86_64', 'abi': 'fixture'}
rules = driver.rules(False, platform)
for switch in ['allow_cache_upload', 'remote_cache_enabled', 'allow_cache_uploads']:
    assert switch + ' = False' in rules, switch
    assert switch + ' = True' not in rules, switch
try:
    driver.rules(True, platform)
except ValueError as error:
    assert 'remote cache reads/uploads disabled' in str(error)
else:
    raise AssertionError('remote configuration was admitted')
`, driver], {encoding: 'utf8', timeout: 10000});
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
});

test('direct driver requests cannot assert host writer custody or start a frontend', () => {
  for (const remote of [null, {cas_address: 'grpc://127.0.0.1:1'}]) {
    const result = spawnSync('python3', ['-I', '-B', driver], {
      input: JSON.stringify({format_version: 1, workspace: '/must-not-create-managed-workspace',
        prepared_directory: '/missing-sdk', buck2: '/missing-frontend',
        input_custody: true, writers_quiesced: true,
        profile: {buck2: {remote_cache: remote}}}),
      encoding: 'utf8', timeout: 10000,
      env: {...process.env, RUDDER_MANAGED_COMPILER_CACHE: '1', RUDDER_INPUTS_IMMUTABLE: '1'},
    });
    assert.ifError(result.error);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /full-lifetime host SDK\/source writer exclusion is not enforced/);
    assert.equal(result.stdout, '');
  }
});
