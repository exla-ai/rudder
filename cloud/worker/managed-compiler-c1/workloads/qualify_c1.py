#!/usr/bin/env python3
"""Root-run no-network BUILD component, never a native SDK/Root grant.
Fresh copied chroot only; fixed UID1500 compiler inputs, no credentials/cache.
All original foreground Popen/pidfd/pipe owners remain until true settlement.
Unknown child/daemon effects require this exact build container/VM teardown.
"""
import argparse,ctypes,errno,hashlib,importlib.util,json,os,selectors,signal,stat,subprocess,time,fcntl,socket,struct,sys
from pathlib import Path

spec=importlib.util.spec_from_file_location('c1',Path(__file__).parents[1]/'compiler_c1.py');c1=importlib.util.module_from_spec(spec);spec.loader.exec_module(c1)
base=c1.base;owners=[];clock=None;root=None
ENV={'PATH':'/tools/rust/bin:/usr/bin:/bin:/tools','HOME':'/tmp/home','TMPDIR':'/tmp','LANG':'C','LC_ALL':'C','TZ':'UTC',
     'LD_LIBRARY_PATH':'/tools/rust/lib','RUSTC':'/tools/rust/bin/rustc','CARGO_HOME':'/tmp/cargo','CARGO_NET_OFFLINE':'true','SSL_CERT_FILE':'/etc/ssl/certs/ca-certificates.crt',
     'CC':'/usr/bin/gcc','CXX':'/usr/bin/g++','AR':'/usr/bin/ar','RUSTFLAGS':'-C linker=/usr/bin/gcc'}

def namespace():
    lib=ctypes.CDLL(None,use_errno=True)
    # Build component only: actual compiler/Buck2 needs /proc. Never mount in
    # the invoking/global namespace. Builder capability denial is a FAILURE;
    # Root can run the same fixture in an exact-owned disposable VM instead.
    if lib.unshare(0x00020000|0x40000000):raise OSError(ctypes.get_errno(),'C1 private mount/network namespace required')
    lib.mount.argtypes=[ctypes.c_char_p,ctypes.c_char_p,ctypes.c_char_p,ctypes.c_ulong,ctypes.c_char_p]
    if lib.mount(None,b'/',None,(1<<18)|16384,None):raise OSError(ctypes.get_errno(),'C1 private propagation required')
    if lib.mount(b'proc',os.fsencode(root/'proc'),b'proc',2|4|8,b'hidepid=2'):raise OSError(ctypes.get_errno(),'C1 private component proc required')
    with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as control:
        request=struct.pack("16sH14s",b"lo",0,b"" )
        current=fcntl.ioctl(control.fileno(),0x8913,request)
        flags=struct.unpack("16sH14s",current)[1]
        fcntl.ioctl(control.fileno(),0x8914,struct.pack("16sH14s",b"lo",flags|1,b""))
    print("C1_PRIVATE_JOB_NETWORK "+str(os.stat("/proc/self/ns/net").st_ino),flush=True)

def floor():
    lib=ctypes.CDLL(None,use_errno=True)
    os.chroot(root);os.chdir('/workspace')
    if lib.prctl(38,1,0,0,0):raise OSError(ctypes.get_errno(),'C1 NNP floor')
    for cap in range(64):
        if lib.prctl(24,cap,0,0,0) and ctypes.get_errno()!=errno.EINVAL:raise OSError(ctypes.get_errno(),'C1 bounding floor')
    os.setgroups([]);os.setresgid(1500,1500,1500);os.setresuid(1500,1500,1500)
    class Header(ctypes.Structure):_fields_=[('version',ctypes.c_uint32),('pid',ctypes.c_int)]
    class Data(ctypes.Structure):_fields_=[('effective',ctypes.c_uint32),('permitted',ctypes.c_uint32),('inheritable',ctypes.c_uint32)]
    header=Header(0x20080522,0);data=(Data*2)()
    if lib.capset(ctypes.byref(header),ctypes.byref(data)):raise OSError(ctypes.get_errno(),'C1 capability floor')

def run(label,argv):
    print("C1_WORKLOAD_START "+label,flush=True)
    clock.remaining();held={'label':label,'child':None,'pidfd':None,'stdout':bytearray(),'stderr':bytearray(),'status':None,'eof':set(),'kill_requested':False};owners.append(held)
    held['child']=subprocess.Popen.__new__(subprocess.Popen)
    held['child'].__init__(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,cwd='/',env=ENV,close_fds=True,preexec_fn=floor)
    child=held['child'];held['pidfd']=os.pidfd_open(child.pid,0);selector=selectors.DefaultSelector()
    for name,pipe in [('stdout',child.stdout),('stderr',child.stderr)]:os.set_blocking(pipe.fileno(),False);selector.register(pipe,selectors.EVENT_READ,name)
    overflow=False;expired=False
    while selector.get_map() or child.poll() is None:
        try:clock.remaining()
        except ValueError:
            expired=True
            if not held['kill_requested'] and child.poll() is None:signal.pidfd_send_signal(held['pidfd'],signal.SIGKILL);held['kill_requested']=True
        for key,_ in selector.select(.05):
            try:chunk=os.read(key.fileobj.fileno(),65536)
            except BlockingIOError:continue
            if not chunk:held['eof'].add(key.data);selector.unregister(key.fileobj);continue
            target=held[key.data]
            if len(target)+len(chunk)>1024*1024:overflow=True
            else:target.extend(chunk)
        held['status']=child.poll()
    held['status']=child.wait();selector.close()
    if expired or overflow or held['status']!=0 or held['eof']!={'stdout','stderr'}:
        print('C1_WORKLOAD_FAILURE '+label+' exit='+str(held['status']),file=sys.stderr,flush=True)
        sys.stderr.write(held['stderr'][-65536:].decode(errors='replace'));sys.stderr.flush()
    base.need(not expired and not overflow and held['status']==0 and held['eof']=={'stdout','stderr'},'actual C1 workload failed/expired/unsettled: '+label)
    return held['stdout'].decode()

def write(file,value):
    file.write_bytes(value);file.chmod(0o644);os.chown(file,1500,1500)

def main():
    global clock,root
    parser=argparse.ArgumentParser();parser.add_argument('--artifact',type=Path,required=True);parser.add_argument('--scratch',type=Path,required=True);args=parser.parse_args()
    base.need(os.getuid()==0 and os.getgid()==0 and args.artifact==Path('/out/rudder-compiler-c1'),'fixed Root C1 component invocation')
    base.need(args.scratch==Path('/workspace/compiler-c1-workload') and not args.scratch.exists(),'fresh fixed C1 scratch required')
    print("C1_PHASE artifact-verify",flush=True)
    clock=base.Clock();receipt=c1.verify(args.artifact,clock);manifest,_=base.read_json(args.artifact/'manifest.json')
    print("C1_PHASE artifact-copy",flush=True)
    args.scratch.mkdir(mode=0o700);copy=args.scratch/'copy';copy.mkdir(mode=0o700)
    # Copy and verify exact actual bytes into an independently owned test layer.
    # The canonical artifact source is never changed by compilation/cleanup.
    for entry in manifest['entries']:
        clock.remaining();path=entry['path'];base.relative(path);destination=copy/path
        if entry['kind']=='directory':destination.mkdir(mode=0o700)
        else:
            source=args.artifact/path;before=source.lstat();fd=os.open(source,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC);digest=hashlib.sha256();size=0
            with os.fdopen(fd,'rb') as src,destination.open('xb') as out:
                base.need(base.version(os.fstat(src.fileno()))==base.version(before),'C1 fixture copy source open changed')
                while data:=src.read(65536):clock.remaining();size+=len(data);base.need(size<=entry['size_bytes'],'C1 fixture grew');digest.update(data);out.write(data)
                base.need(base.version(os.fstat(src.fileno()))==base.version(before)==base.version(source.lstat()),'C1 fixture source changed')
            base.need(size==entry['size_bytes'] and digest.hexdigest()==entry['sha256'],'C1 copied fixture SHA differs');destination.chmod(entry['mode'])
    for directory,_,_ in os.walk(copy,topdown=False):Path(directory).chmod(0o555)
    root=copy/'prepared';work=root/'workspace';tmp=root/'tmp'
    for path in [work,tmp]:path.chmod(0o700);os.chown(path,1500,1500)
    (root/'dev').chmod(0o700);os.mknod(root/'dev/null',stat.S_IFCHR|0o666,os.makedev(1,3));(root/'dev/null').chmod(0o666);(root/'dev').chmod(0o555)
    for name in ['home','cargo']:(tmp/name).mkdir(mode=0o700);os.chown(tmp/name,1500,1500)
    # Actual component copy only; buck2 is outside prepared in the artifact.
    root.chmod(0o755);(root/'buck2').write_bytes((copy/'buck2').read_bytes());(root/'buck2').chmod(0o555);root.chmod(0o555)
    for source in (copy/'workloads').iterdir():
        if source.name=='qualify_c1.py':continue
        write(work/('.buckconfig' if source.name=='buckconfig' else source.name),source.read_bytes())
    write(work/'.buckroot',b'')
    namespace() # One original private job network, shared by compiler/daemon and shutdown.
    run('C-executable',['/usr/bin/gcc','-std=c11','hello.c','-o','c-main']);base.need(run('C-output',['/workspace/c-main'])=='c1-c-ok\n','C output')
    run('CXX-executable',['/usr/bin/g++','-std=c++17','hello.cpp','-o','cxx-main']);base.need(run('CXX-output',['/workspace/cxx-main'])=='c1-cxx-ok\n','CXX runtime output')
    run('C-static-object',['/usr/bin/gcc','-DC1_LIBRARY_ONLY','-c','hello.c','-o','hello.o']);run('C-archive',['/usr/bin/ar','rcs','libc1.a','hello.o'])
    run('Rust-executable',['/tools/rust/bin/rustc','--sysroot','/tools/rust','--edition=2021','-C','linker=/usr/bin/gcc','main.rs','-L','native=/workspace','-l','static=c1','-o','rust-main']);base.need(run('Rust-output',['/workspace/rust-main'])=='c1-rust-executable-ok\n','Rust executable output')
    run('Cargo-offline-build-script',['/tools/rust/bin/cargo','build','--offline']);base.need(run('Cargo-output',['/workspace/target/debug/c1-cargo'])=='c1-rust-executable-ok\n','Cargo executable output')
    run('SQLite-C-link',['/usr/bin/gcc','sqlite.c','-lsqlite3','-o','sqlite-main']);base.need(run('SQLite-output',['/workspace/sqlite-main'])=='c1-sqlite-ok\n','SQLite runtime output')
    run('TypeScript-compile',['/tools/node','/tools/typescript/lib/tsc.js','--strict','--skipLibCheck','--target','ES2022','--module','commonjs','--outDir','ts-out','main.ts']);base.need(run('TypeScript-output',['/tools/node','ts-out/main.js'])=='c1-typescript-ok\n','TS output')
    run('Buck2-CXX-build',['/buck2','build','//:cxx','--console','simple','--build-report','/workspace/buck-report.json'])
    report=json.loads((work/'buck-report.json').read_bytes());base.need(report.get('success') is True,'actual Buck2 build report')
    outputs=report['results']['root//:cxx']['outputs']['DEFAULT'];base.need(len(outputs)==1 and not Path(outputs[0]).is_absolute() and '..' not in Path(outputs[0]).parts and Path(outputs[0]).parts[0]=='buck-out','Buck2 output escaped')
    base.need(run('Buck2-CXX-output',['/workspace/'+outputs[0]])=='c1-cxx-ok\n','Buck2 compiled output')
    run('Buck2-shutdown-request',['/buck2','kill']) # request is NOT a daemon/Root-drain proof
    print(json.dumps({'format_version':1,'scope':'C1-fixed-build-container-functional-component-only','profile_id':c1.PROFILE,'artifact_manifest_sha256':receipt['manifest_sha256'],
      'foreground_children':[{'label':o['label'],'exit':o['status'],'stdout_eof':'stdout' in o['eof'],'stderr_eof':'stderr' in o['eof']} for o in owners],
      'rust_executable':True,'C_CXX':True,'cargo_offline_build_script':True,'system_sqlite_C':True,'Rudder_repository_build':False,'TypeScript':True,'Buck2_CXX_action':True,
      'third_party_cargo_cache_included':False,'physical_Root_Native_SDK_grant':False,'daemon_retirement_proven':False,'production_activation':False},sort_keys=True))
if __name__=='__main__':main()
