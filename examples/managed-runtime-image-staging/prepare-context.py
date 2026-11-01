#!/usr/bin/env python3
"""ROOT SOURCE PREPARER. Copies already built Cloud modules/dependency closure;
extracts only the actually qualified a3 Node ELF. No compiler/package/provider
execution. Never reads operator credential files or fabric/customer data.
"""
import hashlib,json,os,re,shutil,stat,sys,tarfile
from pathlib import Path
NODE_ARCHIVE_SHA='7ce81644609f1e653be10c9c855cf3814b624166e673fae45acff86beb661342'
NODE_SHA='94efd062ed066c1a0b332e18249859c905e920eeada5df55d24c99b919629a90'
MAX_TOTAL=512*1024*1024
source,setup,archive,output=map(Path,sys.argv[1:])
for p in [source,setup,archive]:assert p.is_absolute() and p.resolve()==p
assert output.is_absolute() and not output.exists();output.mkdir(mode=0o700)
payload=output/'payload';payload.mkdir();entries={};total=0
sha=lambda b:hashlib.sha256(b).hexdigest()
def copy(src,rel,mode=0o644):
 global total
 st=src.lstat();assert stat.S_ISREG(st.st_mode) and st.st_nlink==1 and not st.st_mode&0o022
 b=src.read_bytes();assert len(b)==st.st_size;total+=len(b);assert total<=MAX_TOTAL and len(entries)<32768
 dest=payload/rel;dest.parent.mkdir(parents=True,exist_ok=True);dest.write_bytes(b);dest.chmod(mode);entries[rel]={'sha256':sha(b),'bytes':len(b),'mode':oct(mode)}
assert archive.stat().st_size<=MAX_TOTAL
raw=archive.read_bytes();assert sha(raw)==NODE_ARCHIVE_SHA
with tarfile.open(archive,'r:gz') as t:
 members=[m for m in t.getmembers() if m.name.removeprefix('./')=='node'];assert len(members)==1
 m=members[0];assert m.isfile() and 0<m.size<=256*1024*1024
 b=t.extractfile(m).read();assert len(b)==m.size and b[:4]==b'\x7fELF' and sha(b)==NODE_SHA
 (payload/'node').write_bytes(b);(payload/'node').chmod(0o555);entries['node']={'sha256':NODE_SHA,'bytes':len(b),'mode':'0o555'};total+=len(b)
required=['runtime-qualification-driver.js','runtime-qualification-completion-schema.js','runtime-qualification.js',
 'compiler-qualification-driver.js','compiler-qualification-completion-schema.js','compiler-qualification.js','compiler-sdk-semantic-data.js',
 'staging-role-activation-data.js','staging-role-activation.js','staging-listener-admission.js','staging-ordinary-sdk-driver.js','staging-sdk-runtime-flow.js']
for f in required:assert (source/'cloud/dist/managed-home-provisioning'/f).is_file(), 'Root must build coherent completion/bridge Cloud source first'
for folder in ['managed-home-provisioning','project-authority']:
 for p in sorted((source/'cloud/dist'/folder).glob('*.js')):copy(p,'cloud/dist/'+folder+'/'+p.name)
for p in sorted((source/'cloud/dist').glob('*.js')):
 if p.name.startswith('fabric-') or p.name in ['fly-custody.js','home-control-sessions.js']:copy(p,'cloud/dist/'+p.name)
assert (source/'cloud/dist/home-control-sessions.js').is_file(), 'Root must build the actual session transport dependency'
copy(source/'cloud/package.json','cloud/package.json')
copy(source/'cloud/package-lock.json','cloud/package-lock.json')
seen=set()
def dependency(name,parent=None,optional=False):
 assert re.fullmatch(r'(?:@[a-z0-9_.-]+/)?[a-z0-9_.-]+',name)
 probe=parent or source/'cloud'
 while True:
  root=probe/'node_modules'/name
  if root.exists():break
  if probe==source/'cloud':
   assert optional, 'missing required dependency '+name
   return
  assert source/'cloud' in probe.parents
  probe=probe.parent
 assert root.is_dir() and not root.is_symlink() and root.resolve()==root
 relroot=root.relative_to(source/'cloud/node_modules')
 if str(relroot) in seen:return
 seen.add(str(relroot))
 package=json.loads((root/'package.json').read_text())
 for directory,dirs,files in os.walk(root,followlinks=False):
  dirs[:]=[d for d in dirs if d!='node_modules']
  for d in dirs:assert not (Path(directory)/d).is_symlink()
  for f in files:
   p=Path(directory)/f;copy(p,'cloud/node_modules/'+str(relroot)+'/'+str(p.relative_to(root)))
 for child in package.get('dependencies',{}):dependency(child,root)
 for child in package.get('optionalDependencies',{}):dependency(child,root,optional=True)
for name in ['pg','ws','@aws-sdk/client-s3','@aws-sdk/credential-provider-web-identity']:dependency(name)
copy(source/'examples/managed-runtime-image/qualify-runtime-role.mjs','examples/managed-runtime-image/qualify-runtime-role.mjs')
for name in ['supervise.py','install.mjs','health.mjs','receive-private.py']:
 copy(setup/name,'examples/managed-runtime-image-staging/'+name,0o555 if name.endswith('.py') else 0o444)
shutil.copyfile(setup/'Dockerfile',output/'Dockerfile')
shutil.copyfile(setup/'fly.toml',output/'fly.toml')
manifest={'format_version':1,'scope':'staging-control-and-single-PG-only','base_pg_image':'registry.fly.io/rudder-workers-exla@sha256:b0c21439e441671351766e8a2f4e87810887a7d6add0bac4959d7c16388143a4','node_origin_archive_sha256':NODE_ARCHIVE_SHA,'node_sha256':NODE_SHA,'dependencies':sorted(seen),'files':entries,'total_bytes':total,'managed_worker_image':False,'native4_or_role_qualification':False}
(payload/'context-manifest.json').write_text(json.dumps(manifest,sort_keys=True,separators=(',',':'))+'\n');(payload/'context-manifest.json').chmod(0o444)
print(json.dumps({'scope':'prepared-source-and-actual-a3-node-DATA-only','files':len(entries),'bytes':total,'context_manifest_sha256':sha((payload/'context-manifest.json').read_bytes()),'no_build_or_provider_action':True}))
