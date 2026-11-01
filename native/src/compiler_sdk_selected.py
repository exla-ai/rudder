"""Fixed A2 selected-source SDK runner. No shell, env, installer or model IO."""
import os,sys,json,time,pathlib,hashlib,subprocess,selectors,ctypes
os.environ.clear()
rules=json.loads(pathlib.Path('/workspace/rules.json').read_text())
os.umask(rules['umask'])
if ctypes.CDLL(None,use_errno=True).prctl(4,0,0,0,0)!=0:raise RuntimeError('SDK_ND0')
deadline=int(sys.argv[1])/1e9
# The original helper supplied a sealed read-only memfd as observer stdin.
# Consume its bounded DATA, then replace fd0 with an empty pipe. No request
# descriptor or bytes survive in the compiler's stdio/argv/env/filesystem.
if len(sys.argv)!=3:raise RuntimeError('SDK_OBSERVER_FIXED_ARGUMENTS')
observer_proc=SdkObserverProc(int(sys.argv[2]))
job=consume_sdk_observer_job()
input_policy=SdkCompilerReadPolicy(rules['compiler_input_policy'],observer_proc)
def remaining():
    n=deadline-time.clock_gettime(time.CLOCK_BOOTTIME)
    if n<=0:raise RuntimeError('SDK_ORIGINAL_BUILD_EXPIRED')
    return n
def relative(value):
    p=pathlib.PurePosixPath(value)
    if not value or p.is_absolute()or any(c in ('','..','.')for c in value.split('/'))or len(value)>1024:raise RuntimeError('SDK_SOURCE_PATH')
    return p
children=[]
def emit_compiler_failure_output(stdout,stderr,fd=2):
    # Secondary private DATA only. Bounded prefixes preserve real bytes as
    # UTF8/escaped JSON, with whole-stream counts/hashes and explicit clipping.
    # One nonblocking write, no retry or signal-disposition change. A failed
    # diagnostic cannot replace the original compiler failure.
    import fcntl,signal,stat
    previous=None
    try:
        record={'format_version':1,'kind':'sdk-compiler-output-prefix-data-v1',
            'stdout_bytes':len(stdout),'stderr_bytes':len(stderr),
            'stdout_sha256':hashlib.sha256(stdout).hexdigest(),
            'stderr_sha256':hashlib.sha256(stderr).hexdigest(),
            'stdout_prefix_utf8':bytes(stdout[:128]).decode('utf-8','replace'),
            'stderr_prefix_utf8':bytes(stderr[:256]).decode('utf-8','replace'),
            'stdout_truncated':len(stdout)>128,'stderr_truncated':len(stderr)>256}
        raw=json.dumps(record,sort_keys=True,ensure_ascii=True,separators=(',',':')).encode('ascii')+b'\n'
        if len(raw)>3072:return
        kind=stat.S_IFMT(os.fstat(fd).st_mode)
        if kind not in (stat.S_IFREG,stat.S_IFIFO,stat.S_IFSOCK):return
        if kind!=stat.S_IFREG and signal.getsignal(signal.SIGPIPE)!=signal.SIG_IGN:return
        previous=fcntl.fcntl(fd,fcntl.F_GETFL)
        if previous&os.O_ACCMODE==os.O_RDONLY:return
        fcntl.fcntl(fd,fcntl.F_SETFL,previous|os.O_NONBLOCK)
        os.write(fd,raw)
    except BaseException:pass
    finally:
        if previous is not None:
            try:fcntl.fcntl(fd,fcntl.F_SETFL,previous)
            except BaseException:pass

def run(argv):
    remaining();child=subprocess.Popen.__new__(subprocess.Popen);children.append(child)
    child.__init__(argv,cwd=rules['source_root'],env=rules['environment'],stdin=0,stdout=subprocess.PIPE,stderr=subprocess.PIPE,close_fds=True,
        preexec_fn=input_policy.before_exec)
    output={child.stdout:bytearray(),child.stderr:bytearray()};selector=selectors.DefaultSelector()
    for stream in output:os.set_blocking(stream.fileno(),False);selector.register(stream,selectors.EVENT_READ)
    while selector.get_map():
        for key,_ in selector.select(min(remaining(),.05)):
            block=os.read(key.fileobj.fileno(),16384)
            if not block:selector.unregister(key.fileobj);continue
            output[key.fileobj].extend(block)
            if len(output[key.fileobj])>65536:raise RuntimeError('SDK_PIPE_BOUND')
    child.wait(timeout=remaining())
    if child.returncode:
        emit_compiler_failure_output(output[child.stdout],output[child.stderr])
        raise RuntimeError('SDK_ACTUAL_COMPILER_FAILURE')
    remaining();return {'command':argv,'working_directory':rules['source_root'],'environment':rules['environment'],'exit_code':child.returncode,'stdout_sha256':hashlib.sha256(output[child.stdout]).hexdigest(),'stderr_sha256':hashlib.sha256(output[child.stderr]).hexdigest(),
        'input_confinement':input_policy.completed_spawn_data()}
start=time.clock_gettime_ns(time.CLOCK_BOOTTIME);checks=[]
for target in job['profile']['targets']:
    name=target['name']
    if not name or not name.replace('_','').isalnum()or len(name)>128:raise RuntimeError('SDK_TARGET_NAME')
    entry=relative(target['entrypoint'])
    if target['entrypoint']not in target['sources']:raise RuntimeError('SDK_UNDECLARED_ENTRY')
    for source in target['sources']:relative(source)
    destination=pathlib.Path(rules['output_root'])/name;destination.mkdir()
    if target['language']=='rust':
        result=run([arg.replace('{entrypoint}',str(entry)).replace('{output}',str(destination/rules['rust_output']))for arg in rules['rust_argv']])
    elif target['language']=='typescript':
        result=run([arg.replace('{entrypoint}',str(entry)).replace('{output}',str(destination))for arg in rules['typescript_argv']])
    else:raise RuntimeError('SDK_UNSUPPORTED_A2_TARGET')
    checks.append({'target':name,'language':target['language'],**result})
remaining();exe=observer_proc.stat('self/exe');root=os.stat('/')
raw=observer_proc.read('self/exe',64*1024*1024)
status=dict(line.split(':',1)for line in observer_proc.read('self/status',64*1024).decode().splitlines()if':'in line)
caps={k:status[k].strip()for k in ['CapEff','CapPrm','CapInh','CapAmb','CapBnd']}
if exe.st_dev!=root.st_dev or not os.statvfs('/').f_flag&os.ST_RDONLY or any(int(v,16)for v in caps.values()):raise RuntimeError('SDK_PROVIDER_LAYOUT')
print(json.dumps({'format_version':1,'kind':'actual-selected-sdk-build-v1','action_key':job['action_key'],'source_root':job['source_root'],
    'sdk_python':{'device':str(exe.st_dev),'inode':str(exe.st_ino),'sha256':hashlib.sha256(raw).hexdigest(),'size_bytes':len(raw)},
    'sdk_root_device':str(root.st_dev),'sdk_root_readonly':True,'compiler_capabilities':caps,
    'user_namespace':observer_proc.readlink('self/ns/user'),'mount_namespace':observer_proc.readlink('self/ns/mnt'),'pid_namespace':observer_proc.readlink('self/ns/pid'),'network_namespace':observer_proc.readlink('self/ns/net'),
    'started_boottime_nanoseconds':str(start),'completed_boottime_nanoseconds':str(time.clock_gettime_ns(time.CLOCK_BOOTTIME)),
    'checks':checks,'buck2_frontend_executed':False,'models':False,'source_authority':False,'normal_retirement_authority':False},sort_keys=True,separators=(',',':')),flush=True)
