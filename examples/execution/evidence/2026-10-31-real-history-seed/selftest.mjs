// Fake transport and source parsing only. No provider, smoke command or compiler.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import vm from 'node:vm';
import {randomUUID} from 'node:crypto';
import {fileURLToPath} from 'node:url';
import {main,validateProof} from './controller.mjs';
import {plan,ownership,sha,requireTaggedRevision,requirePhysicalInputs,DERIVED_IMAGE_DIGEST,QUALIFIER_ELF_SHA256,SOURCE_TREE_SHA256,BASE_IMAGE_DIGEST,QUALIFIER_PATH,QUALIFIER_TEST,QUALIFIER_ARGS,IMAGE,REVISION,VERSION,ROLE,FORBIDDEN_ID,FORBIDDEN_INSTANCE,FORBIDDEN_IMAGE_DIGEST} from './ownership.mjs';
import {guestRoot,guestSmoke} from './guest.mjs';
const HERE=path.dirname(fileURLToPath(import.meta.url));
const encode=b=>({bytes:b.length,sha256:sha(b),base64:b.toString('base64')});
const digest='sha256:'+'2'.repeat(64); // synthetic only; never physical evidence
const qualifierText=receipt=>'\nrunning 1 test\ntest '+QUALIFIER_TEST+' ... '+JSON.stringify(receipt)+'\nok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 8 filtered out; finished in 0.12s\n';
function refreshQualifier(q,text=qualifierText(q.receipt)){
  const stdout=Buffer.from(text),stderr=Buffer.alloc(0);q.stdout=encode(stdout);q.stderr=encode(stderr);
  Object.assign(q.child,{stdoutBytes:stdout.length,stderrBytes:stderr.length,stdoutSha256:sha(stdout),stderrSha256:sha(stderr)});
}
function proof(p){
  const paths={node:'/usr/local/bin/node',native:'/opt/rudder/dist/native/rudder-native',buddy:'/opt/rudder/dist/native/rudder-jj',cli:'/opt/rudder/dist/index.js',package:'/opt/rudder/package.json',qualifier:QUALIFIER_PATH};
  const artifacts=Object.fromEntries(Object.entries(paths).map(([k,file])=>[k,{path:file,bytes:10,sha256:'a'.repeat(64)}]));
  artifacts.qualifier.sha256=p.input.qualifierElfSha256;
  const baseline={commit:'c'.repeat(40),change:'d'.repeat(32),parents:['e'.repeat(40)],tree:{terms:['f'.repeat(40)],labels:[]}},operation='a'.repeat(128);
  const receipt={seed:{kind:'directory',algorithm:'sha256',digest:'b'.repeat(64),size_bytes:700},baseline,operation,original_owner_checks:8,real_source_history:true,real_runnable_source_mapping:true,spec3:false,models:false,scope:'real-history-seed-producer-and-cross-uid-forks-only',root_source_pin_retained_through_child:true,
    root_write_error:'EROFS',read_owner_blocked_cleanup:true,actual_child_reaped:true,actual_ebusy_retained_owner:true,
    normal_unmount_verified:true,own_mount_pin_removed:true,fixture_original_pins_removed:true,production_ipc:false,backend:false,
    child:{baseline,operation,new_baseline:{commit:'1'.repeat(40),change:'2'.repeat(32),parents:[baseline.commit],tree:{terms:['3'.repeat(40)],labels:[]}},new_operation:'4'.repeat(128),environment_cleared:true,real_git_head:true,real_parent_history:true,first_fork_new_commit_operation:true,second_fork_unchanged:true,new_object_absent_from_second:true,seed:{kind:'directory',algorithm:'sha256',digest:'b'.repeat(64),size_bytes:700},uid:1500,caps0_nnp1:true,
      same_mount_namespace:true,write_error:'EROFS',private_forks:2,child_cas_owner:1500}};
  const qualifier={argv:[QUALIFIER_PATH,...QUALIFIER_ARGS],artifact:artifacts.qualifier,elfMagicVerified:true,
    sourceTreeSha256:p.input.sourceTreeSha256,receipt,child:{code:0,signal:null,exitObserved:true,settled:true,failure:null}};
  refreshQualifier(qualifier);
  const jobs=[['cli-version',['/usr/local/bin/node','/opt/rudder/dist/index.js','--version'],VERSION],
    ['native-smoke',['/opt/rudder/dist/native/rudder-native','--smoke'],'rudder-native smoke ok'],
    ['native-execution',['/opt/rudder/dist/native/rudder-native','fabric','execution','capabilities'],JSON.stringify({format_version:1,platform:'linux',network_isolated:false})],
    ['buddy-version',['/opt/rudder/dist/native/rudder-jj','--version'],'rudder-jj 0.40.0-rudder-bridge-v1'],
    ['buddy-capabilities',['/opt/rudder/dist/native/rudder-jj','rudder-capabilities'],JSON.stringify({format_version:1,jj_version:'0.40.0',vfs_default:false,local_development:false,automatic_jj_replacement:false})],
    ['git-version',['/usr/local/bin/git','--version'],'git version 2.56.0'],['jj-version',['/usr/local/bin/jj','--version'],'jj 0.40.0']];
  const node={version:'v22.23.4',resolvedPath:'/usr/local/bin/node',sha256:artifacts.node.sha256};
  const smoke={formatVersion:1,kind:'stock23-uid1500-smoke',nonce:p.input.nonce,status:'passed',modelCalls:false,supervisorLoaded:false,environmentCleared:true,
    identity:{uid:1500,gid:1500,groups:[],noNewPrivs:1,capabilities:Object.fromEntries(['CapEff','CapPrm','CapInh','CapAmb','CapBnd'].map(k=>[k,'0000000000000000']))},artifacts,node,packageVersion:VERSION,
    commands:jobs.map(([label,argv,text])=>({label,argv,child:{code:0,signal:null,exitObserved:true,settled:true,failure:null},stdout:encode(Buffer.from(text+'\n')),stderr:encode(Buffer.alloc(0))}))};
  const bytes=Buffer.from(JSON.stringify(smoke)+'\n');
  return {formatVersion:1,kind:ROLE,...p.input,qualified:true,stockImage:false,derivedImage:true,productionIpc:false,usableGitHistory:true,spec3:false,
    admittedAfterResolvedDigest:true,modelCalls:false,supervisorLoaded:false,packageVersion:VERSION,artifacts,node,smoke,qualifier,
    child:{code:0,signal:null,exitObserved:true,settled:true,failure:null,stdoutBytes:bytes.length,stderrBytes:0,stdoutSha256:sha(bytes),stderrSha256:sha(Buffer.alloc(0))}};
}
assert.deepEqual(await main([],{transport:()=>{throw Error('default called transport');}}),{status:'prepared-no-run',providerCalls:0});
assert.match(REVISION,/^[a-f0-9]{40}$/);
if(/^0{40}$/.test(REVISION))assert.throws(requireTaggedRevision,error=>error.code==='EXACT_TAGGED_REVISION_REQUIRED');
else requireTaggedRevision();
if(/^sha256:0{64}$/.test(DERIVED_IMAGE_DIGEST)) {
  assert.throws(()=>requirePhysicalInputs({image:IMAGE,revision:REVISION,digest}),error=>error.code==='ACTUAL_DERIVED_IMAGE_DIGEST_REQUIRED');
  const before=(await fs.readdir(HERE)).filter(name=>name.startsWith('run-root-seed-'));
  await assert.rejects(main(['--run','--image',IMAGE,'--revision',REVISION,'--digest',digest,'--nonce',randomUUID()]),error=>error.code==='ACTUAL_DERIVED_IMAGE_DIGEST_REQUIRED');
  assert.deepEqual((await fs.readdir(HERE)).filter(name=>name.startsWith('run-root-seed-')),before);
} else {
  requirePhysicalInputs({image:IMAGE,revision:REVISION,digest:DERIVED_IMAGE_DIGEST});
  const wrong=DERIVED_IMAGE_DIGEST==='sha256:'+'e'.repeat(64)?'sha256:'+'d'.repeat(64):'sha256:'+'e'.repeat(64);
  assert.throws(()=>requirePhysicalInputs({image:IMAGE,revision:REVISION,digest:wrong}),error=>error.code==='EXACT_DERIVED_PHYSICAL_INPUTS_REQUIRED');
  await assert.rejects(main(['--run','--image',IMAGE,'--revision',REVISION,'--digest',wrong,'--nonce',randomUUID()]),error=>error.code==='EXACT_DERIVED_PHYSICAL_INPUTS_REQUIRED');
}
new vm.Script('('+guestRoot.toString()+')');new vm.Script('('+guestSmoke.toString()+')');
// Exercise each frozen-input guard independently using an in-memory DATA-only
// module. None of these synthetic substitutions is written to the template.
const ownershipSource=await fs.readFile(path.join(HERE,'ownership.mjs'),'utf8');
for(const [image,elf,source,code]of [
  ['sha256:'+'0'.repeat(64),'0'.repeat(64),'0'.repeat(64),'ACTUAL_DERIVED_IMAGE_DIGEST_REQUIRED'],
  [digest,'0'.repeat(64),'0'.repeat(64),'ACTUAL_QUALIFIER_ELF_REQUIRED'],
  [digest,'a'.repeat(64),'0'.repeat(64),'ACTUAL_SOURCE_TREE_REQUIRED'],
  [digest,'a'.repeat(64),'b'.repeat(64),null]]){
  let text=ownershipSource;for(const [key,value]of Object.entries({DERIVED_IMAGE_DIGEST:image,QUALIFIER_ELF_SHA256:elf,SOURCE_TREE_SHA256:source}))
    text=text.replace(new RegExp('export const '+key+' = [^;]+;'),'export const '+key+' = '+JSON.stringify(value)+';');
  const bound=await import('data:text/javascript;base64,'+Buffer.from(text).toString('base64'));
  const check=()=>bound.requirePhysicalInputs({image:IMAGE,revision:REVISION,digest});
  if(code)assert.throws(check,error=>error.code===code);else check();
}
const results=[];
for(const scenario of ['success','smoke-failure','foreign-instance','uncertain-create-observed','unobserved-create']){
  let clock=1800000000000,live=false,destroyed=false,foreign=false,postDestroyInventories=0,secretChecks=0;
  const nonce=randomUUID(),v={image:IMAGE,revision:REVISION,digest,nonce};
  const p=plan(v,clock,guestRoot,guestSmoke),guard=ownership(p);
  assert.equal(p.name,'rudder-root-seed-'+nonce);assert.equal(p.root,'/run/rudder-root-seed-canary-'+nonce);
  assert.equal(p.input.role,ROLE);assert.equal(p.input.packageVersion,'4.0.0-alpha.23');assert.equal(p.input.baseImageDigest,BASE_IMAGE_DIGEST);
  assert.equal(p.config.image,IMAGE+'@'+digest);
  assert.throws(()=>plan({...v,digest:FORBIDDEN_IMAGE_DIGEST},clock,guestRoot,guestSmoke),error=>error.code==='PROTECTED_HOME_IMAGE');
  const machine={id:'1234567890abcd',instance_id:'01STOCK23FAKEINSTANCE000000',name:p.name,state:'started',config:p.config,image_ref:{digest,labels:{'org.opencontainers.image.revision':REVISION}}};
  assert(guard.matches(machine));guard.remember(machine);
  const omittedEmptyCmd=structuredClone(machine);delete omittedEmptyCmd.config.init.cmd;assert(guard.matches(omittedEmptyCmd));
  const nonemptyCmd=structuredClone(machine);nonemptyCmd.config.init.cmd=["unexpected"];assert.equal(guard.matches(nonemptyCmd),false);
  for(const change of [m=>m.id=FORBIDDEN_ID,m=>m.instance_id=FORBIDDEN_INSTANCE,m=>m.instance_id='foreign',m=>m.config.env.TOKEN='forbidden',m=>m.config.files=[{}],m=>m.config.volumes=[{}],m=>m.config.services=[{}],m=>m.image_ref.digest='sha256:'+'f'.repeat(64),m=>m.image_ref.labels['org.opencontainers.image.revision']='f'.repeat(40),m=>m.config.metadata.qualification_revision='f'.repeat(40)]){
    const changed=structuredClone(machine);change(changed);assert.equal(guard.matches(changed),false);
  }
  const forbiddenImage=structuredClone(machine);forbiddenImage.image_ref.digest=FORBIDDEN_IMAGE_DIGEST;
  assert.equal(guard.matches(forbiddenImage),false);
  const good=proof(p);validateProof(good,p);
  for(const version of ['jj 0.40.0','jj 0.40.0-941d0290f5511db1d1cf8e4fe7365ba7d1828ea0','jj 0.40.1','jj 0.40.0-garbage']){
    const variant=structuredClone(good), command=variant.smoke.commands.find(c=>c.label==='jj-version');command.stdout=encode(Buffer.from(version+'\n'));
    const bytes=Buffer.from(JSON.stringify(variant.smoke)+'\n');variant.child.stdoutBytes=bytes.length;variant.child.stdoutSha256=sha(bytes);
    if(/^jj 0\.40\.0(?:-[0-9a-f]{40})?$/.test(version))validateProof(variant,p);else assert.throws(()=>validateProof(variant,p));
  }
  const bad=structuredClone(good);bad.smoke.commands[0].child.settled=false;assert.throws(()=>validateProof(bad,p));
  for(const change of [q=>q.child.settled=false,q=>q.child.exitObserved=false,q=>q.child.code=1,
    q=>q.child.failure='QUALIFIER_DEADLINE',q=>q.argv.push('--include-ignored'),q=>q.elfMagicVerified=false,
    q=>q.artifact.sha256='f'.repeat(64),q=>q.sourceTreeSha256='f'.repeat(64)]){
    const variant=structuredClone(good);change(variant.qualifier);assert.throws(()=>validateProof(variant,p));
  }
  for(const change of [r=>r.actual_child_reaped=false,r=>r.actual_ebusy_retained_owner=false,r=>r.normal_unmount_verified=false,
    r=>r.own_mount_pin_removed=false,r=>r.fixture_original_pins_removed=false,r=>r.production_ipc=true,r=>r.backend=true,
    r=>r.child.child_cas_owner=0,r=>r.child.private_forks=1,r=>r.child.same_mount_namespace=false,r=>r.child.write_error='EACCES',
    r=>r.child.seed.kind='blob',r=>r.real_source_history=false,r=>r.real_runnable_source_mapping=false,r=>r.original_owner_checks=0,r=>r.child.environment_cleared=false,r=>r.child.real_git_head=false,r=>r.child.real_parent_history=false,r=>r.child.first_fork_new_commit_operation=false,r=>r.child.second_fork_unchanged=false,r=>r.child.new_object_absent_from_second=false,r=>r.child.new_baseline.commit=r.baseline.commit,r=>r.child.new_operation=r.operation,r=>r.child.new_baseline.parents=['0'.repeat(40)],r=>r.models=true,r=>r.spec3=true]){
    const variant=structuredClone(good);change(variant.qualifier.receipt);refreshQualifier(variant.qualifier);assert.throws(()=>validateProof(variant,p));
  }
  for(const text of ['running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.00s\n',
    qualifierText(good.qualifier.receipt).replace('1 passed; 0 failed','0 passed; 1 failed'),
    qualifierText(good.qualifier.receipt)+'\n'+JSON.stringify(good.qualifier.receipt)+'\n']){
    const variant=structuredClone(good);refreshQualifier(variant.qualifier,text);assert.throws(()=>validateProof(variant,p));
  }
  const privateSentinel='DO_NOT_PERSIST_EXISTING_HOME_PROVIDER_ENV';
  const transport=async(args,options)=>{
    assert(!args.includes('--skip-secrets'));assert(!args.includes(FORBIDDEN_ID));assert(!args.includes(FORBIDDEN_INSTANCE));
    assert(options.timeout>0&&options.timeout<=60000);
    let value,ok=true;
    if(args[0]==='version')value='flyctl v0.4.118 linux/amd64';
    else if(args[0]==='secrets'){secretChecks++;value=[];}
    else if(args[1]==='list'){
      if(destroyed)postDestroyInventories++;
      const protectedHome={id:FORBIDDEN_ID,instance_id:FORBIDDEN_INSTANCE,name:'protected-home',config:{env:{TOKEN:privateSentinel}}};
      value=[protectedHome,...(live?[{...machine,instance_id:foreign?'01FOREIGNINSTANCE0000000000':machine.instance_id}]:[])];
    }else if(args[1]==='run'){
      assert.equal(args[2],IMAGE+'@'+digest);
      assert.deepEqual(JSON.parse(await fs.readFile(args[args.indexOf('--machine-config')+1],'utf8')),p.config);
      new vm.Script(p.config.init.entrypoint.at(-1));live=scenario!=='unobserved-create';ok=!scenario.startsWith('uncertain')&&scenario!=='unobserved-create';value={created:live};
    }else if(args[1]==='destroy'){
      assert.equal(args[2],machine.id);assert(!foreign,'foreign instance was targeted');destroyed=true;live=false;value={destroyed:true};
    }else if(args[1]==='exec'){
      assert(live&&!foreign);assert.equal(args[2],machine.id);assert(args[5].startsWith('/usr/bin/env -i '));
      if(options.label==='admission'){value={status:'admitted',nonce,imageDigest:digest};if(scenario==='foreign-instance')foreign=true;}
      else if(options.label==='proof')value=scenario==='smoke-failure'?{...good,qualified:false,failure:'BOUNDED_FAKE_SMOKE_FAILURE'}:good;
      else if(options.label==='recover-private-logs')value={nonce,logs:{'child.stdout.private':encode(Buffer.from('bounded fake failure log\n')),'child.stderr.private':encode(Buffer.alloc(0))}};
      else throw Error('unexpected fake exec');
    }else throw Error('unexpected fake transport');
    const stdout=typeof value==='string'?value:JSON.stringify(value);
    return {ok,stdout,diagnostics:{settled:true,status:ok?0:1,signal:null,failure:null,stdoutBytes:Buffer.byteLength(stdout),stderrBytes:0,stdoutSha256:sha(stdout),stderrSha256:sha(Buffer.alloc(0))}};
  };
  const result=await main(['--run',...Object.entries(v).flatMap(([k,value])=>['--'+k,value])],{transport,now:()=>clock,pause:async ms=>{clock+=ms;}});
  const expected=['success','uncertain-create-observed'].includes(scenario);
  assert.equal(result.providerCalls,0);assert.equal(result.qualified,false);assert.equal(result.testOnlyMock,true);assert.equal(result.mockPassed,expected);
  assert.equal(result.cleanupRequired,['foreign-instance','unobserved-create'].includes(scenario));
  assert.equal(result.reconciliationRequired,scenario==='unobserved-create');
  assert.equal(destroyed,!['foreign-instance','unobserved-create'].includes(scenario));
  if(destroyed)assert(postDestroyInventories>=2);
  if(expected)assert.equal(secretChecks,2);
  const directory=path.dirname(result.report);
  for(const name of await fs.readdir(directory))assert(!(await fs.readFile(path.join(directory,name),'utf8')).includes(privateSentinel),'provider environment leaked into retained logs');
  if(scenario==='smoke-failure'){
    assert((await fs.readdir(directory)).includes('guest-proof.json'));
    assert.equal(await fs.readFile(path.join(directory,'recovered-child.stdout.private'),'utf8'),'bounded fake failure log\n');
  }
  results.push({scenario,mockPassed:result.mockPassed,cleanupVerified:result.cleanupVerified,independentAbsence:result.independentAbsence,reconciliationRequired:result.reconciliationRequired,foreignInstanceUntouched:scenario!=='foreign-instance'||!destroyed});
  await fs.rm(directory,{recursive:true});
}
const result={status:'passed',fakeTransportScenarios:results.length,providerCalls:0,actualSmokeCommands:0,compiled:false,
  sentinelRevision:/^0{40}$/.test(REVISION),realTransportSentinelGuardChecked:true,
  sentinelImageDigest:/^sha256:0{64}$/.test(DERIVED_IMAGE_DIGEST),sentinelElf:/^0{64}$/.test(QUALIFIER_ELF_SHA256),sentinelSourceTree:/^0{64}$/.test(SOURCE_TREE_SHA256),realTransportImageSentinelGuardChecked:true,threeFrozenInputGuardsChecked:true,
  protectedHomeIdInstanceImageChecked:true,jjPin:'0.40.0',results};
await fs.writeFile(path.join(HERE,'selftest-result.json'),JSON.stringify(result,null,2)+'\n',{mode:0o600});
console.log(JSON.stringify({status:result.status,fakeTransportScenarios:results.length,providerCalls:0,actualSmokeCommands:0}));
