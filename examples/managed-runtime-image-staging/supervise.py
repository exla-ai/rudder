#!/usr/bin/env python3
"""Bounded staging setup with original Popen/pidfd/exe/pipe owners retained.
No automatic restart, existing-cluster adoption or managed retirement/cleanup.
Root alone tears down the exact owned worker resources, then this control VM.
"""
import hashlib,json,os,pwd,selectors,signal,stat,subprocess,sys,time,uuid
from pathlib import Path
ROOT=Path('/opt/rudder-runtime-qualification')
NODE=Path('/usr/local/bin/rudder-runtime-qualification-node')
FLY=Path('/workspace/home/.fly/bin/flyctl')
PGBIN=Path('/usr/lib/postgresql/17/bin')
INPUT=Path('/run/rudder-runtime-qualification')
clock=lambda:time.clock_gettime(time.CLOCK_BOOTTIME)
def restrict_injected_staging_files():
 # Fly currently injects mode0755 despite mode0600 in its Machine config.
 # The image-owned Root0700 parent protects these bytes before restriction.
 directory=os.open(INPUT,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC|os.O_NOFOLLOW);held.append(directory);parent=os.fstat(directory)
 assert stat.S_ISDIR(parent.st_mode) and parent.st_uid==0 and parent.st_gid==0 and stat.S_IMODE(parent.st_mode)==0o700
 parent_key=lambda s:(s.st_dev,s.st_ino,s.st_mode,s.st_uid,s.st_gid)
 assert parent_key(parent)==parent_key(os.lstat(INPUT))
 key=lambda s:(s.st_dev,s.st_ino,s.st_uid,s.st_gid,s.st_mode,s.st_nlink,s.st_size,s.st_mtime_ns,s.st_ctime_ns)
 for name,maximum in [('birth.json',4096),('operator.json',262144),('services.json',262144)]:
  try:fd=os.open(name,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW|os.O_NONBLOCK,dir_fd=directory)
  except FileNotFoundError:continue # Original manual protected-file delivery.
  held.append(fd);before=os.fstat(fd)
  assert stat.S_ISREG(before.st_mode) and before.st_uid==0 and before.st_gid==0 and before.st_nlink==1 and 0<before.st_size<=maximum and stat.S_IMODE(before.st_mode) in [0o600,0o755], 'injected staging file metadata refused'
  raw=os.pread(fd,before.st_size+1,0)
  assert len(raw)==before.st_size and key(before)==key(os.fstat(fd))==key(os.stat(name,dir_fd=directory,follow_symlinks=False))
  if stat.S_IMODE(before.st_mode)!=0o600:os.fchmod(fd,0o600);os.fsync(fd)
  after=os.fstat(fd);named=os.stat(name,dir_fd=directory,follow_symlinks=False)
  assert key(after)==key(named) and stat.S_IMODE(after.st_mode)==0o600 and (before.st_dev,before.st_ino,before.st_uid,before.st_gid,before.st_nlink,before.st_size,before.st_mtime_ns)==(after.st_dev,after.st_ino,after.st_uid,after.st_gid,after.st_nlink,after.st_size,after.st_mtime_ns)
  assert os.pread(fd,before.st_size+1,0)==raw
 assert parent_key(parent)==parent_key(os.fstat(directory))==parent_key(os.lstat(INPUT))
 os.fsync(directory)

def select_staging_birth():
 # Identity/restriction DATA only. Never an owner, clock or role/source grant.
 file=INPUT/'birth.json'
 try:fd=os.open(file,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW|os.O_NONBLOCK)
 except FileNotFoundError:return str(uuid.uuid4()),{'source':'generated-v1'}
 held.append(fd) # Retain original file BEFORE metadata/read/JSON postchecks.
 before=os.fstat(fd)
 assert stat.S_ISREG(before.st_mode) and before.st_uid==0 and before.st_gid==0 and before.st_nlink==1 and stat.S_IMODE(before.st_mode)==0o600 and 0<before.st_size<=4096
 directory=os.open(INPUT,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC|os.O_NOFOLLOW);held.append(directory);parent=os.fstat(directory)
 assert stat.S_ISDIR(parent.st_mode) and parent.st_uid==0 and parent.st_gid==0 and not parent.st_mode&0o022
 raw=os.pread(fd,before.st_size+1,0);after=os.fstat(fd);named=os.stat('birth.json',dir_fd=directory,follow_symlinks=False)
 key=lambda s:(s.st_dev,s.st_ino,s.st_uid,s.st_gid,s.st_mode,s.st_nlink,s.st_size,s.st_mtime_ns,s.st_ctime_ns)
 assert len(raw)==before.st_size and key(before)==key(after)==key(named)
 parent_named=os.lstat(INPUT)
 assert (parent.st_dev,parent.st_ino,parent.st_mode,parent.st_uid,parent.st_gid)==(parent_named.st_dev,parent_named.st_ino,parent_named.st_mode,parent_named.st_uid,parent_named.st_gid)
 def unique(pairs):
  value=dict(pairs);assert len(value)==len(pairs);return value
 value=json.loads(raw,object_pairs_hook=unique)
 assert isinstance(value,dict) and set(value)=={'format_version','kind','boot_nonce'}
 assert type(value['format_version']) is int and value['format_version']==1 and value['kind']=='operator-planned-staging-birth-v1'
 assert isinstance(value['boot_nonce'],str)
 parsed=uuid.UUID(value['boot_nonce']);assert parsed.version==4 and parsed.variant==uuid.RFC_4122 and str(parsed)==value['boot_nonce']
 return value['boot_nonce'],{'source':'protected-birth-file-v1','kind':value['kind'],'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw),'dev':before.st_dev,'ino':before.st_ino}

assert len(sys.argv)==1 and os.getuid()==0 and os.getgid()==0
start=clock();setup_end=start+900;held=[]
restrict_injected_staging_files()
nonce,birth_selection=select_staging_birth()
assert clock()<setup_end
work=Path('/var/lib/rudder-runtime-qualification')/('run-'+nonce)
pgwork=Path('/var/lib/rudder-runtime-qualification-pg')/('run-'+nonce)
work.mkdir(mode=0o700);pgwork.mkdir(mode=0o700);postgres=pwd.getpwnam('postgres');os.chown(pgwork,postgres.pw_uid,postgres.pw_gid)
selector=selectors.DefaultSelector();owners=[];stage='tls-bootstrap';failed=None;pg=None;driver=None;observation_failed=False

def write_json(file,value):
 raw=(json.dumps(value,sort_keys=True,indent=2)+'\n').encode();assert len(raw)<=131072
 temp=file.with_name(file.name+'.tmp-'+str(uuid.uuid4()));fd=os.open(temp,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_CLOEXEC|os.O_NOFOLLOW,0o600)
 try:
  at=0
  while at<len(raw):at+=os.write(fd,raw[at:])
  os.fsync(fd)
 finally:os.close(fd)
 os.replace(temp,file);d=os.open(file.parent,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC|os.O_NOFOLLOW)
 try:os.fsync(d)
 finally:os.close(d)

def protected(file,maximum=262144):
 fd=os.open(file,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW);held.append(fd);before=os.fstat(fd)
 assert stat.S_ISREG(before.st_mode) and before.st_uid==0 and before.st_gid==0 and before.st_nlink==1 and stat.S_IMODE(before.st_mode)==0o600 and 0<before.st_size<=maximum
 raw=os.pread(fd,before.st_size+1,0);after=os.fstat(fd);named=os.lstat(file)
 key=lambda s:(s.st_dev,s.st_ino,s.st_size,s.st_mode,s.st_mtime_ns,s.st_ctime_ns)
 assert len(raw)==before.st_size and key(before)==key(after)==key(named)
 return json.loads(raw)

def digest_file(file,retain=False):
 fd=os.open(file,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW);st=os.fstat(fd)
 assert stat.S_ISREG(st.st_mode) and st.st_uid==0 and st.st_gid==0 and not st.st_mode&0o022 and st.st_size<=256*1024*1024
 h=hashlib.sha256()
 while True:
  b=os.read(fd,1024*1024)
  if not b:break
  h.update(b)
 after=os.fstat(fd);named=os.lstat(file)
 key=lambda v:(v.st_dev,v.st_ino,v.st_size,v.st_mode,v.st_mtime_ns,v.st_ctime_ns)
 assert key(st)==key(after)==key(named)
 if retain:held.append(fd)
 else:os.close(fd)
 return {'sha256':h.hexdigest(),'bytes':st.st_size,'dev':st.st_dev,'ino':st.st_ino}

class ChildOwner:
 def __init__(self,label,argv,expected_exe,env=None):
  self.label=label;self.argv=argv;self.expected=expected_exe;self.child=None;self.pidfd=None;self.exe=None;self.status=None;self.spawn_unknown=False;self.eof={'stdout':False,'stderr':False};self.logs={};self.counts={'stdout':0,'stderr':0};self.truncated=False;self.log_failed=False
  owners.append(self) # BEFORE every spawn/pidfd/exe/pipe effect
  self.expected_image=digest_file(expected_exe,retain=True)
  self.child=subprocess.Popen.__new__(subprocess.Popen)
  try:self.child.__init__(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,cwd='/',env={} if env is None else env,close_fds=True)
  except BaseException:self.spawn_unknown=True;raise
  self.pidfd=os.pidfd_open(self.child.pid,0)
  for key,stream in [('stdout',self.child.stdout),('stderr',self.child.stderr)]:
   try:self.logs[key]=os.open(work/(label+'-'+key+'.private.log'),os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_CLOEXEC,0o600)
   except OSError:self.log_failed=True
   os.set_blocking(stream.fileno(),False);selector.register(stream,selectors.EVENT_READ,(self,key))
 def observe_exe(self,end,allow_settled=False):
  while clock()<end:
   self.poll()
   if self.status is not None:
    if allow_settled:return # actual short-lived setup command, no proc-exe claim
    raise RuntimeError('original child exited before image capture')
   try:
    fd=os.open('/proc/'+str(self.child.pid)+'/exe',os.O_RDONLY|os.O_CLOEXEC);got=os.fstat(fd)
    if (got.st_dev,got.st_ino)==(self.expected_image['dev'],self.expected_image['ino']):self.exe=fd;return
    os.close(fd)
   except FileNotFoundError:pass
   pump(.02)
  raise RuntimeError('original child image capture deadline')
 def poll(self):
  if self.child is not None and hasattr(self.child,'pid') and self.status is None:
   try:self.status=self.child.poll()
   except Exception:self.spawn_unknown=True # retain partial Popen, no settlement
 def settled(self):self.poll();return self.status is not None and all(self.eof.values())
 def signal(self,number):
  assert self.pidfd is not None;signal.pidfd_send_signal(self.pidfd,number)
 def data(self):
  return {'label':self.label,'pid_data':getattr(self.child,'pid',None),'original_pidfd_held':self.pidfd is not None,'original_exe_held':self.exe is not None,'exit_code':self.status,'pipe_eof':self.eof,'bytes':self.counts,'truncated':self.truncated,'log_failed':self.log_failed,'spawn_unknown':self.spawn_unknown}

def pump(timeout=.1):
 for event,_ in selector.select(timeout):
  owner,key=event.data
  try:b=os.read(event.fileobj.fileno(),65536)
  except BlockingIOError:continue
  if not b:selector.unregister(event.fileobj);owner.eof[key]=True;continue
  owner.counts[key]+=len(b)
  if owner.counts[key]<=8*1024*1024 and not owner.log_failed:
   try:
    at=0
    while at<len(b):at+=os.write(owner.logs[key],b[at:])
   except OSError:owner.log_failed=True
  else:owner.truncated=True # continue actual drain, do not block on log bound
 for owner in owners:owner.poll()

def oneshot(label,argv,exe,end,allowed=(0,)):
 owner=ChildOwner(label,argv,exe);owner.observe_exe(end,allow_settled=True)
 while not owner.settled():
  if clock()>=end:raise RuntimeError('original setup child deadline; owner retained')
  pump(.05)
 assert owner.status in allowed
 return owner

def status():
 global observation_failed
 try:write_json(work/'supervisor-status.json',{'format_version':1,'scope':'staging-control-PG-custody-DATA-only','boot_nonce':nonce,'birth_selection':birth_selection,'stage':stage,'failure':failed,'observation_output_failed':observation_failed,'children':[o.data() for o in owners],'setup_end_boottime_milliseconds':int(setup_end*1000),'setup_expired':clock()>=setup_end,'runtime_role_qualification':False,'root_or_helper_retirement':False,'provider_cleanup_required':True})
 except OSError:observation_failed=True # observation failure never drops child custody

write_json(work/'supervisor.json',{'format_version':1,'boot_nonce':nonce,'birth_selection':birth_selection,'supervisor_pid':os.getpid(),'pg_work':str(pgwork),'setup_end_boottime_milliseconds':int(setup_end*1000)})
try:
 # Node was actually used with this PG base in a3. All OTHER modules are freshly
 # built source/dependency bytes, verified against this new context manifest.
 manifest=json.loads((ROOT/'context-manifest.json').read_text());assert manifest['node_sha256']=='94efd062ed066c1a0b332e18249859c905e920eeada5df55d24c99b919629a90'
 assert digest_file(NODE)['sha256']==manifest['node_sha256']
 for rel,entry in manifest['files'].items():
  if rel=='node':continue
  assert not rel.startswith('/') and '..' not in rel.split('/')
  got=digest_file(FLY if rel=='flyctl' else ROOT/rel);assert got['sha256']==entry['sha256'] and got['bytes']==entry['bytes']
 health=ChildOwner('health',[str(NODE),str(ROOT/'examples/managed-runtime-image-staging/health.mjs')],NODE);health.observe_exe(min(setup_end,clock()+10))
 status()
 while not (INPUT/'operator.json').exists():
  assert clock()<setup_end and health.status is None;pump(.2)
 operator=protected(INPUT/'operator.json')
 if 'staging_flow' in operator:
  while not (INPUT/'services.json').exists():assert clock()<setup_end and health.status is None;pump(.2)
  protected(INPUT/'services.json')
 stage='postgres-init';status();end=min(setup_end,clock()+120)
 for name in ['tls','socket']:
  d=pgwork/name;d.mkdir(mode=0o700);os.chown(d,postgres.pw_uid,postgres.pw_gid)
 oneshot('initdb',['/usr/local/bin/gosu','postgres',str(PGBIN/'initdb'),'-D',str(pgwork/'data'),'--auth-local=trust','--auth-host=reject','--encoding=UTF8','--locale=C','--data-checksums'],PGBIN/'initdb',end)
 oneshot('openssl',['/usr/bin/openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1','-subj','/CN=localhost','-addext','subjectAltName=DNS:localhost','-addext','basicConstraints=critical,CA:TRUE','-keyout',str(pgwork/'tls/server.key'),'-out',str(pgwork/'tls/server.crt')],Path('/usr/bin/openssl'),end)
 for name,mode in [('server.key',0o600),('server.crt',0o644)]:os.chmod(pgwork/'tls'/name,mode);os.chown(pgwork/'tls'/name,postgres.pw_uid,postgres.pw_gid)
 conf="\n".join(["listen_addresses='localhost'","port=55432",f"unix_socket_directories='{pgwork}/socket'","unix_socket_permissions=0700","ssl=on","ssl_min_protocol_version='TLSv1.2'",f"ssl_cert_file='{pgwork}/tls/server.crt'",f"ssl_key_file='{pgwork}/tls/server.key'","password_encryption='scram-sha-256'","fsync=on","synchronous_commit=on","full_page_writes=on","shared_buffers='128MB'","max_connections=64","timezone='UTC'","log_statement='none'","log_min_error_statement='panic'","statement_timeout='10s'","lock_timeout='1s'",""])
 with open(pgwork/'data/postgresql.conf','a') as f:f.write(conf);f.flush();os.fsync(f.fileno())
 hba="local all postgres trust\nhostnossl all all 127.0.0.1/32 reject\nhostnossl all all ::1/128 reject\nhostssl all all 127.0.0.1/32 scram-sha-256\nhostssl all all ::1/128 scram-sha-256\n"
 (pgwork/'data/pg_hba.conf').write_text(hba);os.chown(pgwork/'data/pg_hba.conf',postgres.pw_uid,postgres.pw_gid)
 pg=ChildOwner('postgres',['/usr/local/bin/gosu','postgres',str(PGBIN/'postgres'),'-D',str(pgwork/'data')],PGBIN/'postgres');pg.observe_exe(end)
 while not (pgwork/'socket/.s.PGSQL.55432').exists():assert clock()<end and pg.status is None;pump(.05)
 for number in range(30):
  assert clock()<end and pg.status is None
  ready=oneshot('pg-ready-'+str(number),['/usr/local/bin/gosu','postgres',str(PGBIN/'pg_isready'),'-h',str(pgwork/'socket'),'-p','55432','-U','postgres','-t','1'],PGBIN/'pg_isready',end,allowed=(0,1,2))
  if ready.status==0:break
  pump(.1)
 else:raise RuntimeError('original PG readiness bound')
 stage='migrate-frozen';status()
 oneshot('migrate',[str(NODE),str(ROOT/'examples/managed-runtime-image-staging/install.mjs'),'migrate',str(work)],NODE,end)
 protected(work/'migrated.json');stage='frozen-awaiting-operator-admission';status()
 while not (INPUT/'admission.json').exists():assert clock()<setup_end and pg.status is None and health.status is None;pump(.2)
 protected(INPUT/'admission.json');stage='operator-admit';status();end=min(setup_end,clock()+120)
 oneshot('admit',[str(NODE),str(ROOT/'examples/managed-runtime-image-staging/install.mjs'),'admit',str(work)],NODE,end)
 protected(work/'admitted.json') # actual held original installer must also exit0+EOF
 stage='settle-health-before-driver';status();health.signal(signal.SIGTERM);end=min(setup_end,clock()+5)
 while not health.settled():assert clock()<end;pump(.05)
 assert health.status==-signal.SIGTERM
 stage='driver-retained';env={'RUDDER_RUNTIME_QUALIFICATION_INPUT':str(work/'driver-input.json'),'RUDDER_RUNTIME_QUALIFICATION_OUTPUT':str(work/'driver-output')}
 driver=ChildOwner('driver',[str(NODE),str(ROOT/'examples/managed-runtime-image/qualify-runtime-role.mjs')],NODE,env);driver.observe_exe(min(setup_end,clock()+10));status()
except BaseException:
 failed='STAGING_SETUP_REFUSED_OR_UNKNOWN';stage='quarantined-retained';status()
# No lease/TTL/restart/normal-cleanup inference. A referenced selector loop keeps
# every actual child/pidfd/exe/pipe owner alive through Root's owned VM teardown.
last=0
while True:
 pump(.2)
 if pg is not None and pg.status is not None:failed='ORIGINAL_POSTGRES_EXITED';stage='quarantined-retained'
 if driver is not None and driver.status is not None:failed='ORIGINAL_DRIVER_EXITED';stage='quarantined-retained'
 if clock()-last>=2:status();last=clock()
