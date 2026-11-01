#!/usr/bin/env python3
"""Root invokes ONLY through authenticated exact-owned provider/SSH custody.
Bounded stdin, fixed destination enum, expected raw SHA. Not a public service,
readiness issuer or Machine raw_value/base64 credential transport.
"""
import hashlib,os,stat,sys,uuid
from pathlib import Path
ROOT=Path('/run/rudder-runtime-qualification')
NAMES={'operator':ROOT/'operator.json','admission':ROOT/'admission.json','services':ROOT/'services.json',**{n:ROOT/'receipts'/(n+'.json') for n in ['provider','durability','fresh-volume','control-origin','image','storage','image-batch']}}
assert len(sys.argv)==3 and os.getuid()==0 and os.getgid()==0 and sys.argv[1] in NAMES
name,expected=sys.argv[1:];assert len(expected)==64 and all(c in '0123456789abcdef' for c in expected)
for directory in [ROOT,ROOT/'receipts']:
 s=os.lstat(directory);assert stat.S_ISDIR(s.st_mode) and s.st_uid==0 and s.st_gid==0 and stat.S_IMODE(s.st_mode)==0o700
path=NAMES[name];assert not path.exists();raw=sys.stdin.buffer.read(1048577);assert 0<len(raw)<=1048576 and hashlib.sha256(raw).hexdigest()==expected
if name in ['operator','admission','services']:assert len(raw)<=262144
pending=path.with_name(path.name+'.original-'+str(uuid.uuid4()))
fd=os.open(pending,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_CLOEXEC|os.O_NOFOLLOW,0o600)
try:
 at=0
 while at<len(raw):at+=os.write(fd,raw[at:])
 os.fsync(fd);s=os.fstat(fd);assert s.st_uid==0 and s.st_gid==0 and stat.S_IMODE(s.st_mode)==0o600 and s.st_nlink==1
 os.link(pending,path,follow_symlinks=False);os.unlink(pending)
 d=os.open(path.parent,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC|os.O_NOFOLLOW)
 try:os.fsync(d)
 finally:os.close(d)
finally:os.close(fd)
print('PRIVATE_INPUT_INSTALLED '+name+' '+expected)
