// Fake transport and source parsing only. No provider, smoke command or compiler.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import vm from 'node:vm';
import {randomUUID} from 'node:crypto';
import {fileURLToPath} from 'node:url';
import {main,validateProof} from './controller.mjs';
import {plan,ownership,sha,requireTaggedRevision,requirePhysicalInputs,STOCK_IMAGE_DIGEST,IMAGE,REVISION,VERSION,ROLE,FORBIDDEN_ID,FORBIDDEN_INSTANCE,FORBIDDEN_IMAGE_DIGEST} from './ownership.mjs';
import {guestRoot,guestSmoke} from './guest.mjs';
const HERE=path.dirname(fileURLToPath(import.meta.url));
const encode=b=>({bytes:b.length,sha256:sha(b),base64:b.toString('base64')});
const digest='sha256:'+'2'.repeat(64); // synthetic only; not a stock22 image receipt
function proof(p){
  const paths={node:'/usr/local/bin/node',native:'/opt/rudder/dist/native/rudder-native',buddy:'/opt/rudder/dist/native/rudder-jj',cli:'/opt/rudder/dist/index.js',package:'/opt/rudder/package.json'};
  const artifacts=Object.fromEntries(Object.entries(paths).map(([k,file])=>[k,{path:file,bytes:10,sha256:'a'.repeat(64)}]));
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
  return {formatVersion:1,kind:ROLE,...p.input,qualified:true,stockImage:true,admittedAfterResolvedDigest:true,modelCalls:false,supervisorLoaded:false,packageVersion:VERSION,artifacts,node,smoke,
    child:{code:0,signal:null,exitObserved:true,settled:true,failure:null,stdoutBytes:bytes.length,stderrBytes:0,stdoutSha256:sha(bytes),stderrSha256:sha(Buffer.alloc(0))}};
}
assert.deepEqual(await main([],{transport:()=>{throw Error('default called transport');}}),{status:'prepared-no-run',providerCalls:0});
assert.match(REVISION,/^[a-f0-9]{40}$/);
if(/^0{40}$/.test(REVISION))assert.throws(requireTaggedRevision,error=>error.code==='EXACT_TAGGED_REVISION_REQUIRED');
else requireTaggedRevision();
if(/^sha256:0{64}$/.test(STOCK_IMAGE_DIGEST)) {
  assert.throws(()=>requirePhysicalInputs({image:IMAGE,revision:REVISION,digest}),error=>error.code==='ACTUAL_STOCK23_IMAGE_DIGEST_REQUIRED');
  const before=(await fs.readdir(HERE)).filter(name=>name.startsWith('run-stock23-'));
  await assert.rejects(main(['--run','--image',IMAGE,'--revision',REVISION,'--digest',digest,'--nonce',randomUUID()]),error=>error.code==='ACTUAL_STOCK23_IMAGE_DIGEST_REQUIRED');
  assert.deepEqual((await fs.readdir(HERE)).filter(name=>name.startsWith('run-stock23-')),before);
} else {
  requirePhysicalInputs({image:IMAGE,revision:REVISION,digest:STOCK_IMAGE_DIGEST});
  const wrong=STOCK_IMAGE_DIGEST==='sha256:'+'e'.repeat(64)?'sha256:'+'d'.repeat(64):'sha256:'+'e'.repeat(64);
  assert.throws(()=>requirePhysicalInputs({image:IMAGE,revision:REVISION,digest:wrong}),error=>error.code==='EXACT_STOCK23_PHYSICAL_INPUTS_REQUIRED');
  await assert.rejects(main(['--run','--image',IMAGE,'--revision',REVISION,'--digest',wrong,'--nonce',randomUUID()]),error=>error.code==='EXACT_STOCK23_PHYSICAL_INPUTS_REQUIRED');
}
new vm.Script('('+guestRoot.toString()+')');new vm.Script('('+guestSmoke.toString()+')');
const results=[];
for(const scenario of ['success','smoke-failure','foreign-instance','uncertain-create-observed','unobserved-create']){
  let clock=1800000000000,live=false,destroyed=false,foreign=false,postDestroyInventories=0,secretChecks=0;
  const nonce=randomUUID(),v={image:IMAGE,revision:REVISION,digest,nonce};
  const p=plan(v,clock,guestRoot,guestSmoke),guard=ownership(p);
  assert.equal(p.name,'rudder-stock23-'+nonce);assert.equal(p.root,'/run/rudder-stock23-'+nonce);
  assert.equal(p.input.role,'alpha23-stock-worker-smoke');assert.equal(p.input.packageVersion,'4.0.0-alpha.23');
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
  sentinelImageDigest:/^sha256:0{64}$/.test(STOCK_IMAGE_DIGEST),realTransportImageSentinelGuardChecked:true,
  protectedHomeIdInstanceImageChecked:true,jjPin:'0.40.0',results};
await fs.writeFile(path.join(HERE,'selftest-result.json'),JSON.stringify(result,null,2)+'\n',{mode:0o600});
console.log(JSON.stringify({status:result.status,fakeTransportScenarios:results.length,providerCalls:0,actualSmokeCommands:0}));
