import assert from 'node:assert/strict';
import test from 'node:test';
import {EventEmitter} from 'node:events';
import {PassThrough} from 'node:stream';
import {setImmediate as turn} from 'node:timers/promises';
import {inspect} from 'node:util';
import {OriginalSdkDiagnosticRead,projectSdkFailureLogs,sdkDiagnosticTarget} from '../cloud/dist/managed-home-provisioning/staging-sdk-diagnostics.js';

const target={app:'rudder-mh-0123456789ab4cde81230123456789ab',machine:'1855366c049e08'};
const frame={kind:'rudder-root-retained-failure-v1',component:'root_storage',stage:'serve_runtime',class:'io',check:'SdkProcessResult',io_kind:'permission_denied',errno:13};
const line=(f=frame,machine=target.machine)=>JSON.stringify({instance:machine,message:JSON.stringify(f)})+'\n';
const plain=(f=frame,machine=target.machine)=>`\x1b[2m2026-11-02T06:30:00Z\x1b[0m app[${machine}] iad [info]${JSON.stringify(f)}\n`;
const time={end:Infinity,remaining:()=>5000};
class FakeChild extends EventEmitter {
 stdout=new PassThrough();stderr=new PassThrough();kills=[];options;
 spawn(options){this.options=options;}
 kill(signal){this.kills.push(signal);return true;}
 settle(code=0){this.stdout.end();this.stderr.end();this.emit('exit',code,null);this.emit('close',code,null);}
}
function fixture({onFailure=()=>{},clock=time,spawn}={}){
 const child=new FakeChild();if(spawn)child.spawn=options=>spawn(child,options);
 let allocations=0;
 const read=new OriginalSdkDiagnosticRead(target,clock,'/explicit/flyctl','synthetic-private-token',onFailure,()=>{allocations++;return child;});
 return {read,child,allocations:()=>allocations};
}

test('projects actual Root fixed frame in observed Fly ANSI prefix and exact-machine JSON records',()=>{
 for(const raw of [line(),plain(),plain().replace(' iad ',' ')]){
  const [f]=projectSdkFailureLogs(Buffer.from(raw),target);assert.equal(f.check,'SdkProcessResult');
  assert.equal(f.error_text,'root_storage/serve_runtime: SdkProcessResult; permission_denied (errno 13)');
 }
 const raw=line()+JSON.stringify({instance:target.machine,message:'Bearer secret-token arbitrary config.env text'})+'\n';
 assert(!JSON.stringify(projectSdkFailureLogs(Buffer.from(raw),target)).includes('secret-token'));
});

test('rejects foreign, unframed, duplicate-key, secret-shaped and invalid fixed data',()=>{
 const bad=[line(frame,'1855366c049e09'),plain(frame,'1855366c049e09'),JSON.stringify(frame),
  line({...frame,check:'SdkPrivateSecretValue'}),line({...frame,stage:'handshake'}),line({...frame,extra:'secret'}),
  line({...frame,errno:true}),line({...frame,errno:5000}),line({...frame,class:'panic'}),line({...frame,io_kind:'secret'}),
  line({...frame,error_text:'Bearer secret-value'}),
  JSON.stringify({instance:target.machine,message:JSON.stringify(frame).replace('"errno":13','"errno":1,"errno":13')})+'\n',
  line().replace('"instance":',`"instance":"foreign","instance":`),
  plain().replace('[info]','[info]source echo: ')];
 for(const raw of bad)assert.deepEqual(projectSdkFailureLogs(Buffer.from(raw),target),[],raw);
 assert.throws(()=>projectSdkFailureLogs(Buffer.alloc(65537),target));
});

test('optional error text must exactly match a typed operation and the same errno fields',()=>{
 const error_text='SDK retained mount reopen failed: io_kind=permission_denied errno=13';
 assert(projectSdkFailureLogs(Buffer.from(line({...frame,error_text})),target)[0].error_text.startsWith(error_text));
 const mount='SDK private namespace mount failed: io_kind=permission_denied errno=13';
 assert(projectSdkFailureLogs(Buffer.from(line({...frame,error_text:mount})),target)[0].error_text.startsWith(mount));
 for(const text of [error_text+' secret',error_text.replace('13','1'),error_text.replace('permission_denied','not_found'),error_text+'\n'])
  assert.deepEqual(projectSdkFailureLogs(Buffer.from(line({...frame,error_text:text})),target),[]);
});

test('target comes only from the same captured original machine/configure/start identity',()=>{
 const machine={id:target.machine,instanceId:'original-instance',configSha256:'original-config'};
 const data={scope:'retained-disposable-provider-response-data-only',original_creation_captured:true,pending_effect:null,
  provision_nonce:'01234567-89ab-4cde-8123-0123456789ab',app_name:target.app,
  responses:{original_responses:{machine,configure:machine,start:{originalHttpAck:true,ownedReadback:{...machine}}}}};
 assert.deepEqual(sdkDiagnosticTarget(data),target);
 for(const mutate of [d=>d.pending_effect={},d=>d.original_creation_captured=false,d=>d.app_name='foreign',
  d=>d.responses.original_responses.start.ownedReadback.instanceId='foreign',d=>d.responses.original_responses.start.originalHttpAck=false]){
  const d=structuredClone(data);mutate(d);assert.throws(()=>sdkDiagnosticTarget(d));
 }
});

test('retains the original child before spawn, uses explicit executable and env-only sanitized token',async()=>{
 const previous=process.env.FLY_ACCESS_TOKEN;process.env.FLY_ACCESS_TOKEN='inherited-secret';let allocated=false;
 try{
  const child=new FakeChild(),read=new OriginalSdkDiagnosticRead(target,time,'/explicit/flyctl','synthetic-private-token',()=>{},()=>{allocated=true;return child;});
  child.spawn=options=>{assert(allocated);child.options=options;};
  const run=read.startRetaining();const o=child.options;
  assert.equal(o.file,'/explicit/flyctl');assert.deepEqual(o.args,['/explicit/flyctl','logs','--app',target.app,'--machine',target.machine,'--no-tail']);
  assert(o.envPairs.includes('FLY_API_TOKEN=synthetic-private-token'));assert(!o.envPairs.some(s=>s.startsWith('FLY_ACCESS_TOKEN=')));
  assert(!JSON.stringify(o.args).includes('synthetic-private-token'));assert(!inspect(read).includes('synthetic-private-token'));assert.throws(()=>JSON.stringify(read));
  child.settle();await run;
 }finally{if(previous===undefined)delete process.env.FLY_ACCESS_TOKEN;else process.env.FLY_ACCESS_TOKEN=previous;}
});

test('fixed failure stops the caller before CLI settlement; the same child and both pipes stay retained',async()=>{
 const failures=[],f=fixture({onFailure:x=>failures.push(x)});let done=false;const run=f.read.startRetaining().then(()=>{done=true;});
 const raw=line();f.child.stdout.write(raw.slice(0,80));assert.equal(failures.length,0);
 f.child.stdout.write(raw.slice(80));assert.equal(failures.length,1);assert.equal(done,false);
 assert.equal(f.read.statusData().actual_exit_observed,false);assert.equal(f.read.statusData().root_helper_join_proved,false);
 f.child.emit('exit',0,null);f.child.emit('close',0,null);await turn();assert.equal(done,false,'close is not EOF');
 f.child.stdout.end();await turn();assert.equal(done,false,'one EOF is not both');f.child.stderr.end();await run;
 assert.equal(f.allocations(),1);assert.equal(f.child.kills.length,0);assert.equal(f.read.statusData().stdout_eof,true);
});

test('terminal frame without newline still signals failure after actual wait and both EOF',async()=>{
 const failures=[],f=fixture({onFailure:x=>failures.push(x)}),run=f.read.startRetaining();
 f.child.stdout.write(line().trimEnd());assert.equal(failures.length,0);f.child.settle();await run;
 assert.equal(failures.length,1);assert.equal(f.read.failures()[0].check,'SdkProcessResult');
});

test('callback error is contained and fixed; secret error text never leaves capture',async()=>{
 const f=fixture({onFailure(){throw Error('private callback token');}}),run=f.read.startRetaining();
 assert.doesNotThrow(()=>f.child.stdout.write(line()));f.child.settle();await run;
 assert(f.read.statusData().errors.includes('failure_callback_error'));assert(!JSON.stringify(f.read.statusData()).includes('private callback token'));
});

test('overflow terminates only the original logs child once and retains wait/EOF custody',async()=>{
 const f=fixture(),run=f.read.startRetaining();let done=false;run.then(()=>{done=true;});
 f.child.stdout.write(Buffer.alloc(65537,120));f.child.stderr.write(Buffer.alloc(32));assert.deepEqual(f.child.kills,['SIGKILL']);assert.equal(done,false);
 f.child.settle(1);await run;assert.equal(f.allocations(),1);assert(f.read.statusData().errors.includes('original_logs_output_bound'));
 assert.equal(f.read.failures().length,0);assert(!JSON.stringify(f.read.statusData()).includes('xxxxxxxx'));
});

test('original stage remaining bounds the logs child and does not create another stage clock',async()=>{
 const f=fixture({clock:{end:1,remaining:()=>5}}),run=f.read.startRetaining();
 await new Promise(resolve=>setTimeout(resolve,15));assert.deepEqual(f.child.kills,['SIGKILL']);
 f.child.settle(1);await run;assert(f.read.statusData().errors.includes('original_logs_deadline'));
});

test('uncertain constructor and stream errors never retry or discard the original child',async()=>{
 const f=fixture({clock:{end:1,remaining:()=>5},spawn(){throw Error('private constructor error');}}),run=f.read.startRetaining();let done=false;run.then(()=>{done=true;});
 await turn();assert.equal(done,false);assert.equal(f.allocations(),1);assert.equal(f.read.statusData().constructor_completed,false);
 await new Promise(resolve=>setTimeout(resolve,15));assert.equal(done,false);assert(f.read.statusData().errors.includes('original_start_unknown'));assert.deepEqual(f.child.kills,['SIGKILL']);
 const g=fixture(),other=g.read.startRetaining();g.child.stderr.emit('error',Error('private stream error'));g.child.settle(1);await other;
 assert.deepEqual(g.child.kills,['SIGKILL']);assert.deepEqual(g.read.statusData().errors,['original_stream_error']);
});
