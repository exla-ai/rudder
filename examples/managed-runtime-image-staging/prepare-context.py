#!/usr/bin/env python3
"""ROOT SOURCE PREPARER. Copies already built Cloud modules/dependency closure;
extracts only the actually qualified a3 Node ELF. No compiler/package/provider
execution. Never reads operator credential files or fabric/customer data.
"""
import argparse,hashlib,json,os,re,stat,tarfile
from pathlib import Path
NODE_ARCHIVE_SHA='7ce81644609f1e653be10c9c855cf3814b624166e673fae45acff86beb661342'
NODE_SHA='94efd062ed066c1a0b332e18249859c905e920eeada5df55d24c99b919629a90'
MAX_TOTAL=512*1024*1024
MODULES=('registry.js','staging-ordinary-sdk-driver.js','staging-genuine-ingress-experiment.js','staging-genuine-ingress-collector.js','staging-sdk-runtime-flow.js')
ENTRY_SOURCE_PATH='examples/managed-runtime-image/qualify-runtime-role.mjs'
def require_bound_row(row):
 assert type(row)is dict and set(row)=={'sha256','bytes'}
 assert type(row['sha256'])is str and re.fullmatch('[a-f0-9]{64}',row['sha256'])
 assert type(row['bytes'])is int and 0<row['bytes']<=262144
def require_bound_payload(raw,row):
 require_bound_row(row)
 assert len(raw)==row['bytes'] and hashlib.sha256(raw).hexdigest()==row['sha256']
def raw_row(raw):return {'sha256':hashlib.sha256(raw).hexdigest(),'bytes':len(raw)}
def stable_bytes(path):
 st=path.lstat();assert stat.S_ISREG(st.st_mode) and st.st_nlink==1 and not st.st_mode&0o022
 with path.open('rb') as f:
  before=os.fstat(f.fileno());raw=f.read(MAX_TOTAL+1);after=os.fstat(f.fileno())
 identity=lambda s:(s.st_dev,s.st_ino,s.st_mode,s.st_uid,s.st_gid,s.st_nlink,s.st_size,s.st_mtime_ns,s.st_ctime_ns)
 assert identity(st)==identity(before)==identity(after)==identity(path.lstat())
 assert len(raw)==st.st_size and len(raw)<=MAX_TOTAL
 return raw
def validate_capture(source,setup,receipt,source_archive):
 # Build receipt is explicit captured-build DATA, never role/qualification authority.
 assert type(receipt)is dict
 revision=receipt.get('sourceRevision');archive_sha=receipt.get('sourceArchiveSha256');generated=receipt.get('generatedJs')
 assert type(revision)is str and re.fullmatch('[a-f0-9]{40}',revision)
 assert type(archive_sha)is str and re.fullmatch('[a-f0-9]{64}',archive_sha)
 assert type(generated)is dict and 0<len(generated)<=32768
 actual={str(p.relative_to(source)) for root in (source/'dist',source/'cloud/dist') for p in root.rglob('*.js')}
 assert set(generated)==actual, 'generated JS closure differs from captured build receipt'
 for name,expected in generated.items():
  assert type(name)is str and re.fullmatch(r'(?:dist|cloud/dist)/[A-Za-z0-9_./-]+\.js',name) and '..' not in Path(name).parts
  assert type(expected)is dict and set(expected)=={'sha256','bytes'}
  assert type(expected['sha256'])is str and re.fullmatch('[a-f0-9]{64}',expected['sha256'])
  assert type(expected['bytes'])is int and 0<expected['bytes']<=MAX_TOTAL
  assert raw_row(stable_bytes(source/name))==expected, 'generated JS digest differs from captured build'
 assert setup==source/'examples/managed-runtime-image-staging', 'setup must come from the same captured source'
 archive_raw=stable_bytes(source_archive);assert hashlib.sha256(archive_raw).hexdigest()==archive_sha
 source_paths=[ENTRY_SOURCE_PATH,'examples/managed-runtime-image-staging/prepare-context.py','cloud/package.json','cloud/package-lock.json']
 source_paths+=['cloud/src/managed-home-provisioning/'+name for name in ('genuine-ingress-log-worker.py','genuine-ingress-log-project.py')]
 source_paths+=['examples/managed-runtime-image-staging/'+name for name in ('supervise.py','install.mjs','health.mjs','receive-private.py','genuine-ingress-target-check.mjs','genuine-ingress-target-probe.py','Dockerfile','fly.toml')]
 # Read only fixed members, never extract archive-controlled paths.
 import io
 with tarfile.open(fileobj=io.BytesIO(archive_raw),mode='r:*') as archive:
  members={}
  for member in archive.getmembers():
   name=member.name.removeprefix('./')
   if name in source_paths:
    assert name not in members and member.isfile() and 0<member.size<=MAX_TOTAL
    members[name]=member
  assert set(members)==set(source_paths)
  originals={name:archive.extractfile(members[name]).read() for name in source_paths}
 for name,raw in originals.items():assert stable_bytes(source/name)==raw, 'source differs from captured revision archive'
 assert stable_bytes(Path(__file__).resolve())==originals['examples/managed-runtime-image-staging/prepare-context.py'], 'packager differs from captured revision archive'
 prefix='cloud/dist/managed-home-provisioning/'
 compiled={name.removeprefix(prefix):expected for name,expected in generated.items() if name.startswith(prefix) and '/' not in name.removeprefix(prefix)}
 for name in MODULES:require_bound_row(compiled[name])
 return {'integratedRevision':revision,'sourceArchiveSha256':archive_sha,
  'generatedJsSha256':hashlib.sha256((json.dumps(generated,sort_keys=True,separators=(',',':'))+'\n').encode()).hexdigest(),
  'compiledModules':compiled,'qualifyRuntimeRoleSource':raw_row(originals[ENTRY_SOURCE_PATH])}, {name:raw_row(raw) for name,raw in originals.items()}

def prepare(args):
 source,setup,archive,output,flyctl=(getattr(args,name)for name in ('source','setup','archive','output','flyctl'))
 assert all(p.is_absolute() and p.resolve()==p for p in (source,setup,archive,args.build_receipt,args.source_archive))
 receipt_raw=stable_bytes(args.build_receipt);receipt=json.loads(receipt_raw)
 BINDING,source_rows=validate_capture(source,setup,receipt,args.source_archive)
 assert re.fullmatch('[a-f0-9]{64}',args.flyctl_sha256)
 BINDING['flyctlSha256']=args.flyctl_sha256
 assert flyctl.is_absolute() and flyctl.resolve()==flyctl
 for p in [source,setup,archive]:assert p.is_absolute() and p.resolve()==p
 assert output.is_absolute() and not output.exists();output.mkdir(mode=0o700)
 payload=output/'payload';payload.mkdir();entries={};total=0
 sha=lambda b:hashlib.sha256(b).hexdigest()
 def copy(src,rel,mode=0o644):
  nonlocal total
  st=src.lstat();assert stat.S_ISREG(st.st_mode) and st.st_nlink==1 and not st.st_mode&0o022
  b=stable_bytes(src)
  relative=str(src.relative_to(source)) if source in src.parents else None
  if relative and relative.endswith('.js') and relative.startswith('cloud/dist/'):assert raw_row(b)==receipt['generatedJs'][relative]
  if relative in source_rows:assert raw_row(b)==source_rows[relative], 'copied source differs from captured revision archive'
  if src==flyctl:assert sha(b)==BINDING['flyctlSha256']
  assert len(b)==st.st_size;total+=len(b);assert total<=MAX_TOTAL and len(entries)<32768
  dest=payload/rel;dest.parent.mkdir(parents=True,exist_ok=True);dest.write_bytes(b);dest.chmod(mode);entries[rel]={'sha256':sha(b),'bytes':len(b),'mode':oct(mode)}
 assert archive.stat().st_size<=MAX_TOTAL
 raw=stable_bytes(archive);assert sha(raw)==NODE_ARCHIVE_SHA
 with tarfile.open(archive,'r:gz') as t:
  members=[m for m in t.getmembers() if m.name.removeprefix('./')=='node'];assert len(members)==1
  m=members[0];assert m.isfile() and 0<m.size<=256*1024*1024
  b=t.extractfile(m).read();assert len(b)==m.size and b[:4]==b'\x7fELF' and sha(b)==NODE_SHA
  (payload/'node').write_bytes(b);(payload/'node').chmod(0o555);entries['node']={'sha256':NODE_SHA,'bytes':len(b),'mode':'0o555'};total+=len(b)
 required=['runtime-qualification-driver.js','runtime-qualification-completion-schema.js','runtime-qualification.js',
  'compiler-qualification-driver.js','compiler-qualification-completion-schema.js','compiler-qualification.js','compiler-sdk-semantic-data.js',
  'staging-role-activation-data.js','staging-role-activation.js','staging-listener-admission.js','staging-ordinary-sdk-driver.js','staging-sdk-runtime-flow.js','staging-positive-ingress-driver.js']
 required.extend(MODULES)
 for name in MODULES:
  row=BINDING['compiledModules'][name]
  raw=(source/'cloud/dist/managed-home-provisioning'/name).read_bytes();require_bound_payload(raw,row)
 for f in required:assert (source/'cloud/dist/managed-home-provisioning'/f).is_file(), 'Root must build coherent completion/bridge Cloud source first'
 for folder in ['managed-home-provisioning','project-authority']:
  for p in sorted((source/'cloud/dist'/folder).glob('*.js')):copy(p,'cloud/dist/'+folder+'/'+p.name)
 # Bind the bytes actually copied, in addition to the original input read.
 for name in BINDING['compiledModules']:
  assert {k:entries['cloud/dist/managed-home-provisioning/'+name][k]for k in ('sha256','bytes')}==BINDING['compiledModules'][name]
 for p in sorted((source/'cloud/dist').glob('*.js')):
  if p.name.startswith('fabric-') or p.name in ['fly-custody.js','home-control-sessions.js']:copy(p,'cloud/dist/'+p.name)
 assert (source/'cloud/dist/home-control-sessions.js').is_file(), 'Root must build the actual session transport dependency'
 # Staging-control only. tsc emits JS but does not copy these fixed Python sources.
 PYTHON_FILES={name:source_rows['cloud/src/managed-home-provisioning/'+name]['sha256'] for name in ('genuine-ingress-log-worker.py','genuine-ingress-log-project.py')}
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
 for name in ('Dockerfile','fly.toml'):
  raw=stable_bytes(setup/name);assert raw_row(raw)==source_rows['examples/managed-runtime-image-staging/'+name]
  (output/name).write_bytes(raw)
 assert stable_bytes(args.build_receipt)==receipt_raw, 'captured build receipt changed during packaging'
 manifest={'format_version':1,'scope':'staging-control-and-single-PG-only','base_pg_image':'registry.fly.io/rudder-workers-exla@sha256:b0c21439e441671351766e8a2f4e87810887a7d6add0bac4959d7c16388143a4','node_origin_archive_sha256':NODE_ARCHIVE_SHA,'node_sha256':NODE_SHA,'dependencies':sorted(seen),'files':entries,'total_bytes':total,'managed_worker_image':False,'native4_or_role_qualification':False,'captured_source':{'sourceRevision':BINDING['integratedRevision'],'sourceArchiveSha256':BINDING['sourceArchiveSha256'],'generatedJsSha256':BINDING['generatedJsSha256']},'source_files':source_rows,'generated_js':receipt['generatedJs'],'genuine_ingress_packaging':dict(BINDING,pythonFiles=PYTHON_FILES,targetQualified=False,actualGenuineCapture=False)}
 (payload/'context-manifest.json').write_text(json.dumps(manifest,sort_keys=True,separators=(',',':'))+'\n');(payload/'context-manifest.json').chmod(0o444)
 return {'scope':'prepared-source-and-actual-a3-node-DATA-only','files':len(entries),'bytes':total,'context_manifest_sha256':sha((payload/'context-manifest.json').read_bytes()),'no_build_or_provider_action':True}

def main():
 if not __debug__:raise SystemExit('controller packaging requires Python assertions; omit -O')
 parser=argparse.ArgumentParser(description=__doc__)
 for name in ('source','setup','archive','output','flyctl'):parser.add_argument(name,type=Path)
 parser.add_argument('--build-receipt',type=Path,required=True)
 parser.add_argument('--source-archive',type=Path,required=True)
 parser.add_argument('--flyctl-sha256',required=True)
 args=parser.parse_args()
 print(json.dumps(prepare(args)))

if __name__=='__main__':main()
