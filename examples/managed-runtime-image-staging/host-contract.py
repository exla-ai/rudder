"""Same-capture staging DATA validation; no provider, role or custody issuer."""
import hashlib,json,os,re,stat
from pathlib import Path

class Refusal(ValueError):pass
def need(value):
 if not value:raise Refusal('STAGING_HOST_DATA_REFUSED')
def exact(v,keys):need(type(v)is dict and set(v)==set(keys))
def sha(v):need(type(v)is str and re.fullmatch('[a-f0-9]{64}',v)is not None)
def uuid(v):need(type(v)is str and re.fullmatch('[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}',v)is not None)
def canonical(v):return json.dumps(v,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()
def digest(v):return hashlib.sha256(canonical(v)+b'\n').hexdigest()
def unique(rows):
 v={}
 for k,x in rows:need(k not in v);v[k]=x
 return v
def loads(raw):
 need(type(raw)in[str,bytes] and 0<len(raw)<=16*1024*1024)
 return json.loads(raw,object_pairs_hook=unique,parse_constant=lambda _:need(False))
def stamp(s):return s.st_dev,s.st_ino,s.st_uid,s.st_gid,s.st_mode,s.st_nlink,s.st_size,s.st_mtime_ns,s.st_ctime_ns
def read_raw(path,maximum=16*1024*1024):
 p=Path(path);need(p.is_absolute()and p.resolve()==p);a=p.lstat();need(stat.S_ISREG(a.st_mode)and a.st_nlink==1 and not a.st_mode&0o022 and a.st_uid in [0,os.geteuid()])
 fd=os.open(p,os.O_RDONLY|os.O_NOFOLLOW)
 try:
  need(stamp(a)==stamp(os.fstat(fd)));raw=b''
  while len(raw)<=maximum:
   b=os.read(fd,min(1048576,maximum+1-len(raw)))
   if not b:break
   raw+=b
  need(len(raw)<=maximum and stamp(a)==stamp(os.fstat(fd))==stamp(p.lstat()));return raw
 finally:os.close(fd)
def read_ref(ref):
 exact(ref,['path','sha256']);sha(ref['sha256']);raw=read_raw(ref['path']);need(hashlib.sha256(raw).hexdigest()==ref['sha256']);return loads(raw)
def immutable(v):need(type(v)is str and re.fullmatch('registry.fly.io/rudder-workers-exla@sha256:[a-f0-9]{64}',v)is not None)
def image(v):
 need(type(v.get('image'))is str and re.fullmatch('registry.fly.io/rudder-workers-exla:[A-Za-z0-9_.-]+',v['image'])is not None);need(type(v.get('digest'))is str and re.fullmatch('sha256:[a-f0-9]{64}',v['digest'])is not None)
 return v['image'].split(':')[0]+'@'+v['digest']
def validate_positive(v):
 need(type(v)is dict);need(set(v)==set('openCommandKey startCommandKey model profileId text'.split())or set(v)==set('openCommandKey startCommandKey stopCommandKey model profileId text'.split()))
 keys=[v[k]for k in ['openCommandKey','startCommandKey','stopCommandKey']if k in v]
 need(all(type(k)is str and re.fullmatch('[A-Za-z0-9][A-Za-z0-9._:-]{0,127}',k)is not None for k in keys)and len(keys)==len(set(keys)))
 for k in ['model','profileId']:need(type(v[k])is str and v[k].strip()and len(v[k].encode())<=256 and not any(ord(c)<32 or ord(c)==127 for c in v[k]))
 need(type(v['text'])is str and v['text'].strip()and len(v['text'].encode())<=24*1024 and '\0'not in v['text']);return v
CONTROLLER_KEYS='formatVersion kind sourceRevision sourceArchiveSha256 generatedJsSha256 contextManifestSha256 image digest originalBuild'.split()
def validate_control_receipt(raw,c):
 v=loads(raw);exact(v,CONTROLLER_KEYS);need(v['formatVersion']==1 and type(v['formatVersion'])is int and v['kind']=='SAME_CAPTURE_CONTROLLER_BUILD_DATA')
 need(type(v['sourceRevision'])is str and re.fullmatch('[a-f0-9]{40}',v['sourceRevision'])is not None)
 for k in ['sourceArchiveSha256','generatedJsSha256','contextManifestSha256']:sha(v[k])
 image(v);o=v['originalBuild'];exact(o,'actualWait exitCode pidfdHeld pipeEofClaimed logSha256'.split());need(o['actualWait']is True and type(o['exitCode'])is int and o['exitCode']==0 and o['pidfdHeld']is True and o['pipeEofClaimed']is False);sha(o['logSha256'])
 need({k:x for k,x in c.items()if k!='receiptSha256'}==v and hashlib.sha256(raw).hexdigest()==c['receiptSha256']);return v
PATH_KEYS='bundle_directory fresh_volume_receipt_path controller_source_path control_output_path control_build_receipt_path cleanup_output_path credential_store_path binder_completion_path boot_binding_path source_directory'.split()
BINDING_KEYS=['formatVersion','capture','controller','worker_image','positive_ingress','control_app_id','controller_source_sha256','binder_source_sha256','inputs']+PATH_KEYS
def validate_binding(b):
 exact(b,BINDING_KEYS);need(type(b['formatVersion'])is int and b['formatVersion']==1);exact(b['capture'],'sourceRevision sourceArchiveSha256 generatedJsSha256'.split());need(re.fullmatch('[a-f0-9]{40}',b['capture']['sourceRevision'])is not None)
 for k in ['sourceArchiveSha256','generatedJsSha256']:sha(b['capture'][k])
 exact(b['controller'],CONTROLLER_KEYS+['receiptSha256']);need(all(b['controller'].get(k)==v for k,v in b['capture'].items()));sha(b['controller']['receiptSha256'])
 immutable(b['worker_image']);validate_positive(b['positive_ingress']);sha(b['controller_source_sha256']);sha(b['binder_source_sha256'])
 for k in PATH_KEYS:need(type(b[k])is str and Path(b[k]).is_absolute()and str(Path(b[k]))==b[k]and '..'not in Path(b[k]).parts)
 need(len({b[k]for k in PATH_KEYS})==len(PATH_KEYS));need(type(b['control_app_id'])is str and re.fullmatch('[A-Za-z0-9_-]{1,128}',b['control_app_id'])is not None)
 exact(b['inputs'],'workerBuildReceipt controllerBuildReceipt volumeReceipt pgEvidence s3Evidence'.split()+(['positiveProfileSnapshot']if 'positiveProfileSnapshot'in b['inputs']else []))
 for ref in b['inputs'].values():exact(ref,['path','sha256']);sha(ref['sha256'])
 need(b['fresh_volume_receipt_path']==b['inputs']['volumeReceipt']['path']and b['control_build_receipt_path']==b['inputs']['controllerBuildReceipt']['path']);return b
def load_binding(path):return validate_binding(loads(read_raw(path)))
def validate_genuine(v,image):return validate_positive(v) # Derived controller calls this only for positive selection.
def validate_boot(v,b):
 exact(v,['bundle_directory','original_boot_nonce','root_observed_original_ready_identity_sha256']);need(v['bundle_directory']==b['bundle_directory']);uuid(v['original_boot_nonce']);sha(v['root_observed_original_ready_identity_sha256']);return v
def validate_binder_completion(raw,bound_raw,inputs,b,nonce,boot):
 v=loads(raw);exact(v,'format_version kind original_tool_session actual_exit_code actual_wait binder_source_sha256 boot_nonce bound_sha256 operator_sha256 services_sha256 admission_sha256 original_ready_identity_sha256 qualified custody_granted clock_created'.split());need(type(v['format_version'])is int and v['format_version']==1 and v['kind']=='ROOT_OBSERVED_ORIGINAL_BINDER_TERMINAL_DATA_V1'and type(v['original_tool_session'])is str and re.fullmatch('[0-9]{1,20}',v['original_tool_session'])is not None and type(v['actual_exit_code'])is int and v['actual_exit_code']==0 and v['actual_wait']is True)
 need(v['binder_source_sha256']==b['binder_source_sha256']and v['boot_nonce']==nonce==boot['original_boot_nonce']and v['original_ready_identity_sha256']==boot['root_observed_original_ready_identity_sha256'])
 need(v['bound_sha256']==hashlib.sha256(bound_raw).hexdigest())
 for key in ['operator','services','admission']:need(v[key+'_sha256']==hashlib.sha256(inputs[key]).hexdigest())
 for key in ['qualified','custody_granted','clock_created']:need(v[key]is False)
 return v

def completion(flow,positive,ordinary=None):
 """Observe the actual original flow; this DATA check creates no capability."""
 if type(flow)is not dict:return False
 stop='stopCommandKey'in positive
 if flow.get('stage')!=('ordinary-positive-lifecycle-observed-data'if stop else'ordinary-positive-backend-ready-data'):return False
 need(flow.get('failed')is False and flow.get('owners_retained')is True)
 for role,state in [('sdk','sdk_role_commit_observed'),('runtime','role_commit_observed')]:
  v=flow.get(role);need(type(v)is dict and v.get('state')==state and v.get('expired')is False and v.get('error')is None and v.get('owners_retained')is True and v.get('ack_send_unknown')is False and v.get('ack_send_attempted')is True and v.get('publication_ack_data')is not None)
 activation=flow.get('activation');need(type(activation)is dict and activation.get('scope')=='original-staging-admin-transition-data-only');attempts=activation.get('attempts');need(type(attempts)is list and len(attempts)==2 and {a.get('role')for a in attempts}=={'sdk','runtime'}and all(a.get('known_commit')is True and a.get('unknown')is False and a.get('permit_retained')is True for a in attempts))
 v=flow.get('ordinary');need(type(v)is dict and v.get('scope')=='ordinary-source-sdk-staging-data-only'and v.get('expired')is False and v.get('error')is False and v.get('owners_retained')is True)
 receipt=v.get('receipt');need(type(receipt)is dict and type(receipt.get('projects'))is list and len(receipt['projects'])==1 and receipt.get('homeId'))
 need(type(ordinary)is dict and ordinary.get('captured_from_same_original_supervisor')is True and ordinary.get('teardown_authority')is False and ordinary.get('receipt')==receipt)
 custody=ordinary.get('custody');need(type(custody)is dict and custody==v.get('custody')and custody.get('owners_retained')is True and custody.get('teardown_authority')is False and custody.get('pending_effect')is None and custody.get('home_id')==receipt['homeId']and type(custody.get('started'))is dict)
 p=v.get('positive_ingress');need(type(p)is dict and p.get('scope')=='positive-ingress-observation-only'and p.get('owners_retained')is True and p.get('refused')is False and p.get('backend_ready_last_observed')is True and p.get('qualified')is False and p.get('retirement_complete')is False)
 fence=p.get('fence');exact(fence,['run_id','run_epoch','attempt_id']);need(all(type(x)is str and x for x in fence.values()))
 commands=p.get('commands');keys=[positive['openCommandKey'],positive['startCommandKey']]+([positive['stopCommandKey']]if stop else[]);need(type(commands)is list and len(commands)==len(keys))
 for c,key in zip(commands,keys):need(type(c)is dict and c.get('command_key')==key and c.get('dispatch_unknown')is False and type(c.get('receipt_queries'))is int and c['receipt_queries']>0)
 if stop:
  for k in ['lifecycle_complete','enrollment_release_proved','original_helper_join_proved','checkpoint_accepted']:need(p.get(k)is True)
  observations=p.get('lifecycle_observation');need(type(observations)is dict and all(observations.get(k)is True for k in ['enrollment_submitted_observed','enrollment_released_observed','helper_joined_observed','stop_requested_observed','validated_checkpoint_observed']))
 return True
