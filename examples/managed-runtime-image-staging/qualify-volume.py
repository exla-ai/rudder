#!/usr/bin/env python3
"""One original-owned Fly volume/VM filesystem component; no managed grant.
Only original create responses can supply cleanup IDs. No resume/adoption path.
"""
from pathlib import Path
import argparse, datetime, hashlib, io, json, os, re, shlex, signal, stat, tarfile, time, uuid
from owned_child import capture, clock, HELD

APP = 'rudder-v4-qualification'
PROTECTED_APP = 'rudder-workers-exla'
PROTECTED_ID = '8edd225a7d0378'
PROTECTED_INSTANCE = '01M670MG4A8E0DWE9GA1C0PQ92'
PROTECTED_DIGEST = '404f46396aca77324d9b53c55d2824a5401af56ba5362d0bdae67e16108fc245'
FLY = None
IMAGE_DIGEST = None
IMAGE = None
BINDING = None
BUILD_RECEIPT = None
FLY_SHA256 = None
HELD_COMPONENTS=[]
ACTIVE=None
NODE = '/usr/local/bin/node'
REGION = 'sjc'
MOUNT = '/qualification-volume'
ROLE = 'fresh-volume-filesystem-component-v1'
MAX_CALLS, CLEANUP_RESERVE, WORK_SECONDS, CLEANUP_SECONDS = 64, 16, 180, 120
MAX_OUTPUT = 1048576


def digest(value):
    return hashlib.sha256(value).hexdigest()

def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()

def now():
    return time.clock_gettime(time.CLOCK_BOOTTIME)

def utc():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()

def need(condition, reason):
    if not condition:
        raise RuntimeError(reason)

def hold_original():
 # Never throw/log/return while original custody or post-effect observation is unknown.
 import signal
 while True:
  try:signal.pause()
  except BaseException:pass

def retain_unknown(row):
 if not custody(row):hold_original()

def capture_original(capture_fn,*args,**kwargs):
 try:
  row=capture_fn(*args,**kwargs);retain_unknown(row);return row
 except BaseException:hold_original()
def custody(row):
 return type(row)is dict and type(row.get('exitCode'))is int and all(row.get(k)is True for k in ('constructorCompleted','actualChildWait','stdoutEOF','stderrEOF','pidfdHeld'))and row.get('observerError')is None

def durable_new(path, value):
    need(ACTIVE is not None, 'original-component-owner-required')
    return ACTIVE.persist(path, canonical(value) + b'\n')

def dirstamp(s):return(s.st_dev,s.st_ino,s.st_mode,s.st_uid,s.st_gid,s.st_nlink)
def stamp(s):return dirstamp(s)+(s.st_size,s.st_mtime_ns,s.st_ctime_ns)
def strict(raw):
    def pairs(rows):
        out={}
        for key,value in rows:
            need(key not in out,'duplicate-input-key');out[key]=value
        return out
    return json.loads(raw,object_pairs_hook=pairs,parse_constant=lambda _:need(False,'nonfinite-input'))

def validate_build(receipt,raw,receipt_path,read,builder,builder_raw,fly_sha256):
    """Validate retained build DATA. Root must observe the original invocation;
    these hashes cannot authenticate an actual tool event or registry by themselves.
    No provider effects or new child are used in this preflight.
    """
    need(type(receipt)is dict and type(receipt.get('formatVersion'))is int and receipt['formatVersion']==1,'candidate-build-format')
    for key in ('deployed','sdkQualified','runtimeQualified','sourceQualified'):
        need(receipt.get(key)is False,'build-is-not-runtime-authority')
    sha_re=r'[a-f0-9]{64}'
    revision=receipt.get('sourceRevision')
    need(type(revision)is str and re.fullmatch(r'[a-f0-9]{40}',revision),'committed-source-revision-required')
    image=receipt.get('image');image_digest=receipt.get('digest')
    need(type(image)is str and re.fullmatch(r'registry\.fly\.io/rudder-workers-exla:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}',image),'actual-worker-build-required')
    need(type(image_digest)is str and re.fullmatch(r'sha256:'+sha_re,image_digest),'actual-immutable-push-required')
    parent=receipt_path.parent
    need(receipt_path.name=='build-receipt.json' and receipt.get('sourceArchive')==str(parent/'source.tar')
         and receipt.get('sourceDirectory')==str(parent/'source'),'exact-candidate-source-paths')
    archive=read(parent/'source.tar',128*1024*1024)
    need(digest(archive)==receipt.get('sourceArchiveSha256'),'exact-captured-source-archive')
    need(receipt.get('builderSourceSha256')==digest(builder_raw),'original-builder-source-bytes')
    with tarfile.open(fileobj=io.BytesIO(archive),mode='r:')as tar:
        need(tar.pax_headers.get('comment')==revision,'archive-committed-revision-mismatch')
        seen=set();members={}
        for member in tar:
            name=member.name.rstrip('/')
            need(name and not name.startswith('/') and all(p not in ('','.','..')for p in name.split('/'))
                 and name not in seen and (member.isfile()or member.isdir()),'closed-source-archive')
            seen.add(name);members[name]=member
        def archived(name):
            need(name in members and members[name].isfile()and members[name].size<=262144,'required-captured-source-file')
            return tar.extractfile(members[name]).read()
        need(archived('examples/managed-runtime-image-staging/build-candidate.py')==builder_raw,'same-revision-builder-source')
        docker=builder['render_dockerfile'](archived('cloud/worker/Dockerfile').decode(),revision).encode()
        need(digest(docker)==receipt.get('dockerfileSha256'),'same-revision-managed-c1-recipe')
        policy=strict(archived('cloud/worker/managed-runtime/policy.json'))
    need(receipt.get('workerNodeImage')==policy.get('node_base_image')and type(receipt.get('workerNodeImage'))is str
         and re.fullmatch(r'node:[A-Za-z0-9_.-]+@sha256:'+sha_re,receipt['workerNodeImage']),'pinned-current-node-image')
    need(receipt.get('compilerC1Image')==builder['C1_IMAGE']and receipt.get('compilerC1Pins')==builder['C1_PINS'],'pinned-c1-overlay-required')
    js=receipt.get('generatedJs')
    need(type(js)is dict and js and digest(canonical(js)+b'\n')==receipt.get('generatedJsSha256'),'closed-generated-js-map')
    for path,row in js.items():
        need(type(path)is str and path.endswith('.js')and (path.startswith('dist/')or path.startswith('cloud/dist/'))
             and all(p not in ('','.','..')for p in path.split('/'))and type(row)is dict and set(row)=={'sha256','bytes'}
             and type(row['sha256'])is str and re.fullmatch(sha_re,row['sha256'])and type(row['bytes'])is int and row['bytes']>0,'generated-js-row')
    fly=receipt.get('flyExecutable')
    need(type(fly)is dict and set(fly)=={'path','sha256'}and type(fly['path'])is str and Path(fly['path']).is_absolute()
         and type(fly_sha256)is str and re.fullmatch(sha_re,fly_sha256)and fly['sha256']==fly_sha256,'explicit-original-fly-pin')
    need(digest(read(fly['path'],128*1024*1024))==fly_sha256,'held-original-Fly-bytes')
    commands=receipt.get('commands')
    stages=['archive','npm-root','typescript-cli','board','chmod-cli','npm-cloud','typescript-cloud','remote-build']
    need(type(commands)is list and len(commands)==len(stages),'all-original-build-stages-required')
    for stage,row in zip(stages,commands):
        need(type(row)is dict and row.get('stage')==stage and type(row.get('exitCode'))is int and row['exitCode']==0
             and row.get('actualWait')is True and row.get('pidfdHeld')is True and row.get('pipeEofClaimed')is False
             and type(row.get('pid'))is int and row['pid']>0,'actual-original-build-wait-required')
        need(strict(read(parent/(stage+'.json'),4194304))==row,'exact-original-build-stage-record')
    remote=commands[-1];source=parent/'source'
    build_id=receipt.get('buildId')
    need(type(build_id)is str and re.fullmatch(sha_re,build_id),'original-build-id-required')
    intent=read(parent.with_name(parent.name+'.intent.json'),4194304)
    need(digest(intent)==build_id,'original-durable-build-intent')
    intent_data=strict(intent)
    service=receipt.get('buildService','depot')
    need(service in ('depot','fly')and intent_data.get('buildService','depot')==service,
         'same-original-build-service')
    need(intent_data.get('sourceRevision')==revision and intent_data.get('output')==str(parent)
         and intent_data.get('image')==image and intent_data.get('buildOnly')is True
         and intent_data.get('compilerC1Image')==builder['C1_IMAGE'],'same-original-build-intent')
    label=image.split(':',1)[1]
    expected=[fly['path'],'deploy','.','--config',str(source/'candidate.fly.toml'),'--app','rudder-workers-exla',
              '--dockerfile',str(source/'candidate.Dockerfile'),'--build-target','managed_c1',
              '--build-arg','WORKER_NODE_IMAGE='+receipt['workerNodeImage'],
              '--build-arg','RUDDER_CANDIDATE_BUILD_ID='+build_id,
              '--build-arg','RUDDER_IMAGE_PRESEED_PROFILE=baked-cas-v1',
              '--build-arg','RUDDER_IMAGE_QUALIFICATION_NONCE='+revision,
              *(['--depot=false']if service=='fly'else []),
              '--build-only','--push','--remote-only','--image-label',label]
    need(remote.get('argv')==expected,'exact-original-build-only-command')
    logrow=remote.get('fileBackedLog')
    need(type(logrow)is dict and logrow.get('path')==str(parent/'remote-build.log'),'original-build-log-path')
    log=read(parent/'remote-build.log',64*1024*1024)
    need(type(logrow.get('bytes'))is int and len(log)==logrow['bytes']and digest(log)==logrow.get('sha256'),'exact-original-build-log')
    # Parser reads the same retained FD bytes, never reopens a mutable path.
    parsed_digest,product=builder['parse_build_lines'](io.BytesIO(log),image)
    need(parsed_digest==image_digest and product==receipt.get('productData'),'actual-same-build-product-and-push')
    return {'workerImage':image.split(':',1)[0]+'@'+image_digest,'sourceRevision':revision,
            'buildReceiptPath':str(receipt_path),'buildReceiptSha256':digest(raw),
            'sourceArchiveSha256':receipt['sourceArchiveSha256'],'flyctlSha256':fly_sha256,
            'sourceQualified':False,'sdkQualified':False,'runtimeQualified':False}

class ComponentOwner:
    def __init__(self):
        self.handles=[];self.inputs=[];self.effectAttempted=False;self.output=None;self.directory=None;self.directoryStamp=None
        self.remoteUnknown=None;self.calls=[]
        HELD_COMPONENTS.append(self) # retain the original carrier BEFORE allocation/effects
    def hold(self):return hold_original()
    def read(self,path,maximum):
        path=Path(path);need(path.is_absolute()and path.resolve()==path,'exact-original-input-path')
        fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC);self.handles.append(fd);s=os.fstat(fd)
        need(stat.S_ISREG(s.st_mode)and s.st_nlink==1 and s.st_uid==os.geteuid()and s.st_gid==os.getegid()and not s.st_mode&0o022 and 0<s.st_size<=maximum,'bounded-original-input')
        raw=b''
        while len(raw)<s.st_size:
            part=os.pread(fd,min(1048576,s.st_size-len(raw)),len(raw));need(part!=b'','complete-original-input');raw+=part
        need(os.pread(fd,1,s.st_size)==b''and stamp(s)==stamp(os.fstat(fd))==stamp(path.lstat()),'same-original-input')
        self.inputs.append((path,fd,stamp(s)));return raw
    def current_inputs(self):
        for path,fd,s in self.inputs:need(s==stamp(os.fstat(fd))==stamp(path.lstat()),'same-retained-original-input')
    def preflight(self):
        global BINDING, FLY
        raw=self.read(BUILD_RECEIPT,4194304)
        receipt=strict(raw)
        builder_path=Path(__file__).resolve().with_name('build-candidate.py')
        builder_raw=self.read(builder_path,262144)
        builder={'__name__':'candidate_build_validation','__file__':str(builder_path)}
        exec(compile(builder_raw,str(builder_path),'exec'),builder)
        BINDING=validate_build(receipt,raw,Path(BUILD_RECEIPT),self.read,builder,builder_raw,FLY_SHA256)
        FLY=receipt['flyExecutable']['path']
        self.read(Path(__file__).resolve(),262144)
        probe=self.read(Path(__file__).resolve().with_name('probe.mjs'),32768)
        self.current_inputs();return probe
    def start_output(self,output):
        self.effectAttempted=True # uncertain mkdir is retained, never retried
        try:
            output.mkdir(mode=0o700);parent=os.open(output.parent,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW);self.handles.append(parent);os.fsync(parent)
            self.output=output;self.directory=os.open(output,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW);self.handles.append(self.directory)
            s=os.fstat(self.directory);need(stat.S_IMODE(s.st_mode)==0o700 and s.st_uid==os.geteuid()and s.st_gid==os.getegid(),'private-original-output')
            self.directoryStamp=dirstamp(s);self.current_output()
        except BaseException:self.hold()
    def current_output(self):
        need(self.directoryStamp==dirstamp(os.fstat(self.directory))==dirstamp(self.output.lstat()),'same-original-output-directory')
    def persist(self,path,raw):
        try:
            need(type(raw)is bytes and path.parent==self.output and len(raw)<=2*MAX_OUTPUT,'private-original-output-bound');self.current_output()
            fd=os.open(path.name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600,dir_fd=self.directory);self.handles.append(fd);os.fchmod(fd,0o600)
            at=0
            while at<len(raw):
                n=os.write(fd,raw[at:]);need(type(n)is int and 0<n<=len(raw)-at,'complete-original-write');at+=n
            os.fsync(fd);os.fsync(self.directory);s=os.fstat(fd)
            need(stat.S_ISREG(s.st_mode)and s.st_nlink==1 and stat.S_IMODE(s.st_mode)==0o600 and s.st_size==len(raw)and stamp(s)==stamp(path.lstat()),'same-original-persisted-file')
            self.current_output();return fd
        except BaseException:self.hold()
    def call(self,arguments,label,end,timeout):
        # Reserve reviewed carrier's fixed10s tail INSIDE the original call window.
        call_end=min(end,now()+timeout);need(call_end-now()>10,'original-call-window-no-renewal')
        self.current_inputs()
        try:
            row=capture_original(capture,[FLY,*arguments],call_end-10)
            out=row.pop('stdout');err=row.pop('stderr')
            need(type(out)is bytes and type(err)is bytes,'actual-original-stream-bytes')
            self.calls.append(row)
            self.persist(self.output/(label+'.stdout.private'),out);self.persist(self.output/(label+'.stderr.private'),err)
            need(len(out)<=MAX_OUTPUT and len(err)<=MAX_OUTPUT,'provider-output-bound')
            if row['timedOut']is not False or row['truncated']is not False or now()>=call_end:
                self.remoteUnknown=label;return self.hold()
            self.current_inputs();return row,out,err
        except BaseException:return self.hold()
    def unknown_create(self,volume_started,volume_ack,machine_started,machine_ack):
        if (volume_started and not volume_ack)or(machine_started and not machine_ack):
            self.remoteUnknown='original-create-outcome-unknown';return self.hold()
    def finish(self,report):
        try:
            self.current_inputs();self.current_output();durable_new(self.output/'report.json',report)
            if report.get('cleanupVerified')is not True:
                self.remoteUnknown='original-cleanup-not-verified';return self.hold()
            print(json.dumps({key:report.get(key)for key in ['componentPassed','cleanupVerified','protectedHomeUnchanged','volumeId','machineId','instanceId','providerCalls','restartDurabilityQualified']},sort_keys=True),flush=True)
            return 0 if report['componentPassed']and report['cleanupVerified']and report.get('protectedHomeUnchanged')and report['actualCliChildrenReaped']else 1
        except BaseException:return self.hold()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--build-receipt', required=True)
    parser.add_argument('--flyctl-sha256', required=True)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    global ACTIVE, IMAGE, IMAGE_DIGEST, BUILD_RECEIPT, FLY_SHA256
    BUILD_RECEIPT=Path(args.build_receipt)
    FLY_SHA256=args.flyctl_sha256
    os.environ.pop('FLY_ACCESS_TOKEN',None)
    os.umask(0o077)
    ACTIVE=ComponentOwner()
    probe=ACTIVE.preflight()
    IMAGE=BINDING['workerImage'];IMAGE_DIGEST=IMAGE.rsplit(':',1)[1]
    output = Path(args.output)
    need(re.fullmatch(r'/workspace/rudder-fresh-volume-run-[a-z0-9-]+', str(output)) is not None,
         'new-fixed-workspace-output-required')
    need(output.parent.resolve() == Path('/workspace'), 'output-ancestry-changed')
    need(not os.path.lexists(output), 'unused-output-required')
    ACTIVE.start_output(output)  # Existing originals are never resumed.
    controller = Path(__file__).resolve()
    need(0 < len(probe) <= 32768, 'fixed-probe-source-bound')
    nonce = uuid.uuid4().hex
    name, volume_name = 'rudder-volume-' + nonce[:12], 'rudder_qv_' + nonce[:12]
    probe_sha = digest(probe)
    metadata = {'rudder_role': ROLE, 'qualification': nonce, 'probe_sha256': probe_sha}
    intent = {'formatVersion': 1, 'app': APP, 'region': REGION, 'nonce': nonce,
              'name': name, 'volumeName': volume_name, 'image': IMAGE,
              'mountPath': MOUNT, 'probeSha256': probe_sha, 'controllerSha256': digest(controller.read_bytes()),
              'volumeRequest': {'size_gb': 1, 'encrypted': True, 'count': 1,
                                'scheduled_snapshots': False, 'snapshot_id': None},
              'workloadSeconds': WORK_SECONDS, 'cleanupSeconds': CLEANUP_SECONDS,
              'maximumCalls': MAX_CALLS, 'cleanupCallReserve': CLEANUP_RESERVE,
              'protected': {'app': PROTECTED_APP, 'id': PROTECTED_ID,
                            'instanceId': PROTECTED_INSTANCE, 'digest': PROTECTED_DIGEST},
              'coldRestart': False, 'createdAt': utc(), 'artifactBinding': BINDING,
              'capture': 'original-bounded-pipes-actual-wait-and-both-EOF',
              'processExecutableFdHeld': False}
    # ONE synced intent exists before the first provider effect. It never grants
    # ownership when read back; this invocation alone holds original responses.
    durable_new(output / 'intent.json', intent)
    report = {**intent, 'scope': 'fresh encrypted Fly volume filesystem component only',
              'steps': [], 'componentPassed': False, 'cleanupVerified': False,
              'coldRestartPerformed': False, 'restartDurabilityQualified': False,
              'physicalRootQualified': False, 'sourceAuthority': False, 'sdkAuthority': False,
              'haQualified': False, 'fleetDurability': False, 'normalRuntimeRetirement': False,
              'productionEnablement': False, 'models': False}
    start = now()
    work_end = start + WORK_SECONDS
    cleanup_end = None
    calls = 0
    children = ACTIVE.calls  # Exact reviewed capture owns each original Child/pipes/pidfd in HELD.
    volume = machine = instance = protected = original_config_sha = None
    volume_started = machine_started = False
    volume_ack = machine_ack = False
    config = None

    def call(arguments, label, timeout=20, cleanup=False, allow_failure=False):
        nonlocal calls
        need(calls < (MAX_CALLS if cleanup else MAX_CALLS - CLEANUP_RESERVE),
             'provider-call-bound-with-cleanup-reserve')
        end = cleanup_end if cleanup else work_end
        need(end is not None and end - now() > 5, 'original-controller-budget-expired')
        calls += 1
        stem = f'{calls:03d}-{label}'
        row,data,stderr=ACTIVE.call(arguments,stem,end,timeout)
        code=row['exitCode'];timed_out=row['timedOut']
        result={'exitCode':code,'timedOut':timed_out,'stdout':data}
        report['steps'].append({'label':label,'exitCode':code,'timedOut':timed_out,
                               'stdoutSha256':digest(data),'stderrSha256':digest(stderr),'actualCliChildWait':True,
                               'child':row})
        if not allow_failure:
            need(code == 0 and not timed_out, 'provider-observation-failed:' + label)
        return result

    def machines(label, app=APP, cleanup=False):
        rows = json.loads(call(['machine', 'list', '-a', app, '--json'], label, cleanup=cleanup)['stdout'])
        need(isinstance(rows, list), 'machine-inventory-schema')
        return rows

    def volumes(label, cleanup=False):
        rows = json.loads(call(['volumes', 'list', '-a', APP, '--json'], label, cleanup=cleanup)['stdout'])
        need(isinstance(rows, list), 'volume-inventory-schema')
        return rows

    def protected_identity(rows):
        matches = [row for row in rows if row.get('id') == PROTECTED_ID]
        need(len(matches) == 1, 'protected-home-missing-or-ambiguous')
        home = matches[0]
        image = (home.get('image_ref') or {}).get('digest', '').removeprefix('sha256:')
        need(home.get('instance_id') == PROTECTED_INSTANCE and image == PROTECTED_DIGEST,
             'protected-original-home-changed')
        need(home.get('state') == 'started', 'protected-home-not-live')
        return {'id': home['id'], 'instanceId': home['instance_id'], 'digest': image,
                'state': home['state'], 'configSha256': digest(canonical(home.get('config') or {})),
                'volumeIds': sorted(m['volume'] for m in (home.get('config') or {}).get('mounts', []) if 'volume' in m)}

    def assert_volume(row, attached=None, require_contract=True):
        need(volume is not None and row.get('id') == volume.get('id'), 'volume-id-changed')
        need(row.get('name') == volume_name and row.get('region') == REGION, 'volume-original-context-changed')
        for key in ['id', 'name', 'region', 'size_gb', 'encrypted', 'created_at', 'host_id', 'zone']:
            if key in volume:
                need(row.get(key) == volume[key], 'volume-original-binding-changed:' + key)
        need(row.get('attached_machine_id') == attached, 'volume-attachment-unresolved')
        if require_contract:
            need(row.get('size_gb') == 1 and row.get('encrypted') is True, 'one-GiB-encrypted-volume-required')

    def assert_machine(row, allow_first_instance=False):
        nonlocal instance, original_config_sha
        need(machine is not None and row.get('id') == machine['id'] and row.get('id') != PROTECTED_ID,
             'machine-original-id-changed')
        need(row.get('name') == name and row.get('region') == REGION, 'machine-original-context-changed')
        actual = row.get('config') or {}
        need(actual.get('image') == IMAGE and all((actual.get('metadata') or {}).get(k) == v for k, v in metadata.items()),
             'machine-image-or-nonce-changed')
        need(actual.get('init', {}).get('exec') == config['init']['exec'], 'machine-exec-changed')
        mounts = actual.get('mounts') or []
        need(len(mounts) == 1 and not actual.get('services'), 'machine-mount-or-service-changed')
        mount = mounts[0]
        need(set(mount) <= {'volume', 'path', 'encrypted', 'size_gb', 'name'}
             and mount.get('volume') == volume['id'] and mount.get('path') == MOUNT,
             'machine-original-volume-mount-changed')
        for key, expected in [('encrypted', True), ('size_gb', 1), ('name', volume_name)]:
            need(key not in mount or mount[key] == expected, 'machine-provider-mount-metadata-changed:' + key)
        need(not actual.get('files') and not actual.get('processes'), 'machine-extra-inputs-changed')
        need(set((actual.get('env') or {}).keys()) <= {'PRIMARY_REGION', 'FLY_PRIMARY_REGION'}, 'machine-extra-environment')
        need(actual.get('auto_destroy') in [None, False] and actual.get('restart', {}).get('policy') == 'no', 'machine-restart-policy-changed')
        need(all((actual.get('guest') or {}).get(k) == v for k, v in config['guest'].items()), 'machine-guest-size-changed')
        observed = row.get('instance_id')
        need(isinstance(observed, str) and re.fullmatch(r'[A-Za-z0-9]{20,64}', observed) is not None
             and observed != PROTECTED_INSTANCE, 'machine-original-instance-absent')
        if instance is None:
            need(allow_first_instance, 'no-original-instance-carrier')
            instance = observed
            original_config_sha = digest(canonical(actual))
            durable_new(output / 'original-machine-instance.json',
                        {'id': machine['id'], 'instanceId': instance, 'fromOriginalCreateIdReadback': True,
                         'nonce': nonce, 'configSha256': original_config_sha})
        need(observed == instance, 'machine-instance-replaced')
        need(digest(canonical(actual)) == original_config_sha, 'machine-original-config-changed')
        image = (row.get('image_ref') or {}).get('digest', '').removeprefix('sha256:')
        need(image == IMAGE_DIGEST and image != PROTECTED_DIGEST, 'machine-resolved-image-changed')

    def find_original_machine(rows):
        # No name/nonce inventory adoption: an ID must already be retained from
        # this invocation's actual create output, even for cleanup-only use.
        if machine is None:
            return None
        matches = [row for row in rows if row.get('id') == machine['id']]
        need(len(matches) <= 1, 'machine-id-ambiguous')
        return matches[0] if matches else None

    try:
        protected = protected_identity(machines('protected-preflight', PROTECTED_APP))
        report['protectedHomeBefore'] = protected
        initial_machines, initial_volumes = machines('staging-machine-preflight'), volumes('staging-volume-preflight')
        need(not any(m.get('name') == name or (m.get('config') or {}).get('metadata', {}).get('qualification') == nonce
                     for m in initial_machines), 'new-machine-identity-already-exists')
        need(not any(v.get('name') == volume_name for v in initial_volumes), 'new-volume-name-already-exists')
        secrets = json.loads(call(['secrets', 'list', '-a', APP, '--json'], 'staging-secret-preflight')['stdout'])
        need(secrets == [], 'staging-app-has-secrets')

        volume_started = True
        durable_new(output / 'volume-create-started.json', {'nonce': nonce, 'request': intent['volumeRequest'],
                                                          'name': volume_name, 'app': APP, 'region': REGION})
        result = call(['volumes', 'create', volume_name, '-a', APP, '--region', REGION,
                       '--size', '1', '--count', '1', '--no-encryption=false',
                       '--scheduled-snapshots=false', '--json', '--yes'], 'volume-create-once', 45,
                      allow_failure=True)
        try:
            decoded = json.loads(result['stdout'])
            if isinstance(decoded, list):
                need(len(decoded) == 1, 'volume-count-not-one')
                decoded = decoded[0]
            need(isinstance(decoded, dict) and re.fullmatch(r'vol_[a-z0-9]+', str(decoded.get('id'))) is not None,
                 'original-volume-create-id-absent')
            volume = decoded  # Stage original response BEFORE later checks.
            durable_new(output / 'original-volume-response.json', volume)
        except (ValueError, RuntimeError):
            report['volumeCreateUncertain'] = True
            raise
        need(result['exitCode'] == 0 and not result['timedOut'], 'original-volume-create-not-acknowledged')
        volume_ack = True
        need(volume['id'] not in protected['volumeIds'], 'protected-volume-id-refused')
        assert_volume(volume)
        report['providerVolumeEncryptionAcknowledged'] = True
        report['providerVolumeSizeGb'] = volume['size_gb']
        observed_volume = json.loads(call(['volumes', 'show', volume['id'], '-a', APP, '--json'], 'original-volume-readback')['stdout'])
        assert_volume(observed_volume)

        config = {'image': IMAGE, 'guest': {'cpu_kind': 'shared', 'cpus': 1, 'memory_mb': 512},
                  'restart': {'policy': 'no'}, 'auto_destroy': False, 'env': {}, 'services': [],
                  'mounts': [{'volume': volume['id'], 'path': MOUNT}],
                  'init': {'exec': ['/usr/bin/sleep', '600']}, 'metadata': metadata}
        durable_new(output / 'machine.json', config)
        machine_started = True
        durable_new(output / 'machine-create-started.json', {'nonce': nonce, 'name': name, 'app': APP,
                       'region': REGION, 'volumeId': volume['id'], 'configSha256': digest(canonical(config))})
        result = call(['machine', 'run', IMAGE, '-a', APP, '--machine-config', str(output / 'machine.json'),
                       '--name', name, '--region', REGION, '--detach', '--skip-dns-registration'],
                      'machine-create-once', 100, allow_failure=True)
        ids = re.findall(rb'(?m)^\s*Machine ID:\s*([0-9a-f]{14})\s*$', result['stdout'])
        need(len(ids) == 1, 'original-machine-create-id-absent')
        machine = {'id': ids[0].decode('ascii')}
        durable_new(output / 'original-machine-create-id.json', {**machine, 'nonce': nonce,
                    'stdoutSha256': digest(result['stdout']), 'exitCode': result['exitCode'], 'timedOut': result['timedOut']})
        need(result['exitCode'] == 0 and not result['timedOut'], 'original-machine-create-not-acknowledged')
        machine_ack = True
        for poll in range(8):
            current = find_original_machine(machines('original-machine-observe'))
            need(current is not None, 'original-machine-not-observed')
            assert_machine(current, allow_first_instance=True)
            if current.get('state') == 'started':
                break
            need(poll < 7 and work_end - now() > 2, 'original-boot-window-expired')
            time.sleep(min(2, work_end - now()))
        else:
            raise RuntimeError('original-boot-window-expired')
        assert_volume(json.loads(call(['volumes', 'show', volume['id'], '-a', APP, '--json'], 'attached-volume-readback')['stdout']), machine['id'])
        command = shlex.join([NODE, '--input-type=module', '-e', probe.decode('utf8'), '--',
                              nonce, volume['id'], machine['id'], instance, probe_sha])
        result = call(['machine', 'exec', machine['id'], '-a', APP, command, '--timeout', '30'],
                      'fixed-node-volume-probe', 35, allow_failure=True)
        outputs = []
        for line in result['stdout'].splitlines():
            if line.startswith(b'{'):
                need(len(line) <= 16384, 'guest-proof-bound')
                outputs.append(json.loads(line))
        need(len(outputs) == 1, 'one-original-guest-proof-required')
        guest = outputs[0]
        report['guest'] = guest
        durable_new(output / 'guest-proof.json', guest)
        need(result['exitCode'] == 0 and not result['timedOut'], 'guest-exec-not-completed')
        for key, expected in [('nonce', nonce), ('volumeId', volume['id']), ('machineId', machine['id']),
                              ('instanceId', instance), ('probeSha256', probe_sha), ('mountPath', MOUNT),
                              ('scope', 'fresh-Fly-volume-filesystem-component-only')]:
            need(guest.get(key) == expected, 'guest-original-binding-changed:' + key)
        for key in ['passed', 'newEmptyExceptLostFound', 'rootOwned', 'exclusiveCreate', 'noFollow',
                    'fileFsync', 'directoryFsync', 'atomicRename', 'exactReadback',
                    'originalMountRetained', 'ownedFileHandlesClosed']:
            need(guest.get(key) is True, 'guest-condition-failed:' + key)
        for key in ['coldRestartPerformed', 'restartDurabilityQualified', 'encryptionVerifiedByGuest',
                    'physicalRootQualified', 'sourceAuthority', 'sdkAuthority', 'haQualified',
                    'fleetDurability', 'normalRuntimeRetirement', 'productionEnablement', 'models']:
            need(guest.get(key) is False, 'guest-scope-expanded:' + key)
        expected = b'rudder-fresh-volume-probe-v1\nnonce=' + nonce.encode() + b'\n' + bytes(i % 251 for i in range(4096))
        need(guest.get('writeBytes') == len(expected) and guest.get('sha256') == digest(expected)
             and guest.get('preRenameSha256') == digest(expected), 'deterministic-write-readback-mismatch')
        current = find_original_machine(machines('post-probe-original-machine'))
        need(current is not None and current.get('state') == 'started', 'original-vm-lost-after-probe')
        assert_machine(current)
        assert_volume(json.loads(call(['volumes', 'show', volume['id'], '-a', APP, '--json'], 'post-probe-volume')['stdout']), machine['id'])
        need(now() < work_end, 'original-workload-expired')
        report['componentPassed'] = True
    except Exception as error:
        report['error'] = {'kind': type(error).__name__, 'reason': str(error)[:200] if isinstance(error, RuntimeError) else 'bounded-local-or-provider-data-failure'}
    except BaseException:
        ACTIVE.hold()
    finally:
        ACTIVE.unknown_create(volume_started,volume_ack,machine_started,machine_ack)
        # This separate cleanup clock cannot reopen the expired workload or
        # repeat an uncertain create. All effect attempts keep their raw logs.
        cleanup_end = min(start + WORK_SECONDS + CLEANUP_SECONDS, now() + CLEANUP_SECONDS)
        report['workloadElapsedSeconds'] = round(now() - start, 3)
        report['originalVolumeCreateAcknowledged'] = volume_ack
        report['originalMachineCreateAcknowledged'] = machine_ack
        report['volumeCreateUncertain'] = volume_started and not volume_ack
        report['machineCreateUncertain'] = machine_started and not machine_ack
        report['volumeId'] = volume.get('id') if volume else None
        report['machineId'] = machine.get('id') if machine else None
        report['instanceId'] = instance
        try:
            rows = machines('cleanup-original-machine', cleanup=True)
            current = find_original_machine(rows)
            if current is not None:
                assert_machine(current, allow_first_instance=True)
                report['machineDestroyIssued'] = True
                result = call(['machine', 'destroy', machine['id'], '-a', APP, '--force'],
                              'cleanup-destroy-original-machine-once', 25, cleanup=True, allow_failure=True)
                report['machineDestroyAcknowledged'] = result['exitCode'] == 0 and not result['timedOut']
            elif machine is not None:
                report['machineAlreadyAbsentBeforeDestroy'] = True
            if machine is not None:
                for index in range(2):
                    rows = machines('cleanup-machine-absence-' + str(index), cleanup=True)
                    need(not any(m.get('id') == machine['id'] or m.get('name') == name
                                 or (m.get('config') or {}).get('metadata', {}).get('qualification') == nonce for m in rows),
                         'original-machine-destroy-unresolved')
                    if index == 0:
                        time.sleep(min(1, max(0, cleanup_end - now())))
            # An unobserved/unknown Machine create might still attach later.
            # Even a currently detached volume cannot be deleted in that case.
            need(not (machine_started and not machine_ack), 'unknown-machine-create-retains-volume')
            if volume is not None:
                need(volume['id'] not in (protected or {}).get('volumeIds', []), 'protected-volume-cleanup-refused')
                rows = volumes('cleanup-original-volume', cleanup=True)
                exact = [v for v in rows if v.get('id') == volume['id']]
                need(len(exact) <= 1, 'volume-id-ambiguous')
                if exact:
                    assert_volume(exact[0], require_contract=False)
                    report['volumeDestroyIssued'] = True
                    result = call(['volumes', 'destroy', volume['id'], '-a', APP, '--yes'],
                                  'cleanup-destroy-original-volume-once', 25, cleanup=True, allow_failure=True)
                    report['volumeDestroyAcknowledged'] = result['exitCode'] == 0 and not result['timedOut']
            observations = []
            for index in range(2):
                ms = machines('cleanup-final-machine-absence-' + str(index), cleanup=True)
                vs = volumes('cleanup-final-volume-absence-' + str(index), cleanup=True)
                unchanged = protected_identity(machines('cleanup-protected-home-' + str(index), PROTECTED_APP, cleanup=True))
                need(protected is not None and unchanged == protected, 'protected-home-not-unchanged')
                machine_absent = not any((machine is not None and m.get('id') == machine['id']) or m.get('name') == name
                                         or (m.get('config') or {}).get('metadata', {}).get('qualification') == nonce for m in ms)
                volume_absent = not any((volume is not None and v.get('id') == volume['id']) or v.get('name') == volume_name for v in vs)
                observations.append({'machineAbsent': machine_absent, 'volumeAbsent': volume_absent, 'protectedHomeUnchanged': True})
                need(machine_absent and volume_absent, 'original-resource-absence-not-established')
                if index == 0:
                    time.sleep(min(1, max(0, cleanup_end - now())))
            report['cleanupObservations'] = observations
            report['protectedHomeUnchanged'] = True
            need(now() < cleanup_end, 'original-cleanup-window-expired')
            report['cleanupVerified'] = (not report['volumeCreateUncertain'] and not report['machineCreateUncertain']
                                         and (machine is None or (report.get('machineDestroyIssued') is True and report.get('machineDestroyAcknowledged') is True))
                                         and (volume is None or (report.get('volumeDestroyIssued') is True and report.get('volumeDestroyAcknowledged') is True)))
        except Exception as error:
            report['cleanupError'] = {'kind': type(error).__name__, 'reason': str(error)[:200] if isinstance(error, RuntimeError) else 'bounded-cleanup-observation-failure'}
        report['providerCalls'] = calls
        report['instanceId'] = instance
        report['actualCliChildrenReaped'] = all(custody(child) for child in children)
        report['allOriginalBothEOF']=all(child['stdoutEOF']is True and child['stderrEOF']is True for child in children)
        report['processExecutableFdHeld']=False
        report['verifiedAt'] = utc()
        report['elapsedSeconds'] = round(now() - start, 3)
        return ACTIVE.finish(report)

if __name__ == '__main__':
    try:code=main()
    except BaseException:
        if ACTIVE is not None and ACTIVE.effectAttempted:ACTIVE.hold()
        raise SystemExit('FRESH_VOLUME_PRE_EFFECT_REFUSED')from None
    raise SystemExit(code)
