"""Pure host adapter checks. Private retained baseline integration is optional."""
import ast,copy,hashlib,importlib.util,json,os,subprocess,tempfile,unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
ROOT=Path(__file__).resolve().parents[1];HERE=ROOT/'examples/managed-runtime-image-staging'
def module(name,path):
 s=importlib.util.spec_from_file_location(name,path);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
D=module('derive',HERE/'derive-lifecycle.py');H=D.h
BASE=Path('/workspace/rudder-source37-historical-controller-compatibility-bound-source-v1/reused/adapter/baseline')
def positive(stop=False):
 v=dict(openCommandKey='open:1',startCommandKey='start:1',model='explicit-model',profileId='explicit-profile',text='Explicit task')
 if stop:v['stopCommandKey']='stop:1'
 return v
def binding(root=Path('/workspace/fixture')):
 capture=dict(sourceRevision='a'*40,sourceArchiveSha256='b'*64,generatedJsSha256=H.digest({}))
 c=dict(formatVersion=1,kind='SAME_CAPTURE_CONTROLLER_BUILD_DATA',**capture,contextManifestSha256='c'*64,image='registry.fly.io/rudder-workers-exla:controller',digest='sha256:'+'d'*64,originalBuild=dict(actualWait=True,exitCode=0,pidfdHeld=True,pipeEofClaimed=False,logSha256='e'*64))
 refs={k:dict(path=str(root/(k+'.json')),sha256='f'*64)for k in ['workerBuildReceipt','controllerBuildReceipt','volumeReceipt','pgEvidence','s3Evidence']}
 b=dict(formatVersion=1,capture=capture,controller=dict(c,receiptSha256=refs['controllerBuildReceipt']['sha256']),worker_image='registry.fly.io/rudder-workers-exla@sha256:'+'1'*64,positive_ingress=positive(),control_app_id='actual-app-id',controller_source_sha256='2'*64,binder_source_sha256='3'*64,inputs=refs)
 b.update(dict(bundle_directory=str(root/'run/bundle'),fresh_volume_receipt_path=refs['volumeReceipt']['path'],controller_source_path=str(root/'run/controller/run.py'),control_output_path=str(root/'run/control-run'),control_build_receipt_path=refs['controllerBuildReceipt']['path'],cleanup_output_path=str(root/'run/control-run/operator-cleanup-v1'),credential_store_path=str(root/'credentials.json'),binder_completion_path=str(root/'run/gates/completion.json'),boot_binding_path=str(root/'run/gates/boot.json'),source_directory=str(root/'source')))
 return b,c
class Host(unittest.TestCase):
 def test_fractional_physical_report_timings_keep_strict_protocol_receipts(self):
  script=r'''
import assert from 'node:assert/strict';
import {parseStrict,parseReportStrict} from './examples/managed-runtime-image-staging/host-contract.mjs';
const raw='{"componentPassed":true,"calls":[{"elapsedSeconds":0.125,"budget":1.8e2}]}';
assert.deepEqual(parseReportStrict(raw),JSON.parse(raw));
assert.throws(()=>parseStrict(raw));
for(const bad of ['{"elapsed":1e999}','{"elapsed":.1}','{"elapsed":01}','{"elapsed":1.}','{"x":0.1,"x":0.2}','{"__proto__":{}}'])assert.throws(()=>parseReportStrict(bad));
assert.deepEqual(parseStrict('{"epoch":180}'),{epoch:180});
'''
  result=subprocess.run(['node','--input-type=module','-e',script],cwd=ROOT,capture_output=True,text=True,timeout=10)
  self.assertEqual(result.returncode,0,result.stderr)
 def test_same_capture_rejects_changed_revision_archive_and_generated_map(self):
  b,c=binding();w={**b['capture'],'generatedJs':{}};self.assertEqual(D.same_capture(w,c),b['capture'])
  for key in b['capture']:
   wrong=copy.deepcopy(c);wrong[key]='9'*len(wrong[key])
   with self.assertRaises(H.Refusal):D.same_capture(w,wrong)
  with self.assertRaises(H.Refusal):D.same_capture(dict(w,generatedJs={'other.js':{}}),c)
 def test_binding_has_no_historical_or_genuine_override(self):
  b,_=binding();H.validate_binding(b)
  for change in [dict(genuine_ingress={}),dict(worker_image='mutable:tag'),dict(capture=dict(b['capture'],sourceRevision='0'*40))]:
   with self.assertRaises(H.Refusal):H.validate_binding(dict(b,**change))
 def test_original_completion_exact_raw_association(self):
  b,_=binding();nonce='11111111-1111-4111-8111-111111111111';boot=H.validate_boot(dict(bundle_directory=b['bundle_directory'],original_boot_nonce=nonce,root_observed_original_ready_identity_sha256='4'*64),b)
  inputs={k:(k+'\n').encode()for k in ['operator','services','admission']};bound=b'bound\n';v=dict(format_version=1,kind='ROOT_OBSERVED_ORIGINAL_BINDER_TERMINAL_DATA_V1',original_tool_session='123',actual_exit_code=0,actual_wait=True,binder_source_sha256=b['binder_source_sha256'],boot_nonce=nonce,bound_sha256=hashlib.sha256(bound).hexdigest(),original_ready_identity_sha256='4'*64,qualified=False,custody_granted=False,clock_created=False)
  v.update({k+'_sha256':hashlib.sha256(raw).hexdigest()for k,raw in inputs.items()});H.validate_binder_completion(D.encode(v),bound,inputs,b,nonce,boot)
  for wrong in [dict(actual_exit_code=True),dict(actual_wait=False),dict(qualified=True),dict(original_tool_session='other')]:
   with self.assertRaises(H.Refusal):H.validate_binder_completion(D.encode(dict(v,**wrong)),bound,inputs,b,nonce,boot)
  with self.assertRaises(H.Refusal):H.validate_binder_completion(D.encode(v),bound+b' ',inputs,b,nonce,boot)
 def flow(self,stop=False):
  receipt=dict(homeId='home',projects=[dict(projectId='project',workspaceId='workspace')]);custody=dict(owners_retained=True,teardown_authority=False,pending_effect=None,home_id='home',started=dict(id='machine',instance_id='instance'))
  p=dict(scope='positive-ingress-observation-only',owners_retained=True,refused=False,backend_ready_last_observed=True,qualified=False,retirement_complete=False,fence=dict(run_id='run',run_epoch='1',attempt_id='attempt'),commands=[dict(command_key=k,dispatch_unknown=False,receipt_queries=1)for k in ['open:1','start:1']+(['stop:1']if stop else[])])
  p.update({k:True for k in ['lifecycle_complete','enrollment_release_proved','original_helper_join_proved','checkpoint_accepted']});p['lifecycle_observation']={k:True for k in ['enrollment_submitted_observed','enrollment_released_observed','helper_joined_observed','stop_requested_observed','validated_checkpoint_observed']}
  flow=dict(stage='ordinary-positive-lifecycle-observed-data'if stop else'ordinary-positive-backend-ready-data',failed=False,owners_retained=True,activation=dict(scope='original-staging-admin-transition-data-only',attempts=[dict(role=r,known_commit=True,unknown=False,permit_retained=True)for r in ['sdk','runtime']]),ordinary=dict(scope='ordinary-source-sdk-staging-data-only',expired=False,error=False,owners_retained=True,receipt=receipt,custody=custody,positive_ingress=p))
  for role,state in [('sdk','sdk_role_commit_observed'),('runtime','role_commit_observed')]:flow[role]=dict(state=state,expired=False,error=None,owners_retained=True,ack_send_unknown=False,ack_send_attempted=True,publication_ack_data={})
  return flow,dict(captured_from_same_original_supervisor=True,teardown_authority=False,receipt=receipt,custody=custody)
 def test_completion_requires_same_original_source_custody_and_known_commits(self):
  flow,ordinary=self.flow();self.assertTrue(H.completion(flow,positive(),ordinary))
  for edit in [lambda f:f['sdk'].update(ack_send_unknown=True),lambda f:f['activation']['attempts'][0].update(known_commit=False),lambda f:f['ordinary']['positive_ingress']['commands'][1].update(command_key='wrong'),lambda f:f['ordinary']['positive_ingress'].update(fence=None)]:
   wrong=copy.deepcopy(flow);edit(wrong)
   with self.assertRaises(H.Refusal):H.completion(wrong,positive(),ordinary)
  with self.assertRaises(H.Refusal):H.completion(flow,positive(),dict(ordinary,captured_from_same_original_supervisor=False))
 def test_stop_requires_all_join_checkpoint_enrollment_observations(self):
  flow,ordinary=self.flow(True);self.assertTrue(H.completion(flow,positive(True),ordinary))
  for key in flow['ordinary']['positive_ingress']['lifecycle_observation']:
   wrong=copy.deepcopy(flow);wrong['ordinary']['positive_ingress']['lifecycle_observation'][key]=False
   with self.assertRaises(H.Refusal):H.completion(wrong,positive(True),ordinary)
 @unittest.skipUnless(BASE.is_dir(),'retained full baseline not available on this host')
 def test_full_original_owner_gate_and_clocks_are_retained(self):
  with tempfile.TemporaryDirectory()as tmp:
   root=Path(tmp);b,c=binding(root);refs=b['inputs'];w=dict(sourceDirectory=b['source_directory'],image='registry.fly.io/rudder-workers-exla:worker',digest='sha256:'+'1'*64)
   cfg=dict(positiveIngress=b['positive_ingress'],controlAppId=b['control_app_id'],credentialStorePath=b['credential_store_path'])
   with patch.object(D,'validate_inputs',return_value=(refs,w,b['controller'],b['capture'])):rows,actual,original=D.render(BASE,cfg,root/'run')
   text=rows['controller/run.py'].decode();self.assertIn('self.end=self.begin+900;self.cleanup_end=self.begin+1080',text);self.assertIn("while not(os.path.lexists(self.bundle/'bound.json')and os.path.lexists(BINDING['binder_completion_path']))",text)
   self.assertEqual(rows['supervision/original_controller.py'],original['supervision/original_controller.py']);self.assertEqual(rows['cleanup/cleanup.py'].replace(b'FINALIZED = True',b'FINALIZED = False'),original['cleanup/cleanup.py'])
   self.assertIn(b"'positive_ingress'",rows['producer/bind.mjs']);self.assertNotIn(b'genuine_ingress',rows['producer/bind.mjs'])
   for filename in rows:
    if filename.endswith('.py'):ast.parse(rows[filename])
 def test_exclusive_static_writer_never_replaces_existing_file(self):
  with tempfile.TemporaryDirectory()as tmp:
   p=Path(tmp)/'data';D.write_new(p,b'first')
   with self.assertRaises(FileExistsError):D.write_new(p,b'second')
   self.assertEqual(p.read_bytes(),b'first');self.assertEqual(p.stat().st_mode&0o777,0o600)
 def test_actual_production_policy_candidate_and_positive_validators(self):
  b,c=binding();payload=dict(binding=b,controller=c)
  script=r'''
import assert from 'node:assert/strict';
import {operatorData} from './examples/managed-runtime-image-staging/prepare-operator.mjs';
import * as types from './cloud/dist/managed-home-provisioning/types.js';
import * as protocol from './cloud/dist/managed-home-provisioning/compiler-qualification-protocol.js';
import * as catalog from './cloud/dist/managed-home-provisioning/compiler-sdk-catalog.js';
import * as positive from './cloud/dist/managed-home-provisioning/staging-positive-ingress-driver.js';
import fs from 'node:fs';
import ts from 'typescript';
const cloudTypes=new URL('./cloud/dist/managed-home-provisioning/types.js',import.meta.url).href;
const source=fs.readFileSync('./cloud/src/managed-home-provisioning/staging-codex-profile-import.ts','utf8');
const emitted=ts.transpileModule(source,{compilerOptions:{module:ts.ModuleKind.ESNext,target:ts.ScriptTarget.ES2022}}).outputText.replaceAll('from "./types.js"','from '+JSON.stringify(cloudTypes));
const profile=await import('data:text/javascript;base64,'+Buffer.from(emitted).toString('base64'));
const {binding:b,controller}=JSON.parse(process.env.STAGING_TEST_DATA),ref=(kind,digest)=>({kind,algorithm:'sha256',digest:digest.repeat(64),size_bytes:230});
const files=Object.fromEntries(['/opt/rudder/dist/native/rudder-native','/opt/rudder/dist/native/rudder-jj','/usr/local/bin/rudder-machine-root','/usr/local/bin/rudder-root-storage'].map(p=>[p,{sha256:'a'.repeat(64),bytes:1}]));
const product={sdkQualified:false,runtimeQualified:false,managedFiles:files,managedMetadata:{'/usr/local/share/rudder-managed-image/sidecar-policy-pin.json':{sha256:'b'.repeat(64),bytes:1}},components:{components:[{path:'/usr/local/bin/node',sha256:'c'.repeat(64)}]},runtimeManifest:{profile_id:'fly-managed-image-v1',manifest_sha256:'d'.repeat(64),entries:[{path:'/opt/rudder-managed-runtime/v1/fixture',sha256:'e'.repeat(64),size_bytes:1}]},runtimeData:{layout:'baked-image-cas-v1',total_bytes:1,inventory:ref('blob','f'),roots:{manifest:ref('blob','1'),installer_input:ref('directory','2')}}};
const values={controllerBuildReceipt:controller,workerBuildReceipt:{...b.capture,sourceQualified:false,sdkQualified:false,runtimeQualified:false,image:'registry.fly.io/rudder-workers-exla:worker',digest:'sha256:'+'1'.repeat(64),commands:[{actualWait:true,exitCode:0,pidfdHeld:true,pipeEofClaimed:false}],generatedJs:{},productData:product},volumeReceipt:{kind:'operator-reviewed-fresh-volume-component-v1',isolated_only:true,component_passed:true,cleanup_verified:true,filesystem_write_fsync_rename_readback:true,root_sdk_source_authority:false,cold_restart_durability_qualified:false},pgEvidence:{ok:true,cleanupVerified:true},s3Evidence:{actualCasPassed:true}};
const api={...types,...protocol,...catalog,...positive,...profile},raw=operatorData(b,values,api,'synthetic credential for pure tests');assert.equal(Object.keys(raw).length,11);
const operator=JSON.parse(raw['operator.template.private.json']);assert.deepEqual(operator.positive_ingress,b.positive_ingress);assert.equal(operator.candidate.source_manifest_sha256,b.inputs.workerBuildReceipt.sha256);assert(!Object.hasOwn(operator,'genuine_ingress'));
for(const [key,change]of [['controllerBuildReceipt',v=>v.sourceRevision='0'.repeat(40)],['volumeReceipt',v=>v.cleanup_verified=false],['workerBuildReceipt',v=>v.commands[0].actualWait=false]]){const wrong=structuredClone(values);change(wrong[key]);assert.throws(()=>operatorData(b,wrong,api,'synthetic credential for pure tests'));}
assert.throws(()=>operatorData({...b,positive_ingress:{...b.positive_ingress,stopCommandKey:'start:1'}},values,api,'synthetic credential for pure tests'));
const exp=Math.floor(Date.now()/1000)+3600,claims={exp,'https://api.openai.com/auth':{chatgpt_account_id:'synthetic-account'}};
const accessToken='e30.'+Buffer.from(JSON.stringify(claims)).toString('base64url')+'.c2ln';
const snap={formatVersion:1,kind:'CodexOAuthAccessSnapshotV1',profileId:b.positive_ingress.profileId,backend:'codex',provider:'openai-codex',accessToken,chatgptAccountId:'synthetic-account',expiresAtMs:exp*1000};
const withSnapshot={...b,inputs:{...b.inputs,positiveProfileSnapshot:{path:'/workspace/fixture/snapshot.json',sha256:'a'.repeat(64)}}};
const snapshots={...values,positiveProfileSnapshot:snap};
assert.deepEqual(JSON.parse(operatorData(withSnapshot,snapshots,api,'synthetic credential for pure tests')['operator.template.private.json']).positive_profile_snapshot,snap);
for(const change of [{profileId:'wrong'},{refreshToken:'forbidden'}, {expiresAtMs:0}])assert.throws(()=>operatorData(withSnapshot,{...snapshots,positiveProfileSnapshot:{...snap,...change}},api,'synthetic credential for pure tests'));
console.log('HOST_PRODUCTION_PURE_CHECKS_PASSED');
'''
  result=subprocess.run(['node','--input-type=module','-e',script],cwd=ROOT,env=dict(os.environ,STAGING_TEST_DATA=json.dumps(payload)),capture_output=True,text=True,timeout=30)
  self.assertEqual(result.returncode,0,result.stderr);self.assertEqual(result.stdout.strip(),'HOST_PRODUCTION_PURE_CHECKS_PASSED')
if __name__=='__main__':unittest.main()
