#!/usr/bin/env python3
"""Derive one full retained staging lifecycle, or publish its externally observed gates.
No provider operation is performed by this adapter. Receipts are DATA; the caller
must observe the actual original build/binder/supervisor tools, never replay them.
"""
import argparse,ast,hashlib,importlib.util,json,os,re,stat
from pathlib import Path
HERE=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('staging_host_contract',HERE/'host-contract.py');h=importlib.util.module_from_spec(spec);spec.loader.exec_module(h)
BASELINE_FILES=('controller/run.py','controller/exec-wrapper.py','controller/guest-read.py','controller/nonsecret_policy.py','supervision/run.py','supervision/original_controller.py','cleanup/cleanup.py','cleanup/canonical.mjs','producer/bind.mjs','producer/durable-bundle.mjs','controller/genuine_contract.py')
# Original owner source identities. These are recovery/compatibility DATA, not
# independent approval or a transfer of any historical qualification.
BASELINE_SHA={'controller/run.py': '0bec97aba30bab1dc3a3510f15397372f01d265f2edd9c00992267b56cbe2548', 'controller/exec-wrapper.py': '280231a36f45a4b04cd2a0660924339610d1257d491d5986fc3a796cb8516bcf', 'controller/guest-read.py': 'dc6a980a781f2c41d563441d80df630dfe592f4f17051b154386088d3087f627', 'controller/nonsecret_policy.py': '66e4f8dd9c1de68bcd43bb8c668e869279c9b6ddbceda3019e77b384a7cd79fa', 'supervision/run.py': '9ef8e812dd7236d9601efcdaba0e856e9b50932e163b842da83bc79873a1c75f', 'supervision/original_controller.py': 'e72bee91aa3aeeff21714e6d624e06a47de569e9dfa819d8c98be5ebf70377f9', 'cleanup/cleanup.py': 'd6ecc73d2facdd83746fde4a5f68969f7b4e567454a012861ef6537105f901e7', 'cleanup/canonical.mjs': 'b4d5b8e7fbdce49d02f009d9f8861a88199fcb247859e33b70cf88542b6c8270', 'producer/bind.mjs': 'f3cf3e300bf6e66c6d4b64342141f9cac5f792176d045217a411a468705a332c', 'producer/durable-bundle.mjs': '36805fc8f7225bb536777b35760ade3bd9d62afe8c498c0fba3ece909e0b7576', 'controller/genuine_contract.py': '282320c4c5f943a226f023f75c50afae249fca7d4c4c787483bb137e39c65d96'}
PROTECTED_HOME={'app':'rudder-workers-exla','id':'8edd225a7d0378','instanceId':'01M670MG4A8E0DWE9GA1C0PQ92','digest':'404f46396aca77324d9b53c55d2824a5401af56ba5362d0bdae67e16108fc245'}

def encode(v):return h.canonical(v)+b'\n'
def replace_once(text,before,after):
 h.need(text.count(before)==1);return text.replace(before,after,1)
def protected_home(config):
 value=config.get('protectedHome',PROTECTED_HOME)
 h.exact(value,['app','id','instanceId','digest'])
 h.need(value['app']==PROTECTED_HOME['app']and value['id']==PROTECTED_HOME['id'])
 h.need(type(value['instanceId'])is str and re.fullmatch('[A-Za-z0-9]{20,64}',value['instanceId'])is not None)
 h.need(type(value['digest'])is str and re.fullmatch('[a-f0-9]{64}',value['digest'])is not None)
 return dict(value)
def rewrite_protected(rows,expected):
 # Only the known identity literals change. The baseline hashes are checked
 # before this adapter runs; every pre/post read and deletion guard stays intact.
 rows=dict(rows);run=rows['controller/run.py'].decode();cleanup=rows['cleanup/cleanup.py'].decode()
 for name,key,prefix in [('PROTECTED_INSTANCE','instanceId',''),('PROTECTED_IMAGE','digest','sha256:')]:
  run=replace_once(run,name+'='+repr(prefix+PROTECTED_HOME[key]),name+'='+repr(prefix+expected[key]))
 old=tuple(PROTECTED_HOME[k]if k!='digest'else 'sha256:'+PROTECTED_HOME[k]for k in ['id','instanceId','digest'])
 new=tuple(expected[k]if k!='digest'else 'sha256:'+expected[k]for k in ['id','instanceId','digest'])
 cleanup=replace_once(cleanup,'PROTECTED = '+repr(old),'PROTECTED = '+repr(new))
 rows['controller/run.py']=run.encode();rows['cleanup/cleanup.py']=cleanup.encode();return rows
def operator_source():
 path=Path(__file__).resolve()
 return {'path':str(path),'sha256':hashlib.sha256(h.read_raw(path)).hexdigest()}
def sync_dir(p):
 fd=os.open(p,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW)
 try:os.fsync(fd)
 finally:os.close(fd)
def write_new(p,raw):
 fd=os.open(p,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
 try:
  os.fchmod(fd,0o600)
  with os.fdopen(fd,'wb',closefd=False)as f:f.write(raw);f.flush();os.fsync(fd)
 finally:os.close(fd)
 sync_dir(p.parent)
def private_dir(p):
 s=p.lstat();h.need(stat.S_ISDIR(s.st_mode)and not stat.S_ISLNK(s.st_mode)and stat.S_IMODE(s.st_mode)==0o700 and s.st_uid==os.geteuid())
def same_capture(worker,controller):
 capture={k:worker[k]for k in ['sourceRevision','sourceArchiveSha256','generatedJsSha256']}
 h.need(all(controller.get(k)==v for k,v in capture.items()));h.need(re.fullmatch('[a-f0-9]{40}',capture['sourceRevision'])is not None)
 for k in ['sourceArchiveSha256','generatedJsSha256']:h.sha(capture[k])
 h.need(h.digest(worker['generatedJs'])==capture['generatedJsSha256']);return capture

def validate_inputs(config):
 h.exact(config,'formatVersion workerBuildReceipt controllerBuildReceipt volumeReceipt pgEvidence s3Evidence positiveIngress controlAppId credentialStorePath'.split()+[k for k in ['positiveProfileSnapshot','protectedHome']if k in config]);h.need(type(config['formatVersion'])is int and config['formatVersion']==1);h.validate_positive(config['positiveIngress']);expected_protected=protected_home(config)
 refs={k:config[k]for k in ['workerBuildReceipt','controllerBuildReceipt','volumeReceipt','pgEvidence','s3Evidence']+(['positiveProfileSnapshot']if 'positiveProfileSnapshot'in config else [])};values={k:h.read_ref(v)for k,v in refs.items()}
 worker,controller=values['workerBuildReceipt'],values['controllerBuildReceipt'];capture=same_capture(worker,controller)
 c=dict(controller,receiptSha256=refs['controllerBuildReceipt']['sha256']);h.validate_control_receipt(h.read_raw(refs['controllerBuildReceipt']['path']),c)
 image=h.image(worker);h.need(worker.get('sdkQualified')is False and worker.get('runtimeQualified')is False and worker.get('sourceQualified')is False)
 commands=worker.get('commands');h.need(type(commands)is list and commands)
 for command in commands:h.need(command.get('actualWait')is True and type(command.get('exitCode'))is int and command['exitCode']==0 and command.get('pidfdHeld')is True and command.get('pipeEofClaimed')is False)
 remote=[v for v in commands if v.get('stage')=='remote-build'];h.need(len(remote)==1);log=remote[0]['fileBackedLog'];raw=h.read_raw(log['path'],64*1024*1024);h.need(len(raw)==log['bytes']and hashlib.sha256(raw).hexdigest()==log['sha256'])
 # Reuse the actual candidate parser, including the one completed immutable push
 # and SAME-build managed/C1 marker. Importing this library has no main effects.
 builder=HERE/'build-candidate.py';s=importlib.util.spec_from_file_location('staging_candidate',builder);m=importlib.util.module_from_spec(s);s.loader.exec_module(m)
 import io
 actual_digest,product=m.parse_build_lines(io.BytesIO(raw),worker['image']);h.need(actual_digest==worker['digest']and product==worker['productData'])
 source=Path(worker['sourceDirectory']);h.need(source.is_absolute()and source.resolve()==source);archive=Path(worker['sourceArchive']);h.need(hashlib.sha256(h.read_raw(archive,512*1024*1024)).hexdigest()==capture['sourceArchiveSha256'])
 # The same reviewed packager validates the entire generated JS closure and
 # fixed source members against this capture, without preparing a context.
 s=importlib.util.spec_from_file_location('staging_packager',HERE/'prepare-context.py');p=importlib.util.module_from_spec(s);s.loader.exec_module(p)
 p.validate_capture(source,source/'examples/managed-runtime-image-staging',worker,archive)
 volume=values['volumeReceipt'];h.exact(volume,'actual_guest_device_bytes cleanup_verified cold_restart_durability_qualified component_passed filesystem_write_fsync_rename_readback instance_id isolated_only kind machine_id probe_sha256 provider_encryption_acknowledged provider_nominal_gib raw_report_path raw_report_sha256 root_sdk_source_authority verified_at volume_id'.split())
 h.need(volume['kind']=='operator-reviewed-fresh-volume-component-v1'and all(volume[k]is True for k in ['cleanup_verified','component_passed','filesystem_write_fsync_rename_readback','isolated_only','provider_encryption_acknowledged'])and volume['root_sdk_source_authority']is False and volume['cold_restart_durability_qualified']is False)
 report=h.read_ref({'path':volume['raw_report_path'],'sha256':volume['raw_report_sha256']});binding=report.get('artifactBinding');h.need(type(binding)is dict)
 h.need(binding.get('workerImage')==image and binding.get('sourceRevision')==capture['sourceRevision']and binding.get('sourceArchiveSha256')==capture['sourceArchiveSha256']and binding.get('buildReceiptPath')==refs['workerBuildReceipt']['path']and binding.get('buildReceiptSha256')==refs['workerBuildReceipt']['sha256'])
 h.need(report.get('componentPassed')is True and report.get('cleanupVerified')is True and report.get('protectedHomeUnchanged')is True and report.get('actualCliChildrenReaped')is True and report.get('allOriginalBothEOF')is True)
 h.need(report.get('protected')==expected_protected)
 h.need(report.get('machineId')==volume['machine_id']and report.get('instanceId')==volume['instance_id']and report.get('volumeId')==volume['volume_id'])
 h.need(values['pgEvidence'].get('ok')is True and values['pgEvidence'].get('cleanupVerified')is True and values['s3Evidence'].get('actualCasPassed')is True)
 return refs,worker,c,capture

def render(baseline,config,output):
 refs,worker,c,capture=validate_inputs(config)
 originals={n:h.read_raw(baseline/n)for n in BASELINE_FILES}
 for n,raw in originals.items():h.need(hashlib.sha256(raw).hexdigest()==BASELINE_SHA[n])
 rows={n:raw for n,raw in originals.items()if n!='controller/genuine_contract.py'}
 rows=rewrite_protected(rows,protected_home(config))
 for n in ['controller/run.py','supervision/run.py','cleanup/cleanup.py']:rows[n]=replace_once(rows[n].decode(),'LIFECYCLE_BINDINGS_FINALIZED = False','LIFECYCLE_BINDINGS_FINALIZED = True').encode()
 run=rows['controller/run.py'].decode();run=replace_once(run,"WORKER=BINDING['worker_artifact']['artifactBinding']['workerImage']","WORKER=BINDING['worker_image']")
 run=replace_once(run,"self.genuine=validate_genuine(o.get('genuine_ingress'),WORKER)if 'genuine_ingress'in o else None","self.genuine=validate_genuine(o.get('positive_ingress'),WORKER)")
 run=replace_once(run,"BINDING['genuine_ingress']","BINDING['positive_ingress']")
 rows['controller/run.py']=run.encode();rows['controller/genuine_contract.py']=h.read_raw(HERE/'host-contract.py')
 # Keep the original boot and completion predicates byte/AST identical.
 old={n.name:ast.dump(n,include_attributes=False)for n in ast.parse(originals['controller/genuine_contract.py']).body if isinstance(n,ast.FunctionDef)}
 new={n.name:ast.dump(n,include_attributes=False)for n in ast.parse(rows['controller/genuine_contract.py']).body if isinstance(n,ast.FunctionDef)}
 for name in ['validate_boot','validate_binder_completion']:h.need(old[name]==new[name])
 binder=rows['producer/bind.mjs'].decode();binder=replace_once(binder,'LIFECYCLE_BINDINGS_FINALIZED = false','LIFECYCLE_BINDINGS_FINALIZED = true');binder=binder.replace('genuine_ingress','positive_ingress');binder=replace_once(binder,"'fly_token','positive_ingress']);","'fly_token','positive_ingress',...(binding.inputs.positiveProfileSnapshot?['positive_profile_snapshot']:[])]);");rows['producer/bind.mjs']=binder.encode()
 rows['producer/genuine-contract.mjs']=h.read_raw(HERE/'host-contract.mjs');rows['producer/prepare.mjs']=replace_once(h.read_raw(HERE/'prepare-operator.mjs').decode(),"from './host-contract.mjs'","from './genuine-contract.mjs'").encode()
 b={'formatVersion':1,'capture':capture,'controller':c,'worker_image':h.image(worker),'positive_ingress':config['positiveIngress'],'control_app_id':config['controlAppId'],'controller_source_sha256':hashlib.sha256(rows['controller/run.py']).hexdigest(),'binder_source_sha256':hashlib.sha256(rows['producer/bind.mjs']).hexdigest(),'inputs':refs,
 'bundle_directory':str(output/'bundle'),'fresh_volume_receipt_path':refs['volumeReceipt']['path'],'controller_source_path':str(output/'controller/run.py'),'control_output_path':str(output/'control-run'),'control_build_receipt_path':refs['controllerBuildReceipt']['path'],'cleanup_output_path':str(output/'control-run/operator-cleanup-v1'),'credential_store_path':config['credentialStorePath'],'binder_completion_path':str(output/'gates/completion.json'),'boot_binding_path':str(output/'gates/boot.json'),'source_directory':worker['sourceDirectory']}
 h.validate_binding(b);rows['lifecycle-bindings.json']=encode(b)
 # Every output/source locator remains distinct, including ancestor collisions
 # with future gate/receipt paths. Only the intentional cleanup child is nested.
 future=[b[k]for k in ['bundle_directory','control_output_path','boot_binding_path','binder_completion_path']]
 h.need(len(set(future))==len(future)and all(not os.path.lexists(p)for p in future))
 for p in future:
  for q in future:
   if p!=q:h.need(Path(p)not in Path(q).parents)
  for ref in refs.values():h.need(p!=ref['path']and Path(p)not in Path(ref['path']).parents)
 h.need(not os.path.lexists(Path(b['credential_store_path']))or Path(b['credential_store_path']).is_file())
 return rows,b,originals

def derive(args):
 output=args.output;h.need(output.is_absolute()and not os.path.lexists(output));private_dir(output.parent)
 config=h.loads(h.read_raw(args.config));rows,b,originals=render(args.baseline,config,output)
 operator=operator_source();expected_protected=protected_home(config)
 intent=output.with_name(output.name+'.intent.json');write_new(intent,encode({'scope':'derive full original staging lifecycle DATA; no provider action','configSha256':hashlib.sha256(h.read_raw(args.config)).hexdigest(),'protectedHome':expected_protected,'operatorSource':operator,'imageCaptureRevision':b['capture']['sourceRevision'],'files':{k:hashlib.sha256(v).hexdigest()for k,v in rows.items()}}))
 try:
  output.mkdir(mode=0o700);sync_dir(output.parent)
  for directory in ['controller','supervision','cleanup','producer','gates']:(output/directory).mkdir(mode=0o700);sync_dir(output)
  for name,raw in rows.items():write_new(output/name,raw)
  write_new(output/'derived.json',encode({'formatVersion':1,'baseline':str(args.baseline),'originals':{n:hashlib.sha256(raw).hexdigest()for n,raw in originals.items()},'derived':{n:hashlib.sha256(raw).hexdigest()for n,raw in rows.items()},'protectedHome':expected_protected,'operatorSource':operator,'imageCaptureRevision':b['capture']['sourceRevision'],'scope':'operator adapter source DATA over the declared image capture; external original tool observation required','sourceQualified':False,'sdkQualified':False,'runtimeQualified':False}))
 except BaseException:
  # A partial output consumes the paths. Never erase it or retry publication.
  raise
 print('FULL_STAGING_LIFECYCLE_DERIVED_NO_PROVIDER_ACTION')

def gate(args):
 run=args.run;b=h.load_binding(run/'lifecycle-bindings.json');private_dir(run/'gates');identity_raw=h.read_raw(run/'supervision/original.json');identity=h.loads(identity_raw)
 h.need(args.ack_original_events and type(args.original_session)is int and args.original_session>0)
 identity_sha=hashlib.sha256(identity_raw).hexdigest();h.need(identity_sha==args.identity_sha256 and identity['controllerSha256']==b['controller_source_sha256'])
 if args.mode=='boot':
  value=h.validate_boot({'bundle_directory':b['bundle_directory'],'original_boot_nonce':args.nonce,'root_observed_original_ready_identity_sha256':identity_sha},b);target=Path(b['boot_binding_path'])
 else:
  h.need(type(args.original_exit_code)is int and args.original_exit_code==0)
  boot=h.validate_boot(h.loads(h.read_raw(b['boot_binding_path'])),b);inputs={k:h.read_raw(Path(b['bundle_directory'])/(k+'.json'))for k in ['operator','services','admission']};bound=h.read_raw(Path(b['bundle_directory'])/'bound.json')
  value={'format_version':1,'kind':'ROOT_OBSERVED_ORIGINAL_BINDER_TERMINAL_DATA_V1','original_tool_session':str(args.original_session),'actual_exit_code':0,'actual_wait':True,'binder_source_sha256':b['binder_source_sha256'],'boot_nonce':boot['original_boot_nonce'],'bound_sha256':hashlib.sha256(bound).hexdigest(),'operator_sha256':hashlib.sha256(inputs['operator']).hexdigest(),'services_sha256':hashlib.sha256(inputs['services']).hexdigest(),'admission_sha256':hashlib.sha256(inputs['admission']).hexdigest(),'original_ready_identity_sha256':identity_sha,'qualified':False,'custody_granted':False,'clock_created':False}
  h.validate_binder_completion(encode(value),bound,inputs,b,boot['original_boot_nonce'],boot);target=Path(b['binder_completion_path'])
 write_new(target,encode(value));print('ORIGINAL_EVENT_GATE_DATA_PUBLISHED')

def main():
 if not __debug__:raise SystemExit('assertions must remain enabled')
 parser=argparse.ArgumentParser(description=__doc__);sub=parser.add_subparsers(dest='mode',required=True)
 p=sub.add_parser('derive');p.add_argument('--baseline',type=Path,required=True);p.add_argument('--config',type=Path,required=True);p.add_argument('--output',type=Path,required=True)
 for mode in ['boot','completion']:
  p=sub.add_parser(mode);p.add_argument('--run',type=Path,required=True);p.add_argument('--original-session',type=int,required=True);p.add_argument('--identity-sha256',required=True);p.add_argument('--ack-original-events',action='store_true')
  if mode=='boot':p.add_argument('--nonce',required=True)
  else:p.add_argument('--original-exit-code',type=int,required=True)
 args=parser.parse_args();derive(args)if args.mode=='derive'else gate(args)
if __name__=='__main__':main()
