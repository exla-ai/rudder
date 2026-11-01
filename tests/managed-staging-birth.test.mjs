// Pure DATA validation only. No Cloud imports, supervisor/driver startup,
// Root/role factory, clocks, provider or globally patched runtime context.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import vm from 'node:vm';

const source=await fs.readFile(new URL('../examples/managed-runtime-image/qualify-runtime-role.mjs',import.meta.url),'utf8');
const begin=source.indexOf('const exact='),end=source.indexOf('const stamp=');
assert(begin>=0&&end>begin);
const validate=vm.runInNewContext(source.slice(begin,end)+'\nvalidateSupervisorBirthData;');
const legacy=()=>({format_version:1,boot_nonce:'a1486f70-930c-4cdb-b5ef-cc5dbf5647ec',
  supervisor_pid:101,pg_work:'/var/lib/rudder-runtime-qualification-pg/run-a1486f70-930c-4cdb-b5ef-cc5dbf5647ec',
  setup_end_boottime_milliseconds:900000});
const protectedBirth=()=>({source:'protected-birth-file-v1',kind:'operator-planned-staging-birth-v1',
  sha256:'a'.repeat(64),bytes:128,dev:42,ino:9001});

test('legacy five fields and both exact new shapes validate without changing original state DATA',()=>{
 for(const state of [legacy(),{...legacy(),birth_selection:{source:'generated-v1'}},
   {...legacy(),birth_selection:protectedBirth()}]){
  const original=JSON.stringify(state);
  assert.equal(validate(state),undefined);
  assert.equal(JSON.stringify(state),original);
  assert.equal(state.supervisor_pid,101);assert.equal(state.setup_end_boottime_milliseconds,900000);
 }
});

test('inexact selection fields, invalid protected metadata and changed birth schema refuse',()=>{
 const invalid=[null,{},'generated-v1',{source:'generated-v1',ready:true},
   {...protectedBirth(),source:'generated-v1'}, {...protectedBirth(),kind:'isolated-staging-control-birth-v1'},
   {...protectedBirth(),sha256:'A'.repeat(64)}, {...protectedBirth(),sha256:'a'.repeat(63)},
   ...[0,-1,4097,1.5,true,'128',Number.MAX_SAFE_INTEGER+1].map(bytes=>({...protectedBirth(),bytes})),
   ...[-1,1.5,'42',Number.MAX_SAFE_INTEGER+1].map(dev=>({...protectedBirth(),dev})),
   ...[0,-1,1.5,'9001',Number.MAX_SAFE_INTEGER+1].map(ino=>({...protectedBirth(),ino})),
   {...protectedBirth(),owner:true}];
 for(const birth_selection of invalid)assert.throws(()=>validate({...legacy(),birth_selection}));
 for(const state of [
   {...legacy(),extra:true},
   {...legacy(),birth_selection:protectedBirth(),extra:true},
   {...legacy(),birth_selection:protectedBirth(),format_version:true},
   {...legacy(),birth_selection:{source:'generated-v1'},boot_nonce:legacy().boot_nonce.toUpperCase()},
   {...legacy(),birth_selection:protectedBirth(),boot_nonce:'a1486f70-930c-5cdb-b5ef-cc5dbf5647ec'},
   {...legacy(),birth_selection:protectedBirth(),boot_nonce:'a1486f70-930c-4cdb-75ef-cc5dbf5647ec'},
 ])assert.throws(()=>validate(state));
});
