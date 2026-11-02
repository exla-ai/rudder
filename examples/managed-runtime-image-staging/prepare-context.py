PACKAGING_EFFECTS_ALLOWED = False
if PACKAGING_EFFECTS_ALLOWED is not True:
    raise SystemExit('DISABLED_GENUINE_INGRESS_CONTROLLER_PACKAGING')

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
BINDING = {'integratedRevision':None,'integratedSourceManifestSha256':None,'compiledModules':None,'flyctlSha256':None,'qualifyRuntimeRoleSource':None}
MODULES=('registry.js','staging-ordinary-sdk-driver.js','staging-genuine-ingress-experiment.js','staging-genuine-ingress-collector.js','staging-sdk-runtime-flow.js')
ENTRY_SOURCE_PATH='examples/managed-runtime-image/qualify-runtime-role.mjs'
EXPECTED_ENTRY_SOURCE={'sha256': 'a4f7e3775e25fd6c2e4a821a4311839baf6f10025afb80b853c26bc7ed011cde', 'bytes': 12030}
def require_bound_row(row):
 assert type(row)is dict and set(row)=={'sha256','bytes'}
 assert type(row['sha256'])is str and re.fullmatch('[a-f0-9]{64}',row['sha256'])
 assert type(row['bytes'])is int and 0<row['bytes']<=262144
def require_bound_payload(raw,row):
 require_bound_row(row)
 assert len(raw)==row['bytes'] and hashlib.sha256(raw).hexdigest()==row['sha256']
def validate_packaging_binding(binding):
 assert type(binding)is dict and set(binding)=={'integratedRevision','integratedSourceManifestSha256','compiledModules','flyctlSha256','qualifyRuntimeRoleSource'}
 assert type(binding['integratedRevision'])is str and re.fullmatch('[a-f0-9]{40}',binding['integratedRevision'])
 assert type(binding['integratedSourceManifestSha256'])is str and re.fullmatch('[a-f0-9]{64}',binding['integratedSourceManifestSha256'])
 assert type(binding['compiledModules'])is dict and set(binding['compiledModules'])==set(MODULES)
 for row in binding['compiledModules'].values():require_bound_row(row)
 assert type(binding['flyctlSha256'])is str and re.fullmatch('[a-f0-9]{64}',binding['flyctlSha256'])
 require_bound_row(binding['qualifyRuntimeRoleSource'])
 assert binding['qualifyRuntimeRoleSource']==EXPECTED_ENTRY_SOURCE
validate_packaging_binding(BINDING)
source,setup,archive,output,flyctl=map(Path,sys.argv[1:])
assert flyctl.is_absolute() and flyctl.resolve()==flyctl
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
required.extend(MODULES)
for name in MODULES:
 row=BINDING['compiledModules'][name]
 raw=(source/'cloud/dist/managed-home-provisioning'/name).read_bytes();require_bound_payload(raw,row)
for f in required:assert (source/'cloud/dist/managed-home-provisioning'/f).is_file(), 'Root must build coherent completion/bridge Cloud source first'
for folder in ['managed-home-provisioning','project-authority']:
 for p in sorted((source/'cloud/dist'/folder).glob('*.js')):copy(p,'cloud/dist/'+folder+'/'+p.name)
# Bind the bytes actually copied, in addition to the original input read.
for name in MODULES:
 assert {k:entries['cloud/dist/managed-home-provisioning/'+name][k]for k in ('sha256','bytes')}==BINDING['compiledModules'][name]
for p in sorted((source/'cloud/dist').glob('*.js')):
 if p.name.startswith('fabric-') or p.name in ['fly-custody.js','home-control-sessions.js']:copy(p,'cloud/dist/'+p.name)
assert (source/'cloud/dist/home-control-sessions.js').is_file(), 'Root must build the actual session transport dependency'
# Staging-control only. tsc emits JS but does not copy these fixed Python sources.
PYTHON_FILES={'genuine-ingress-log-worker.py':'77bfde8dc867b869ac2971f3973c8ec91e7c1f0bfcf7a46420208f79fa011b8a','genuine-ingress-log-project.py':'df3e53242dfbb0b360caf6b69df363e0467e00fc50ad9f33cf5b893088312d18'}
for name,digest in PYTHON_FILES.items():
 file=source/'cloud/src/managed-home-provisioning'/name;assert sha(file.read_bytes())==digest
 copy(file,'cloud/dist/managed-home-provisioning/'+name,0o444)
st=flyctl.lstat();assert stat.S_ISREG(st.st_mode) and st.st_uid in [0,os.geteuid()] and st.st_mode&0o111 and not st.st_mode&0o022 and 0<st.st_size<=256*1024*1024
raw=flyctl.read_bytes();assert len(raw)==st.st_size and raw[:4]==b'\x7fELF' and sha(raw)==BINDING['flyctlSha256']
copy(flyctl,'flyctl',0o555)
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
entry=source/ENTRY_SOURCE_PATH
require_bound_payload(entry.read_bytes(),BINDING['qualifyRuntimeRoleSource'])
copy(entry,ENTRY_SOURCE_PATH)
assert {k:entries[ENTRY_SOURCE_PATH][k]for k in ('sha256','bytes')}==BINDING['qualifyRuntimeRoleSource']
for name in ['supervise.py','install.mjs','health.mjs','receive-private.py','genuine-ingress-target-check.mjs','genuine-ingress-target-probe.py']:
 copy(setup/name,'examples/managed-runtime-image-staging/'+name,0o555 if name.endswith('.py') else 0o444)
shutil.copyfile(setup/'Dockerfile',output/'Dockerfile')
shutil.copyfile(setup/'fly.toml',output/'fly.toml')
manifest={'format_version':1,'scope':'staging-control-and-single-PG-only','base_pg_image':'registry.fly.io/rudder-workers-exla@sha256:b0c21439e441671351766e8a2f4e87810887a7d6add0bac4959d7c16388143a4','node_origin_archive_sha256':NODE_ARCHIVE_SHA,'node_sha256':NODE_SHA,'dependencies':sorted(seen),'files':entries,'total_bytes':total,'managed_worker_image':False,'native4_or_role_qualification':False,'genuine_ingress_packaging':dict(BINDING,pythonFiles=PYTHON_FILES,targetQualified=False,actualGenuineCapture=False)}
(payload/'context-manifest.json').write_text(json.dumps(manifest,sort_keys=True,separators=(',',':'))+'\n');(payload/'context-manifest.json').chmod(0o444)
print(json.dumps({'scope':'prepared-source-and-actual-a3-node-DATA-only','files':len(entries),'bytes':total,'context_manifest_sha256':sha((payload/'context-manifest.json').read_bytes()),'no_build_or_provider_action':True}))
