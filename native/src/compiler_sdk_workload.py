"""Fixed SDK qualification only, inside its original UID1500 helper namespace.
No model, account, network, project or caller-selected command is accepted.
"""
import os,sys,json,time,hashlib,subprocess,pathlib,selectors,fcntl,ctypes,struct
os.environ.clear()
os.umask(0o022)
if ctypes.CDLL(None,use_errno=True).prctl(4,0,0,0,0)!=0:raise RuntimeError('SDK_POSTEXEC_ND0')
deadline=int(sys.argv[1])/1_000_000_000
if len(sys.argv)!=2 or os.getuid()!=0 or os.getgid()!=0:
    raise RuntimeError('SDK_QUALIFICATION_NAMESPACE')
def remaining():
    value=deadline-time.clock_gettime(time.CLOCK_BOOTTIME)
    if value<=0:raise RuntimeError('SDK_ORIGINAL_ENABLE_EXPIRED')
    return value
children=[]
def run(argv,expected=None):
    remaining()
    child=subprocess.Popen.__new__(subprocess.Popen)
    children.append(child)
    child.__init__(argv,cwd='/workspace',env={'LC_ALL':'C','LANG':'C','TZ':'UTC','HOME':'/tmp','TMPDIR':'/tmp','PATH':'/tools/rust/bin:/tools:/usr/bin:/bin','PYTHONDONTWRITEBYTECODE':'1'},
        stdin=None,stdout=subprocess.PIPE,stderr=subprocess.PIPE,close_fds=True)
    buffers={child.stdout:bytearray(),child.stderr:bytearray()};selector=selectors.DefaultSelector()
    for stream in buffers:
        os.set_blocking(stream.fileno(),False);selector.register(stream,selectors.EVENT_READ)
    while selector.get_map():
        for key,_ in selector.select(min(remaining(),0.05)):
            block=os.read(key.fileobj.fileno(),16384)
            if not block:selector.unregister(key.fileobj);continue
            buffers[key.fileobj].extend(block)
            if len(buffers[key.fileobj])>65536:raise RuntimeError('SDK_ACTUAL_TOOL_OUTPUT_BOUND')
    child.wait(timeout=remaining());out=bytes(buffers[child.stdout]);err=bytes(buffers[child.stderr])
    if child.returncode!=0:raise RuntimeError('SDK_ACTUAL_TOOL_EXIT')
    remaining()
    if expected is not None and out!=expected:raise RuntimeError('SDK_ACTUAL_TOOL_OUTPUT')
    return {'exit_code':child.returncode,'stdout_sha256':hashlib.sha256(out).hexdigest(),'stderr_sha256':hashlib.sha256(err).hexdigest(),'stdout_bytes':len(out),'stderr_bytes':len(err)}
start=time.clock_gettime_ns(time.CLOCK_BOOTTIME)
exe=os.stat('/proc/self/exe');named=os.stat('/tools/python/bin/python3');root=os.stat('/')
if(exe.st_dev,exe.st_ino)!=(named.st_dev,named.st_ino)or exe.st_dev!=root.st_dev:raise RuntimeError('SDK_ACTUAL_PYTHON_FLOOR')
with open('/proc/self/exe','rb')as f:
    data=f.read(64*1024*1024+1)
    if len(data)>64*1024*1024:raise RuntimeError('SDK_PYTHON_ELF_BOUND')
    python_exe={'device':str(exe.st_dev),'inode':str(exe.st_ino),'sha256':hashlib.sha256(data).hexdigest(),'size_bytes':len(data)}
flags=os.statvfs('/').f_flag
if not flags&os.ST_RDONLY:raise RuntimeError('SDK_PREPARED_ROOT_NOT_READONLY')
with open('/workspace/buck2','rb')as f:buck2_raw=f.read(65536)
if len(buck2_raw)<64 or buck2_raw[:6]!=b'\x7fELF\x02\x01':raise RuntimeError('SDK_BUCK2_ELF64')
phoff=struct.unpack_from('<Q',buck2_raw,32)[0];size,count=struct.unpack_from('<HH',buck2_raw,54)
if size<56 or count>256 or phoff+size*count>len(buck2_raw):raise RuntimeError('SDK_BUCK2_HEADER_BOUND')
if any(struct.unpack_from('<I',buck2_raw,phoff+i*size)[0]==3 for i in range(count)):raise RuntimeError('SDK_BUCK2_NOT_STATIC')
buck2=run(['/workspace/buck2','--version'])
status=dict(line.split(':',1)for line in pathlib.Path('/proc/self/status').read_text().splitlines()if ':'in line)
caps={key:status[key].strip()for key in ['CapEff','CapPrm','CapInh','CapAmb','CapBnd']}
if any(int(value,16)!=0 for value in caps.values()):raise RuntimeError('SDK_COMPILER_RETAINS_CAPABILITIES')
pathlib.Path('/workspace/main.rs').write_text('pub fn rudder_sdk_value() -> u32 { 42 }\n')
pathlib.Path('/workspace/check.rs').write_text('extern crate rudder_sdk; pub fn check() -> u32 { rudder_sdk::rudder_sdk_value() }\n')
pathlib.Path('/workspace/main.ts').write_text('const value: string = "rudder-sdk-typescript-ok"; console.log(value);\n')
rust=run(['/tools/rust/bin/rustc','--sysroot','/tools/rust','--crate-type=rlib','--crate-name','rudder_sdk','--edition=2021','--remap-path-prefix=/workspace=.','-C','target-cpu=generic','/workspace/main.rs','-o','/workspace/librudder_sdk.rlib'])
rust_result=run(['/tools/rust/bin/rustc','--sysroot','/tools/rust','--crate-type=rlib','--emit=metadata','--edition=2021','--extern','rudder_sdk=/workspace/librudder_sdk.rlib','/workspace/check.rs','-o','/workspace/check.rmeta'],b'')
if pathlib.Path('/workspace/librudder_sdk.rlib').read_bytes()[:8]!=b'!<arch>\n' or not pathlib.Path('/workspace/check.rmeta').stat().st_size:raise RuntimeError('SDK_ACTUAL_RUST_LIBRARY')
typescript=run(['/tools/node','/tools/typescript/lib/tsc.js','--pretty','false','--strict','--target','ES2020','--outDir','/workspace/js','/workspace/main.ts'])
typescript_result=run(['/tools/node','/workspace/js/main.js'],b'rudder-sdk-typescript-ok\n')
remaining()
print(json.dumps({'format_version':1,'kind':'actual-fixed-rust-library-typescript-sdk-workload-v1','network_namespace':os.readlink('/proc/self/ns/net'),
    'user_namespace':os.readlink('/proc/self/ns/user'),'mount_namespace':os.readlink('/proc/self/ns/mnt'),'pid_namespace':os.readlink('/proc/self/ns/pid'),
    'started_boottime_nanoseconds':str(start),'completed_boottime_nanoseconds':str(time.clock_gettime_ns(time.CLOCK_BOOTTIME)),
    'sdk_python':python_exe,'compiler_capabilities':caps,'buck2_static':True,'sdk_root_device':str(root.st_dev),'sdk_root_readonly':True,'buck2':buck2,'rust':rust,'rust_result':rust_result,'typescript':typescript,'typescript_result':typescript_result,
    'rust_artifact_sha256':hashlib.sha256(pathlib.Path('/workspace/librudder_sdk.rlib').read_bytes()).hexdigest(),
    'typescript_artifact_sha256':hashlib.sha256(pathlib.Path('/workspace/js/main.js').read_bytes()).hexdigest(),
    'models':False,'source_authority':False,'normal_retirement_authority':False},sort_keys=True,separators=(',',':')),flush=True)
