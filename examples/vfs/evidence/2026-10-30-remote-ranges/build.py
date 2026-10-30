from pathlib import Path
import uuid,json,hashlib,subprocess,shutil,os,tarfile,sys,time
repo=Path(sys.argv[1]).resolve();base=Path(sys.argv[2]).resolve();base.mkdir(parents=True,exist_ok=True)
run=base/str(uuid.uuid4());source=run/'source';source.mkdir(parents=True)
# Apply this allowlist to names BEFORE any content read/hash/copy. Runtime state
# can live inside source directories; a Git file listing is not a source policy.
EXACT_SUPPORT = {
 'Cargo.toml','Cargo.lock','engine/Cargo.toml','native/Cargo.toml','vcs-bridge/Cargo.toml',
 'engine/src/build/frontend_driver.py','engine/src/build/linux_runner.py',
 'native/src/permission_hook.mjs','assets/rudder.icns',
 'tests/fixtures/task-routing.json','tests/fixtures/readiness-cases.json',
 'examples/build-profile/Cargo.lock','examples/build-profile/package-lock.json',
 'examples/build-profile/tools.lock.json','examples/build-profile/prepare_tools.py',
 'examples/build-profile/run.py','examples/build-profile/rudder.toml',
 'examples/build-profile/rust/lib.rs','examples/build-profile/typescript/main.ts',
}
RUST_ROOTS = ('engine/src/','engine/tests/','native/src/','native/tests/',
              'vcs-bridge/src/','vcs-bridge/tests/','vcs-bridge/examples/')
HELPER_INPUTS = {'src/native-binary.ts','src/fabric-local.ts','src/fabric-remote.ts','src/fabric-fetch.ts'}
def source_name(relative):
 parts=Path(relative).parts
 if not parts or Path(relative).is_absolute() or any(p.startswith('.') or p in {'target','dist','node_modules','cache','sessions','logs','runtime'} for p in parts):return False
 return relative in EXACT_SUPPORT or (relative.endswith('.rs') and relative.startswith(RUST_ROOTS))
def regular_source(relative):
 original=repo/relative
 # Refuse symlinks at every component instead of following a source-shaped alias.
 cursor=repo
 for component in Path(relative).parts:
  cursor=cursor/component
  if cursor.is_symlink():raise RuntimeError('source allowlist contains a symlink')
 return original.is_file()
def names():
 candidates=subprocess.check_output(['git','ls-files','-co','--exclude-standard','-z','engine','native','vcs-bridge','examples/build-profile','Cargo.toml','Cargo.lock','assets/rudder.icns','tests/fixtures/task-routing.json','tests/fixtures/readiness-cases.json'],cwd=repo).split(b'\0')
 return sorted({p.decode() for p in candidates if p and source_name(p.decode())})
files={};originals=names()
for relative in originals:
 original=repo/relative
 if not regular_source(relative):continue
 data=original.read_bytes();destination=source/relative;destination.parent.mkdir(parents=True,exist_ok=True);destination.write_bytes(data);shutil.copystat(original,destination);files[relative]=hashlib.sha256(data).hexdigest()
assert originals==names()and all(hashlib.sha256((repo/name).read_bytes()).hexdigest()==sha for name,sha in files.items()),'source changed during exact freeze'
manifest={'format_version':1,'purpose':'exact-source remote mounted qualification','repository_revision':subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip(),'uncommitted_engine_source':True,'workspace_manifest_changed':False,'originals_unchanged_during_copy':True,'files':files};data=(json.dumps(manifest,sort_keys=True,separators=(',',':'))+'\n').encode();(run/'source-sha256.json').write_bytes(data);sha=hashlib.sha256(data).hexdigest();state={'directory':str(run),'source':str(source),'sourceManifestSha256':sha,'nonce':run.name,'name':'vfs-remote-'+run.name[:12]};(run/'state.json').write_text(json.dumps(state,indent=2)+'\n');(base/'current.json').write_text(json.dumps(state,indent=2)+'\n');print(json.dumps({'phase':'frozen','directory':str(run),'sourceManifestSha256':sha}),flush=True)
env=os.environ.copy();env['CARGO_TARGET_DIR']=str(repo/'target');env['RUDDER_QUALIFICATION_SOURCE_SHA']=sha
with(run/'build.log').open('w')as log:
 result=subprocess.run(['cargo','build','--manifest-path',str(source/'Cargo.toml'),'-p','rudder-engine','--bin','rudder-vfs-remote-qualifier','--release','--locked'],cwd=source,env=env,stdout=log,stderr=subprocess.STDOUT)
assert result.returncode==0,'frozen release build failed'
assert all(hashlib.sha256((source/name).read_bytes()).hexdigest()==value for name,value in files.items()),'frozen Rust inputs changed'
artifact=run/'payload';(artifact/'helper').mkdir(parents=True);driver=artifact/'driver';shutil.copy2(repo/'target/release/rudder-vfs-remote-qualifier',driver);info=json.loads(subprocess.check_output([str(driver),'--build-info'],text=True));assert info['build_source_sha256']==sha,'binary overwritten by another source build'
esbuild=str(repo/'node_modules/.bin/esbuild');subprocess.run([esbuild,'src/fabric-fetch.ts','--bundle','--platform=node','--format=esm','--outfile='+str(run/'discovery.mjs'),'--metafile='+str(run/'helper-metafile.json')],cwd=repo,check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
inputs=json.loads((run/'helper-metafile.json').read_text())['inputs'];helper={}
for name in inputs:
 assert name in HELPER_INPUTS and regular_source(name),'unexpected helper source input'
 data=(repo/name).read_bytes();helper[name]=hashlib.sha256(data).hexdigest();destination=source/name;destination.parent.mkdir(parents=True,exist_ok=True);destination.write_bytes(data)
subprocess.run([esbuild,'src/fabric-fetch.ts','--bundle','--platform=node','--format=esm','--outfile='+str(artifact/'helper/fetch.mjs')],cwd=source,check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
assert all(hashlib.sha256((source/name).read_bytes()).hexdigest()==digest for name,digest in helper.items())
(artifact/'helper/helper.mjs').write_text("import{runFabricFetchHelper}from'./fetch.mjs';await runFabricFetchHelper();\n");(run/'helper-input-sha256.json').write_text(json.dumps(helper,indent=2)+'\n')
with tarfile.open(run/'payload.tar.gz','w:gz')as archive:
 for entry in sorted(artifact.rglob('*')):archive.add(entry,arcname=str(entry.relative_to(artifact)),recursive=False)
with tarfile.open(run/'source.tar.gz','w:gz')as archive:
 for name in sorted(set(files)|set(helper)):
  assert source_name(name) or name in HELPER_INPUTS
  archive.add(source/name,arcname='source/'+name,recursive=False)
ready={'format_version':1,'driverPath':str(driver),'driverSha256':hashlib.sha256(driver.read_bytes()).hexdigest(),'payloadPath':str(run/'payload.tar.gz'),'payloadSha256':hashlib.sha256((run/'payload.tar.gz').read_bytes()).hexdigest(),'sourceManifestSha256':sha,'frozenSourceUnchangedAfterBuild':True,'workspaceManifestChanged':False,'buildInfo':info,'helperInputs':helper}
(run/'ready-driver.json').write_text(json.dumps(ready,indent=2)+'\n');print(json.dumps({'phase':'ready',**ready}),flush=True)
