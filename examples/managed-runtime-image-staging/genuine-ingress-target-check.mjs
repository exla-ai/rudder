const TARGET_CHECK_EFFECTS_ALLOWED=false;
if(TARGET_CHECK_EFFECTS_ALLOWED!==true)throw Error('DISABLED_GENUINE_INGRESS_TARGET_CHECK');
// Separate platform check, never an original holder/command/qualification issuer.
const {ChildProcess}=await import('node:child_process');
const {MessageChannel}=await import('node:worker_threads');
const fs=await import('node:fs');const {createHash}=await import('node:crypto');
const ROOT='/opt/rudder-runtime-qualification';const UID=0;
const NODE='/usr/local/bin/rudder-runtime-qualification-node',PYTHON='/usr/bin/python3',FLY='/workspace/home/.fly/bin/flyctl';
const NODE_SHA='94efd062ed066c1a0b332e18249859c905e920eeada5df55d24c99b919629a90';
const retained=[];const need=v=>{if(!v)throw Error('FIXED_TARGET_CHECK_REFUSED');};
const hash=b=>createHash('sha256').update(b).digest('hex');
function file(path,max){
 const fd=fs.openSync(path,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW);retained.push(fd);const st=fs.fstatSync(fd);
 need(st.isFile()&&st.uid===UID&&(st.mode&0o022)===0&&st.size>0&&st.size<=max);
 const bytes=fs.readFileSync(fd);need(bytes.length===st.size);return {fd,st,bytes};
}
need(process.platform==='linux'&&process.versions.node.split('.')[0]==='22'&&process.execPath===NODE&&process.geteuid()===UID);
const manifest=JSON.parse(file(ROOT+'/context-manifest.json',1024*1024).bytes);
need(manifest.node_sha256===NODE_SHA&&hash(file(NODE,256*1024*1024).bytes)===NODE_SHA);
const pythonReal=fs.realpathSync(PYTHON);need(/^\/usr\/bin\/python3\.[0-9]+$/.test(pythonReal));
const python=file(pythonReal,64*1024*1024);need((python.st.mode&0o111)!==0&&python.bytes.subarray(0,4).equals(Buffer.from([127,69,76,70])));
const pythonAlias=fs.statSync(PYTHON);need(pythonAlias.dev===python.st.dev&&pythonAlias.ino===python.st.ino);
const fly=file(FLY,256*1024*1024),flyRow=manifest.files.flyctl;
need(flyRow&&flyRow.sha256===manifest.genuine_ingress_packaging.flyctlSha256&&hash(fly.bytes)===flyRow.sha256&&fly.bytes.length===flyRow.bytes&&(fly.st.mode&0o111)!==0);
const sources=['cloud/dist/managed-home-provisioning/genuine-ingress-log-worker.py','cloud/dist/managed-home-provisioning/genuine-ingress-log-project.py','examples/managed-runtime-image-staging/genuine-ingress-target-probe.py'];
const opened=sources.map(rel=>{const got=file(ROOT+'/'+rel,65536),row=manifest.files[rel];need(row&&row.sha256===hash(got.bytes)&&row.bytes===got.bytes.length);return got;});
need(hash(opened[0].bytes)==='77bfde8dc867b869ac2971f3973c8ec91e7c1f0bfcf7a46420208f79fa011b8a'&&hash(opened[1].bytes)==='df3e53242dfbb0b360caf6b69df363e0467e00fc50ad9f33cf5b893088312d18');
const unknown=new MessageChannel();retained.push(unknown);unknown.port1.on('message',()=>{});unknown.port1.ref();unknown.port2.ref();
const child=new ChildProcess();retained.push(child);let exit=false,eof={stdout:false,stderr:false},code=null,errors=[],parts={stdout:[],stderr:[]},bytes={stdout:0,stderr:0};
let finish;const settled=new Promise(resolve=>{finish=resolve;});
function observe(){if(exit&&eof.stdout&&eof.stderr){unknown.port1.close();unknown.port2.close();finish();}}
child.on('error',()=>errors.push('original_worker_error'));child.once('exit',(v,s)=>{if(Number.isInteger(v)&&s===null){exit=true;code=v;observe();}else errors.push('original_worker_exit_unknown');});
try{
 need(typeof child.spawn==='function');child.spawn({file:PYTHON,args:[PYTHON,'-I','-B','/proc/self/fd/5'],envPairs:['PATH=/usr/bin:/bin','PYTHONIOENCODING=utf-8'],stdio:['ignore','pipe','pipe',...opened.map(f=>f.fd)]});
 for(const n of ['stdout','stderr']){const stream=child[n];need(stream);retained.push(stream);stream.on('data',b=>{bytes[n]+=b.length;if(bytes[n]<=8192)parts[n].push(b);else errors.push('original_worker_bound');});stream.on('error',()=>errors.push('original_worker_stream_error'));stream.once('end',()=>{eof[n]=true;observe();});}
}catch{errors.push('original_worker_constructor_or_observer_unknown');}
// A constructor/stream unknown keeps SAME original owner and ports referenced.
await settled;need(code===0&&errors.length===0&&bytes.stderr===0);
const value=JSON.parse(Buffer.concat(parts.stdout).toString('utf8'));
const expected={schema:1,kind:'ISOLATED_PLATFORM_MECHANICS_DATA',pythonProcFd:true,actualTinyPidfd:true,actualTinyWait:true,bothTinyEof:true,samePreallocatedChild:true,rawFsync:true,originalBudgetTested:false,flyInvoked:false,genuineIngress:false,qualified:false};
need(JSON.stringify(value)===JSON.stringify(expected));
console.log(JSON.stringify({schema:1,kind:'STAGING_CONTROL_PLATFORM_DATA',node22:true,retainedChildSpawn:true,pythonProcFd:true,actualTinyPidfd:true,actualTinyWait:true,bothTinyEof:true,samePreallocatedChild:true,originalBudgetTested:false,flyInvoked:false,genuineIngress:false,qualified:false}));
