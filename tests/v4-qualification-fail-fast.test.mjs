import assert from 'node:assert/strict';
import test from 'node:test';
import {EventEmitter} from 'node:events';
import {PassThrough} from 'node:stream';
import {mkdtempSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {OriginalQualificationDiagnosticRead,projectSdkFailureLogs} from '../cloud/dist/managed-home-provisioning/staging-sdk-diagnostics.js';

const target={app:'rudder-mh-0123456789ab4cde81230123456789ab',machine:'1855366c049e08'};
const machineText='SDK actual isolated namespaces unavailable: io_kind=permission_denied errno=13';
const frame={kind:'rudder-root-retained-failure-v1',component:'root_storage',stage:'serve_runtime',
 check:'SdkProcessNamespaces',class:'io',io_kind:'permission_denied',errno:13,error_text:machineText};
const line=(value=frame,machine=target.machine)=>JSON.stringify({instance:machine,message:JSON.stringify(value)})+'\n';

for(const role of ['sdk','runtime'])test(`${role}: exact machine failure is available before logs child exits, without releasing custody`,async()=>{
 const home=mkdtempSync(join(tmpdir(),'rudder-v4-fail-fast-'));
 const previous=process.env.RUDDER_RUNTIME_QUALIFICATION_INPUT;
 process.env.RUDDER_RUNTIME_QUALIFICATION_INPUT=join(home,'input.json');
 const child=new EventEmitter();child.stdout=new PassThrough();child.stderr=new PassThrough();
 let allocations=0,spawns=0,kills=0,remainingCalls=0,settled=false;
 child.spawn=()=>{spawns++;};child.kill=()=>{kills++;return true;};
 const failures=[],clock={remaining(){remainingCalls++;return 300_000;}};
 const read=new OriginalQualificationDiagnosticRead(target,clock,'/synthetic/flyctl','test-only-token',
  failure=>failures.push(failure),()=>{allocations++;return child;},role);
 let run;
 try{
  run=read.startRetaining().then(()=>{settled=true;});
  const callsBeforeFailure=remainingCalls;
  child.stdout.write(line(frame,'1855366c049e09'));
  assert.equal(failures.length,0,'another machine cannot end this stage');
  const raw=Buffer.from(line()),split=raw.length-2;
  child.stdout.write(raw.subarray(0,split));assert.equal(failures.length,0);
  child.stdout.write(raw.subarray(split));
  assert.equal(failures.length,1);assert.equal(failures[0].machine_error_text,machineText);
  assert(failures[0].error_text.startsWith(machineText+'; root_storage/serve_runtime:'));
  assert.equal(settled,false);assert.equal(remainingCalls,callsBeforeFailure,'failure observation cannot renew the stage clock');
  const status=read.statusData();assert.equal(status.actual_exit_observed,false);
  assert.equal(status.owners_retained,true);assert.equal(status.root_helper_join_proved,false);
  assert.equal(status.qualification_authority,false);assert.equal(kills,0);
  child.stdout.write(line());assert.equal(failures.length,1,'first failure stays fixed');
 }finally{
  child.emit('exit',0,null);child.emit('close',0,null);child.stdout.end();child.stderr.end();
  if(run)await run;
  if(previous===undefined)delete process.env.RUDDER_RUNTIME_QUALIFICATION_INPUT;
  else process.env.RUDDER_RUNTIME_QUALIFICATION_INPUT=previous;
  rmSync(home,{recursive:true,force:true});
 }
 assert.equal(allocations,1);assert.equal(spawns,1);assert.equal(settled,true);
 assert.equal(read.statusData().failures[0].machine_error_text,machineText);
 assert(!JSON.stringify(read.statusData()).includes('test-only-token'));
});

test('exact machine text stays optional and cannot be supplied as an unvalidated diagnostic field',()=>{
 const {error_text,...withoutText}=frame;
 assert.equal(projectSdkFailureLogs(Buffer.from(line(withoutText)),target)[0].machine_error_text,null);
 for(const value of [{...frame,error_text:machineText+' private-value'},
  {...frame,machine_error_text:'untrusted'}, {...withoutText,machine_error_text:machineText}])
  assert.deepEqual(projectSdkFailureLogs(Buffer.from(line(value)),target),[]);
});
