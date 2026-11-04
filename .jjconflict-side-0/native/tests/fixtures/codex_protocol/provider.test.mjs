// Pure local support regressions. No listener, subprocess, CLI or model.
import test from 'node:test';
import assert from 'node:assert/strict';
import {toolFor,completed,FIXTURE_KEY,sha} from './provider.mjs';
test('same actual-Codex flat exec tool is selected',()=>{
  assert.equal(toolFor({tools:[{type:'function',name:'exec_command'}]}).name,'exec_command');
  assert.equal(toolFor({tools:[{type:'function',name:'shell_command'}]}).name,'shell_command');
  assert.equal(toolFor({tools:[{type:'function',name:'arbitrary'}]}),undefined);
});
test('namespaced tool retains its namespace',()=>{
  assert.equal(toolFor({tools:[{type:'namespace',name:'functions',tools:[{type:'function',name:'exec_command'}]}]}).namespace,'functions');
});
test('synthetic terminal SSE has explicit successful status and zero accounting',()=>{
  const value=completed('fixture',[{type:'message',id:'example'}]);
  assert.equal(value.type,'response.completed');assert.equal(value.response.status,'completed');
  assert.equal(value.response.output[0].id,'example');assert.equal(value.response.usage.total_tokens,0);
});
test('credential is an explicit fixture literal and file hash is stable',()=>{
  assert.equal(FIXTURE_KEY,'synthetic-loopback-fixture-not-a-credential');
  assert.equal(sha(Buffer.from('actual-codex-tool-edit\n')),'5293751da30620a50e6bde3cef86855f8049fd0ecade6ffe5ce5b2e6deb5a658');
});
