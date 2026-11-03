// Exercise the entry's pure pre-effect selection using the production validator.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import vm from 'node:vm';
import {validateStagingCodexAccessSnapshot} from '../cloud/dist/managed-home-provisioning/staging-codex-profile-import.js';
import {validatePositiveIngressInput} from '../cloud/dist/managed-home-provisioning/staging-positive-ingress-driver.js';

const source=await fs.readFile(new URL('../examples/managed-runtime-image/qualify-runtime-role.mjs',import.meta.url),'utf8');
const begin=source.indexOf('const exact='),end=source.indexOf('// Optional identity/restriction DATA');
assert(begin>=0&&end>begin);
const select=vm.runInNewContext('const GENUINE_ENTRY_EFFECTS_ALLOWED=false;\n'+source.slice(begin,end)+'\nqualificationSelection;', {validatePositiveIngressInput,validateStagingCodexAccessSnapshot});
const positive=()=>({openCommandKey:'open:1',startCommandKey:'start:1',model:'explicit-model',profileId:'explicit-profile',text:'Explicit task'});
const flow=()=>({format_version:1,qualification_kind:'sdk-runtime-ordinary-v2',postgres:{},candidate:{},account_id:'account',command_key:'ordinary',activation_authority:{},services:{},supervisor:{}});

test('positive selection accepts Open/Start with optional distinct Stop and copies input',()=>{
 for(const input of [positive(),{...positive(),stopCommandKey:'stop:1'}]){
  const original=JSON.stringify(input),selected=select({...flow(),positive_ingress:input});
  assert.equal(selected.flow,true);assert.equal(selected.genuine,false);
  assert.deepEqual(selected.positiveInput,input);
  assert.notEqual(selected.positiveInput,input);assert(Object.isFrozen(selected.positiveInput));
  assert.equal(JSON.stringify(input),original);
 }
});

test('ordinary and original runtime/compiler modes keep their closed input shapes',()=>{
 assert.equal(select(flow()).positiveInput,undefined);
 for(const qualification_kind of ['runtime-image-v1','compiler-sdk-v1']){
  const input={format_version:1,qualification_kind,postgres:{},candidate:{},account_id:'a',command_key:'k'};
  assert.equal(select(input).flow,false);
  assert.throws(()=>select({...input,positive_ingress:positive()}));
 }
});

test('genuine remains disabled and cannot be combined with positive ingress',()=>{
 for(const input of [{...flow(),genuine_ingress:{}},{...flow(),genuine_ingress:{},positive_ingress:positive()}])assert.throws(()=>select(input));
});

test('malformed selection refuses before any output or driver ownership',()=>{
 for(const input of [null,{},'text',{...positive(),extra:true},{...positive(),startCommandKey:'open:1'},
  {...positive(),stopCommandKey:'start:1'},{...positive(),stopCommandKey:null},
  {...positive(),model:''},{...positive(),profileId:'bad\nprofile'},{...positive(),text:'\0'}]){
  assert.throws(()=>select({...flow(),positive_ingress:input}));
 }
 for(const input of [{...flow(),extra:true},{...flow(),format_version:true},null,{}])assert.throws(()=>select(input));
 // The selection validator is reached before the entry creates output/storage.
 assert(source.indexOf('qualificationSelection(i);')<source.indexOf('await fs.mkdir(output'));
});

test('the accepted positive input occupies the existing flow eighth argument',()=>{
 const call=source.match(/driver=new StagingSdkRuntimeFlow\([^;]+\)/)?.[0];assert(call);
 let actual;
 const i={activation_authority:{},candidate:{},account_id:'a',command_key:'k'};
 vm.runInNewContext(call,{driver:undefined,StagingSdkRuntimeFlow:class{constructor(...values){actual=values;}},full:{},i,setup:{},genuineFlow:undefined,positiveInput:positive(),positiveProfileSnapshot:undefined});
 assert.equal(actual.length,9);assert.equal(actual[8],undefined);assert.deepEqual(actual[7],positive());
});

const installer=await fs.readFile(new URL('../examples/managed-runtime-image-staging/install.mjs',import.meta.url),'utf8');
const project=vm.runInNewContext("const assert=ok=>{if(!ok)throw Error();};\n"+
 installer.slice(installer.indexOf('function projectPositiveIngressData'),installer.indexOf('const sig='))+'\nprojectPositiveIngressData;', {validatePositiveIngressInput});

test('installer forwards only valid positive flow selection before PG effects',()=>{
 assert.equal(project({},true,false),undefined);
 const value=project({positive_ingress:positive()},true,false);
 assert.deepEqual(value,positive());assert(Object.isFrozen(value));
 for(const [flow,genuine] of [[false,false],[true,true],[false,true]])assert.throws(()=>project({positive_ingress:positive()},flow,genuine));
 assert.throws(()=>project({positive_ingress:{...positive(),extra:true}},true,false));
 assert(installer.indexOf('projectPositiveIngressData(plan,flow,genuine);')<installer.indexOf('admin=new pg.Client'));
 // Execute the actual installer cfg expression with disposable public fixtures.
 const cfg=installer.slice(installer.indexOf('const cfg='));
 assert(cfg.startsWith('const cfg='));
 const expression=cfg.slice(0,cfg.indexOf(';')+1);
 const actual=vm.runInNewContext(expression+'\ncfg;',{
  qualificationKind:'sdk-runtime-ordinary-v2',config:()=>({}),ROLE:{runtime:'r',registration:'g'},
  secrets:{runtime:'r',registration:'g',encryption:'e'},policy:{},plan:{fly_token:'synthetic',account_id:'a',command_key:'k'},
  candidate:{},flow:true,ac:{},services:{},state:{},genuineIngress:undefined,positiveIngress:value,profileSnapshot:false,
 });
 assert.equal(actual.positive_ingress,value);assert.equal(actual.genuine_ingress,undefined);
});


test('private access snapshot is selected only with matching positive flow and checked before effects',()=>{
 const exp=Math.floor(Date.now()/1000)+3600;
 const claims={exp,"https://api.openai.com/auth":{chatgpt_account_id:"test-account"}};
 const token=Buffer.from('{}').toString('base64url')+'.'+Buffer.from(JSON.stringify(claims)).toString('base64url')+'.c2ln';
 const snapshot={formatVersion:1,kind:'CodexOAuthAccessSnapshotV1',profileId:positive().profileId,backend:'codex',provider:'openai-codex',accessToken:token,chatgptAccountId:'test-account',expiresAtMs:exp*1000};
 const selected=select({...flow(),positive_ingress:positive(),positive_profile_snapshot:snapshot});
 assert(Buffer.isBuffer(selected.positiveProfileSnapshot));
 assert.deepEqual(JSON.parse(selected.positiveProfileSnapshot),snapshot);
 for(const input of [
  {...flow(),positive_profile_snapshot:snapshot},
  {...flow(),positive_ingress:positive(),positive_profile_snapshot:{...snapshot,profileId:'foreign'}},
  {...flow(),positive_ingress:positive(),positive_profile_snapshot:{...snapshot,extra:true}},
  {...flow(),positive_ingress:positive(),positive_profile_snapshot:{...snapshot,expiresAtMs:1}},
 ])assert.throws(()=>select(input));
});
