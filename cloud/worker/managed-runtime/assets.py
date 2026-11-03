#!/usr/bin/env python3
"""Root image-build DATA tools, never a runtime/credential authority constructor.
Public pinned archives only; no installers, model requests, auth store or hooks.
"""
from pathlib import Path, PurePosixPath
import argparse, base64, ctypes, gzip, hashlib, io, json, os, shutil, stat, sys, tarfile, time, urllib.error, urllib.request, uuid

RUNTIME = '/opt/rudder-managed-runtime/v1'
SIDECAR = '/opt/rudder-host-control'
CODEX_SHA = 'b8a11fedf24b2dfb59a70c9e52b030f9a508dcd49cd2dd0d41956f4c785156b8'
CODEX_BYTES = 319346888
CLAUDE_SHA = 'fd13c70000fec3f2116dfbcbb6121210b4a29328bf0e46210a59dd594a67d824'
CLAUDE_BYTES = 328262000
PROFILE = 'fly-managed-image-v1'
NODE_BASE = 'node:22-bookworm-slim@sha256:4b150b48b4116e2a22f2c39cc27025fbf4f153e2078fd3b14d159fdb5c7279da'
CHUNK = 65536
# Local denial modules close ws optional-addon lookup inside the declared tree.
# ws catches this explicit error and uses its builtin/pure-JS implementation.
DENY = b"'use strict';\nthrow new Error('RUDDER_OPTIONAL_ADDON_DISABLED');\n"
SIDECAR_FILES = ('host-control-core.mjs', 'host-control.mjs', 'node_modules/ws/LICENSE', 'node_modules/ws/README.md', 'node_modules/ws/browser.js', 'node_modules/ws/index.js', 'node_modules/ws/lib/buffer-util.js', 'node_modules/ws/lib/constants.js', 'node_modules/ws/lib/event-target.js', 'node_modules/ws/lib/extension.js', 'node_modules/ws/lib/limiter.js', 'node_modules/ws/lib/permessage-deflate.js', 'node_modules/ws/lib/receiver.js', 'node_modules/ws/lib/sender.js', 'node_modules/ws/lib/stream.js', 'node_modules/ws/lib/subprotocol.js', 'node_modules/ws/lib/validation.js', 'node_modules/ws/lib/websocket-server.js', 'node_modules/ws/lib/websocket.js', 'node_modules/ws/node_modules/bufferutil/index.js', 'node_modules/ws/node_modules/utf-8-validate/index.js', 'node_modules/ws/package.json', 'node_modules/ws/wrapper.mjs', 'package-lock.json', 'package.json', 'root-upload.mjs', 'runtime-qualification.mjs')

class Rejected(Exception): pass

# Only fixed build stages and validation identifiers leave this process. Never
# format network exceptions: they can contain URLs, headers or response bodies.
CHECKS = frozenset('''ACQUISITION_FREE_FLOOR ACQUISITION_FREE_SPACE ARCHIVE_BYTE_BOUND
ARCHIVE_DUPLICATE_MEMBER ARCHIVE_FULL_INTEGRITY ARCHIVE_MEMBER_BYTES ARCHIVE_MEMBER_POLICY ARCHIVE_ROOT_FILE
ARTIFACT_ALREADY_EXISTS ARTIFACT_GREW ARTIFACT_INCOMPLETE ARTIFACT_WRITE_UNCERTAIN CLAUDE_CANDIDATE_PIN
CLOSURE_ENTRY_BOUND CLOSURE_FILE_BOUND CLOSURE_SPECIAL_FILE CLOSURE_SYMLINK CLOSURE_TOTAL_BYTES
CODEX_ELF_DIGEST CODEX_MEMBER_IDENTITY CODEX_MEMBER_MISSING CODEX_NOT_ELF FILE_BYTE_BOUND FILE_CHANGED
FILE_OPEN_CHANGED FREE_FLOOR_INVALID IMAGE_BUILD_REQUIRES_ROOT JSON_BOUND MANIFEST_ALREADY_EXISTS
MANIFEST_BYTE_BOUND MANIFEST_FILE_POLICY MANIFEST_MISMATCH METADATA_PIN_MISMATCH NOREPLACE_RENAME_FAILED
NOREPLACE_RENAME_UNAVAILABLE OPTIONAL_ADDON_LOOKUP_NOT_CLOSED ORIGINAL_BUILD_DEADLINE POLICY_IDENTITY
PROTECTED_CLOSURE_CHANGED PROTECTED_MODE_MISMATCH PUBLIC_ARCHIVE_ORIGIN PUBLIC_ARCHIVE_REDIRECT
PUBLIC_ARCHIVE_RESPONSE ROOT_DIRECTORY_OWNER ROOT_FILE_POLICY ROOT_PATH_REDIRECT
RUNTIME_EXACT_PINNED_CANDIDATES_REQUIRED SIDECAR_EXACT_27_FILE_CLOSURE SIDECAR_MANIFEST_ENTRY_SHAPE
SIDECAR_MANIFEST_SHAPE SIDECAR_MANIFEST_TOTAL_BYTES SIDECAR_POLICY_PIN_MISMATCH SIDECAR_SOURCE_DIGEST
SIDECAR_SOURCE_POLICY SIDECAR_UNDECLARED_PACKAGE SIDECAR_WRAPPER_MISSING SOURCE_METADATA_CHANGED
WRITE_UNCERTAIN WS_BUILD_RECEIPT_CLOSURE WS_BUILD_RECEIPT_IDENTITY WS_CLOSURE_MISSING WS_IDENTITY
WS_PACKAGE_IDENTITY'''.split())
_diagnostic_context = {'command': 'entry', 'artifact': None, 'phase': 'entry'}

def diagnostic_phase(phase):
    need(phase in {'entry','policy','metadata','space','public_download','extract','archive_integrity',
                  'publication','enumerate','source_policy','manifest','verify','components','addons'}, 'POLICY_IDENTITY')
    _diagnostic_context['phase'] = phase

def failure_diagnostic(error):
    value = {'kind': 'RUDDER_MANAGED_IMAGE_ASSET_REJECTED', **_diagnostic_context,
             'class': 'unexpected', 'check': None, 'status': None, 'errno': None}
    if isinstance(error, Rejected):
        value['class'] = 'validation'
        if len(error.args) == 1 and type(error.args[0]) is str and error.args[0] in CHECKS:
            value['check'] = error.args[0]
    elif isinstance(error, urllib.error.HTTPError):
        value['class'] = 'http_error'
        if type(error.code) is int and 100 <= error.code <= 599: value['status'] = error.code
    elif isinstance(error, TimeoutError): value['class'] = 'timeout'
    elif isinstance(error, urllib.error.URLError): value['class'] = 'url_error'
    elif isinstance(error, OSError):
        value['class'] = 'os_error'
        if type(error.errno) is int and 0 < error.errno <= 4095: value['errno'] = error.errno
    elif isinstance(error, (ValueError, KeyError, TypeError, tarfile.TarError)):
        value['class'] = 'data_error'
    return value

def need(value, code):
    if not value: raise Rejected(code)

def sha(data): return hashlib.sha256(data).hexdigest()

def canonical(value): return (json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False)+'\n').encode()

def read_json(path, limit=65536):
    with open(path, 'rb') as f: data=f.read(limit+1)
    need(len(data)<=limit, 'JSON_BOUND'); return json.loads(data),data

def policy_at(path):
    p,_=read_json(path)
    for field in ('sidecar_script_sha256','sidecar_core_sha256','uploader_script_sha256','qualification_script_sha256'):
        value=p.get(field);need(isinstance(value,str) and len(value)==64 and all(c in '0123456789abcdef' for c in value) and value!='0'*64,'SIDECAR_SOURCE_DIGEST')
    need(p['version']==1 and p['profile_id']==PROFILE and p['node_base_image']==NODE_BASE and p['codex']['bytes']==CODEX_BYTES
         and p.get('claude_supported') is False and p.get('registry_signature_verification_claimed') is False
         and p['codex']['sha256']==CODEX_SHA and p['codex']['archive_member']=='package/vendor/x86_64-unknown-linux-musl/bin/codex', 'POLICY_IDENTITY')
    need(p['codex']['url']=='https://registry.npmjs.org/@openai/codex/-/codex-0.166.0-linux-x64.tgz'
         and p['ws']['url']=='https://registry.npmjs.org/ws/-/ws-8.22.0.tgz', 'PUBLIC_ARCHIVE_ORIGIN')
    if p.get('claude_packaged'):
        need(p['claude_packaged'] is True and p['claude']['version']=='2.1.316' and p['claude']['platform']=='linux-x64'
             and p['claude']['sha256']==CLAUDE_SHA and p['claude']['bytes']==CLAUDE_BYTES
             and p['claude']['url']=='https://downloads.claude.ai/claude-code-releases/2.1.316/linux-x64/claude'
             and p['claude']['primary_fingerprint']=='31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE','CLAUDE_CANDIDATE_PIN')
    p['_raw_sha256']=sha(Path(path).read_bytes());return p

def original_end(seconds=180): return time.clock_gettime(time.CLOCK_BOOTTIME)+seconds

def check_time(end): need(time.clock_gettime(time.CLOCK_BOOTTIME)<end, 'ORIGINAL_BUILD_DEADLINE')

def directory(path):
    path=Path(path);need(path.is_absolute() and path.resolve()==path, 'ROOT_PATH_REDIRECT')
    for part in [path,*path.parents]:
        s=part.lstat();need(stat.S_ISDIR(s.st_mode) and not stat.S_ISLNK(s.st_mode) and s.st_uid==0 and s.st_gid==0
                          and not s.st_mode&0o022, 'ROOT_DIRECTORY_OWNER')
    return path

def file_stamp(s): return (s.st_dev,s.st_ino,s.st_size,s.st_mode,s.st_uid,s.st_gid,s.st_nlink,s.st_mtime_ns,s.st_ctime_ns)

def file_hash(path, limit, end):
    check_time(end);before=path.lstat()
    need(stat.S_ISREG(before.st_mode) and before.st_nlink==1 and before.st_uid==0 and before.st_gid==0
         and not before.st_mode&0o022 and 0<before.st_size<=limit, 'ROOT_FILE_POLICY')
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC|os.O_NONBLOCK)
    try:
        need(file_stamp(os.fstat(fd))==file_stamp(before), 'FILE_OPEN_CHANGED')
        h=hashlib.sha256();total=0
        while True:
            check_time(end);b=os.read(fd,CHUNK)
            if not b: break
            total+=len(b);need(total<=limit, 'FILE_BYTE_BOUND');h.update(b)
        need(total==before.st_size and file_stamp(os.fstat(fd))==file_stamp(path.lstat())==file_stamp(before), 'FILE_CHANGED')
        return h.hexdigest(),total
    finally: os.close(fd)

def write_new(path, data, mode):
    fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,mode)
    try:
        view=memoryview(data)
        while view:
            n=os.write(fd,view);need(n>0,'WRITE_UNCERTAIN');view=view[n:]
        os.fchmod(fd,mode);os.fsync(fd)
    finally: os.close(fd)

def rename_new(source,target):
    # Linux image builds only; no overwrite/racy pathname adoption.
    libc=ctypes.CDLL(None,use_errno=True)
    need(hasattr(libc,'renameat2'),'NOREPLACE_RENAME_UNAVAILABLE')
    result=libc.renameat2(-100,os.fsencode(source),-100,os.fsencode(target),1)
    need(result==0,'NOREPLACE_RENAME_FAILED')

def sync_file(path):
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
    try:os.fsync(fd)
    finally:os.close(fd)

def fsync_dir(path):
    fd=os.open(path,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
    try: os.fsync(fd)
    finally: os.close(fd)

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self,*_args,**_kwargs): raise Rejected('PUBLIC_ARCHIVE_REDIRECT')

class CountReader(io.RawIOBase):
    def __init__(self, raw, limit, end, digest=None): self.raw,self.limit,self.end,self.digest,self.count=raw,limit,end,digest,0
    def readable(self): return True
    def read(self,size=-1):
        check_time(self.end);data=self.raw.read(min(size if size>=0 else CHUNK,CHUNK));self.count+=len(data)
        need(self.count<=self.limit,'ARCHIVE_BYTE_BOUND')
        if self.digest is not None:self.digest.update(data)
        return data

def extract(raw, spec, output, artifact, end, bounds=lambda:None):
    """Internal DATA seam also used by small synthetic archive regressions.
    Production CLI obtains spec ONLY from checked immutable source policy.
    No archive member is executed. Nothing is published before full SRI.
    """
    diagnostic_phase('extract');check_time(end);bounds()
    compressed=CountReader(raw,spec['max_compressed_bytes'],end,hashlib.sha512())
    expanded=CountReader(gzip.GzipFile(fileobj=compressed,mode='rb'),spec['max_expanded_bytes'],end)
    staging=output.parent/('.'+output.name+'.pending-'+uuid.uuid4().hex)
    need(not output.exists() and not output.is_symlink(),'ARTIFACT_ALREADY_EXISTS')
    if artifact=='ws': staging.mkdir(mode=0o700)
    seen=set();selected=None;written=[];recorded=[];members=0
    with tarfile.open(fileobj=expanded,mode='r|') as archive:
        for entry in archive:
            check_time(end);bounds();members+=1;parts=PurePosixPath(entry.name).parts
            need(members<=128 and parts and parts[0]=='package' and all(v not in ('.','..','/') for v in parts)
                 and (entry.isfile() or entry.isdir()), 'ARCHIVE_MEMBER_POLICY')
            need(entry.name not in seen,'ARCHIVE_DUPLICATE_MEMBER');seen.add(entry.name)
            if entry.isdir(): continue
            need(0<=entry.size<=spec['max_expanded_bytes'],'ARCHIVE_MEMBER_BYTES')
            if artifact=='codex' and entry.name!=spec['archive_member']:continue
            target=staging if artifact=='codex' else staging.joinpath(*parts[1:])
            need(target!=staging or artifact=='codex','ARCHIVE_ROOT_FILE')
            if artifact=='ws':target.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
            if artifact=='codex':need(selected is None and entry.size==spec['bytes'],'CODEX_MEMBER_IDENTITY')
            source=archive.extractfile(entry);h=hashlib.sha256();count=0
            fd=os.open(target,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600)
            try:
                while True:
                    check_time(end);bounds();chunk=source.read(CHUNK)
                    if not chunk:break
                    count+=len(chunk);need(count<=entry.size,'ARTIFACT_GREW');view=memoryview(chunk)
                    while view:
                        n=os.write(fd,view);need(n>0,'ARTIFACT_WRITE_UNCERTAIN');view=view[n:]
                    h.update(chunk)
                need(count==entry.size,'ARTIFACT_INCOMPLETE');os.fsync(fd)
            finally:os.close(fd)
            written.append(target);recorded.append({'path':'/'.join(parts[1:]),'sha256':h.hexdigest(),'size_bytes':count})
            if artifact=='codex':
                need(h.hexdigest()==spec['sha256'],'CODEX_ELF_DIGEST');selected={'archivePath':entry.name,'bytes':count,'sha256':h.hexdigest()}
    # Drain through gzip EOF/footer AND every remaining raw archive byte before
    # validating the original full SHA512. No compressed archive is stored.
    while expanded.read(CHUNK):pass
    while compressed.read(CHUNK):pass
    check_time(end);bounds()
    diagnostic_phase('archive_integrity')
    need('sha512-'+base64.b64encode(compressed.digest.digest()).decode()==spec['integrity'],'ARCHIVE_FULL_INTEGRITY')
    if artifact=='codex':
        need(selected is not None,'CODEX_MEMBER_MISSING')
        with open(staging,'rb') as f:need(f.read(4)==b'\x7fELF','CODEX_NOT_ELF')
    else:
        need({'package/package.json','package/wrapper.mjs'}<=seen,'WS_CLOSURE_MISSING')
        package,_=read_json(staging/'package.json');need(package['name']=='ws' and package['version']=='8.22.0','WS_PACKAGE_IDENTITY')
    for path in written:os.chmod(path,0o555);sync_file(path)
    if artifact=='ws':
        for path in sorted(staging.rglob('*'),reverse=True):
            if path.is_dir():os.chmod(path,0o555);fsync_dir(path)
        os.chmod(staging,0o555);fsync_dir(staging)
    diagnostic_phase('publication');check_time(end);bounds();rename_new(staging,output);fsync_dir(output.parent)
    return {'artifact':artifact,'archiveIntegrityVerified':True,'registrySignaturesVerified':False,
            'archiveRetained':False,'compressedBytesRead':compressed.count,'expandedBytesRead':expanded.count,
            'memberCount':members,'selected':selected,'integrity':spec['integrity'],'files':sorted(recorded,key=lambda v:v['path']),'paidModelCalls':0}

def acquire(args):
    diagnostic_phase('policy');p=policy_at(args.policy);end=original_end();need(args.free_floor_bytes>=0,'FREE_FLOOR_INVALID');out=Path(args.output);directory(out.parent)
    if args.artifact=='codex':
        diagnostic_phase('metadata')
        metadata,raw=read_json(args.metadata,1024*1024);need(sha(raw)==p['codex']['metadata_sha256'],'SOURCE_METADATA_CHANGED')
        need(metadata['registryMetadata']['dist']['tarball']==p['codex']['url']
             and metadata['registryMetadata']['dist']['integrity']==p['codex']['integrity']
             and metadata['selected']['sha256']==CODEX_SHA and metadata['selected']['bytes']==CODEX_BYTES,'METADATA_PIN_MISMATCH')
    spec=p[args.artifact]
    # This is acquisition additional-output reserve, NOT Docker build peak.
    required=CODEX_BYTES if args.artifact=='codex' else 8*1024*1024
    diagnostic_phase('space');need(shutil.disk_usage(out.parent).free>args.free_floor_bytes+required+16*1024*1024,'ACQUISITION_FREE_SPACE')
    bounds=lambda:need(shutil.disk_usage(out.parent).free>args.free_floor_bytes+16*1024*1024,'ACQUISITION_FREE_FLOOR')
    if args.archive:
        with open(args.archive,'rb') as source:receipt=extract(source,spec,out,args.artifact,end,bounds)
    else:
        diagnostic_phase('public_download')
        opener=urllib.request.build_opener(urllib.request.ProxyHandler({}),NoRedirect())
        with opener.open(spec['url'],timeout=20) as response:
            need(response.status==200 and response.geturl()==spec['url'],'PUBLIC_ARCHIVE_RESPONSE')
            receipt=extract(response,spec,out,args.artifact,end,bounds)
    diagnostic_phase('publication');check_time(end);bounds()
    receipt['sourcePolicySha256']=sha(Path(args.policy).read_bytes());receipt['url']=spec['url']
    write_new(Path(args.receipt),canonical(receipt),0o444);fsync_dir(Path(args.receipt).parent)

def enumerate_files(root, kind, end):
    todo=[root];files={};entries=0;total=0;dirs=[]
    while todo:
        check_time(end);d=todo.pop();directory(d);dirs.append(d)
        for child in sorted(d.iterdir()):
            entries+=1;need(entries<=128,'CLOSURE_ENTRY_BOUND');s=child.lstat()
            need(not stat.S_ISLNK(s.st_mode),'CLOSURE_SYMLINK')
            if stat.S_ISDIR(s.st_mode):todo.append(child);continue
            need(stat.S_ISREG(s.st_mode),'CLOSURE_SPECIAL_FILE')
            relative=child.relative_to(root).as_posix()
            if relative=='manifest.json':
                need(s.st_uid==s.st_gid==0 and s.st_nlink==1 and stat.S_IMODE(s.st_mode)==0o444 and 0<s.st_size<=65536,'MANIFEST_FILE_POLICY');continue
            limit=max(CODEX_BYTES,CLAUDE_BYTES) if kind=='runtime' else 8*1024*1024
            digest,size=file_hash(child,limit,end);total+=size;need(total<=(1024*1024*1024 if kind=='runtime' else 8*1024*1024),'CLOSURE_TOTAL_BYTES')
            need(len(files)<64,'CLOSURE_FILE_BOUND');files[relative]=(digest,size)
    return files,dirs

def expected_sidecar(root, files, p, ws_receipt):
    expected={'host-control.mjs','host-control-core.mjs','root-upload.mjs','runtime-qualification.mjs','package.json','package-lock.json'}
    need(set(files)==set(SIDECAR_FILES) and len(files)==27,'SIDECAR_EXACT_27_FILE_CLOSURE')
    need(files['runtime-qualification.mjs'][0]==p['qualification_script_sha256'] and files['root-upload.mjs'][0]==p['uploader_script_sha256'] and files['host-control.mjs'][0]==p['sidecar_script_sha256'] and files['host-control-core.mjs'][0]==p['sidecar_core_sha256'] and files['package.json'][0]==p['sidecar_package_sha256']
         and files['package-lock.json'][0]==p['sidecar_lock_sha256'],'SIDECAR_SOURCE_POLICY')
    for name in files:
        need(name in expected or name.startswith('node_modules/ws/'),'SIDECAR_UNDECLARED_PACKAGE')
    need('node_modules/ws/wrapper.mjs' in files,'SIDECAR_WRAPPER_MISSING')
    receipt,_=read_json(ws_receipt)
    need(receipt['artifact']=='ws' and receipt['archiveIntegrityVerified'] is True and receipt['integrity']==p['ws']['integrity']
         and receipt['sourcePolicySha256']==p['_raw_sha256'],'WS_BUILD_RECEIPT_IDENTITY')
    archived={'node_modules/ws/'+f['path']:(f['sha256'],f['size_bytes']) for f in receipt['files']}
    stubs={'node_modules/ws/node_modules/'+n+'/index.js' for n in ['bufferutil','utf-8-validate']}
    actual={name:value for name,value in files.items() if name.startswith('node_modules/ws/') and name not in stubs}
    need(actual==archived,'WS_BUILD_RECEIPT_CLOSURE')
    package,_=read_json(root/'node_modules/ws/package.json');need(package['name']=='ws' and package['version']=='8.22.0','WS_IDENTITY')
    for name in ['bufferutil','utf-8-validate']:
        stub='node_modules/ws/node_modules/'+name+'/index.js'
        need(files.get(stub)==(sha(DENY),len(DENY)),'OPTIONAL_ADDON_LOOKUP_NOT_CLOSED')

def manifest_value(files,kind,policy=None):
    if kind=='runtime':
        expected={'codex':(CODEX_SHA,CODEX_BYTES)}
        if policy is not None and policy.get('claude_packaged') is True:expected['claude']=(CLAUDE_SHA,CLAUDE_BYTES)
        need(files==expected,'RUNTIME_EXACT_PINNED_CANDIDATES_REQUIRED')
        return {'version':1,'profile_id':PROFILE,'entries':[{'path':RUNTIME+'/'+name,'sha256':v[0],'size_bytes':v[1]} for name,v in sorted(files.items())]}
    return {'version':1,'files':[{'path':name,'sha256':v[0],'size_bytes':v[1]} for name,v in sorted(files.items())]}

def pin_value(value, raw, kind, policy):
    if kind=='runtime':return {'profile_id':PROFILE,'manifest_sha256':sha(raw),'entries':value['entries']}
    need(set(value)=={'version','files'} and value['version']==1,'SIDECAR_MANIFEST_SHAPE')
    need(all(set(entry)=={'path','sha256','size_bytes'} and isinstance(entry['sha256'],str) and len(entry['sha256'])==64
             and all(c in '0123456789abcdef' for c in entry['sha256']) and type(entry['size_bytes']) is int and 0<entry['size_bytes']<=8*1024*1024
             for entry in value['files']),'SIDECAR_MANIFEST_ENTRY_SHAPE')
    files={entry['path']:(entry['sha256'],entry['size_bytes']) for entry in value['files']}
    need(len(value['files'])==27 and set(files)==set(SIDECAR_FILES) and [entry['path'] for entry in value['files']]==list(SIDECAR_FILES),'SIDECAR_EXACT_27_FILE_CLOSURE')
    need(files['host-control.mjs'][0]==policy['sidecar_script_sha256'] and files['host-control-core.mjs'][0]==policy['sidecar_core_sha256'] and files['runtime-qualification.mjs'][0]==policy['qualification_script_sha256'] and files['root-upload.mjs'][0]==policy['uploader_script_sha256'],'SIDECAR_SOURCE_POLICY')
    need(sum(entry['size_bytes'] for entry in value['files'])<=8*1024*1024,'SIDECAR_MANIFEST_TOTAL_BYTES')
    return {'sidecar_manifest_sha256':sha(raw),'sidecar_script_sha256':policy['sidecar_script_sha256'],
            'sidecar_core_sha256':policy['sidecar_core_sha256'],'files':value['files']}

def manifest(args):
    diagnostic_phase('policy');p=policy_at(args.policy);end=original_end();root=directory(args.root)
    diagnostic_phase('enumerate');files,dirs=enumerate_files(root,args.kind,end)
    diagnostic_phase('source_policy')
    if args.kind=='sidecar':expected_sidecar(root,files,p,args.ws_receipt)
    diagnostic_phase('manifest')
    value=manifest_value(files,args.kind,p)
    data=canonical(value);need(len(data)<=(65536 if args.kind=='runtime' else 32768),'MANIFEST_BYTE_BOUND')
    named=root/'manifest.json'
    if args.verify:
        diagnostic_phase('verify')
        s=named.lstat();need(stat.S_ISREG(s.st_mode) and stat.S_IMODE(s.st_mode)==0o444 and s.st_uid==s.st_gid==0 and s.st_nlink==1,'MANIFEST_FILE_POLICY')
        actual,raw=read_json(named)
        need(actual==value and raw==data,'MANIFEST_MISMATCH')
        need(all(stat.S_IMODE(path.lstat().st_mode)==0o555 for path in [*dirs,*[root/n for n in files]]),'PROTECTED_MODE_MISMATCH')
    else:
        need(not named.exists() and not named.is_symlink(),'MANIFEST_ALREADY_EXISTS')
        for name in files:os.chmod(root/name,0o555);sync_file(root/name)
        temporary=root/('.manifest.pending-'+uuid.uuid4().hex);write_new(temporary,data,0o444)
        rename_new(temporary,named);fsync_dir(root)
        for path in sorted(dirs,key=lambda v:len(v.parts),reverse=True):os.chmod(path,0o555);fsync_dir(path)
        # Rehash exact protected closure before emitting config/source-policy pin.
        after,_=enumerate_files(root,args.kind,end);need(after==files,'PROTECTED_CLOSURE_CHANGED')
    pin=pin_value(value,data,args.kind,p)
    if args.pin:write_new(Path(args.pin),canonical(pin),0o444);fsync_dir(Path(args.pin).parent)

def deny_addons(args):
    diagnostic_phase('addons')
    root=directory(args.root)
    for name in ['bufferutil','utf-8-validate']:
        target=root/'node_modules/ws/node_modules'/name;target.mkdir(parents=True,mode=0o755)
        write_new(target/'index.js',DENY,0o555)

def components(args):
    diagnostic_phase('components')
    p=policy_at(args.policy);end=original_end();runtime,rb=read_json(Path(RUNTIME)/'manifest.json');sidecar,sb=read_json(Path(SIDECAR)/'manifest.json')
    pin_path=Path('/usr/local/share/rudder-managed-image/sidecar-policy-pin.json');file_hash(pin_path,65536,end)
    actual_pin,pin_raw=read_json(pin_path);expected_pin=pin_value(sidecar,sb,'sidecar',p)
    need(actual_pin==expected_pin and pin_raw==canonical(expected_pin),'SIDECAR_POLICY_PIN_MISMATCH')
    paths=['/usr/local/bin/node','/opt/rudder/dist/native/rudder-native','/opt/rudder/dist/native/rudder-jj',
           '/usr/local/bin/rudder-root-storage','/usr/local/bin/rudder-machine-root','/opt/rudder/dist/index.js','/opt/rudder/dist/managed-task-profile-cli.js']
    values=[]
    for name in paths:
        path=Path(name);directory(path.parent);digest,size=file_hash(path,512*1024*1024,end)
        values.append({'path':name,'sha256':digest,'size_bytes':size})
    result={'version':1,'scope':'immutable-image-components-only','productionStartup':False,'spec3':False,'models':False,
        'acceptedJournalAuthority':False,'claude_packaged':p.get('claude_packaged') is True,'claude_supported':False,'source_policy_sha256':p['_raw_sha256'],'node_base_image':p['node_base_image'],'runtime_manifest_sha256':sha(rb),'sidecar_manifest_sha256':sha(sb),'components':values}
    write_new(Path(args.output),canonical(result),0o444);fsync_dir(Path(args.output).parent)

def main():
    parser=argparse.ArgumentParser();sub=parser.add_subparsers(dest='command',required=True)
    a=sub.add_parser('acquire');a.add_argument('--policy',required=True);a.add_argument('--artifact',choices=['codex','ws'],required=True)
    a.add_argument('--metadata');a.add_argument('--output',required=True);a.add_argument('--receipt',required=True);a.add_argument('--archive')
    a.add_argument('--free-floor-bytes',type=int,default=512*1024*1024);a.set_defaults(run=acquire)
    m=sub.add_parser('manifest');m.add_argument('--policy',required=True);m.add_argument('--kind',choices=['runtime','sidecar'],required=True)
    m.add_argument('--root',required=True);m.add_argument('--pin');m.add_argument('--ws-receipt');m.add_argument('--verify',action='store_true');m.set_defaults(run=manifest)
    d=sub.add_parser('deny-addons');d.add_argument('--root',required=True);d.set_defaults(run=deny_addons)
    c=sub.add_parser('components');c.add_argument('--output',required=True);c.add_argument('--policy',required=True);c.set_defaults(run=components)
    args=parser.parse_args()
    _diagnostic_context.update(command=args.command,artifact=getattr(args,'artifact',getattr(args,'kind',None)),phase='entry')
    need(os.geteuid()==0 and os.getegid()==0,'IMAGE_BUILD_REQUIRES_ROOT');args.run(args)

if __name__=='__main__':
    try:main()
    except Exception as error:
        print(json.dumps(failure_diagnostic(error),sort_keys=True,separators=(',',':')),file=sys.stderr)
        raise SystemExit(1)
