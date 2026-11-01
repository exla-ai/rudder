"""Explicit actual-kernel component fixtures, UNRUN by source author.
No Root/SDK role or compiler outcome is manufactured. Private /workspace only.
"""
import ctypes
import errno
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import select
import resource
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import traceback
import unittest

spec=importlib.util.spec_from_file_location('sdk_confinement',Path(__file__).with_name('compiler_sdk_confinement.py'))
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
libc=ctypes.CDLL(None,use_errno=True);libc.syscall.restype=ctypes.c_long

def require(value,message):
    if not value:raise RuntimeError(message)

def mount(source,target,kind=None,flags=0):
    def encoded(value):return None if value is None else os.fsencode(value)
    require(libc.mount(encoded(source),encoded(target),encoded(kind),ctypes.c_ulong(flags),None)==0,
            'actual fixture mount errno='+str(ctypes.get_errno()))

def drop_caps():
    require(libc.prctl(38,1,0,0,0)==0,'fixture NNP')
    require(libc.prctl(28,1|2|4|8,0,0,0)==0,'fixture securebits')
    for cap in range(64):
        require(libc.prctl(24,cap,0,0,0)==0 or ctypes.get_errno()==errno.EINVAL,'fixture bounding drop')
    class Header(ctypes.Structure):_fields_=[('version',ctypes.c_uint32),('pid',ctypes.c_int32)]
    class Data(ctypes.Structure):_fields_=[('effective',ctypes.c_uint32),('permitted',ctypes.c_uint32),('inheritable',ctypes.c_uint32)]
    number=126 if module._sc_platform.machine()=='x86_64' else 91
    header=Header(0x20080522,0);data=(Data*2)()
    require(libc.syscall(number,ctypes.byref(header),ctypes.byref(data))==0,'fixture capset')
    require(libc.prctl(4,0,0,0,0)==0,'fixture ND0')

def filter_probe_and_fork():
    require(libc.prctl(38,1,0,0,0)==0,'fixture NNP')
    fence=module._CompilerSeccomp(module.SDK_COMPILER_INPUT_POLICY);fence.install();fence.probe()
    # Real threads exercise glibc's clone3->clone route beneath the same fence.
    ran=[];worker=threading.Thread(target=lambda:ran.append('actual-thread'));worker.start();worker.join()
    require(not worker.is_alive() and ran==['actual-thread'],'ordinary thread blocked')
    child=os.fork()
    if child==0:
        libc.prctl(1,signal.SIGKILL,0,0,0)
        fence.probe();os._exit(0)
    actual,status=os.waitpid(child,0);require(actual==child and os.waitstatus_to_exitcode(status)==0,'descendant filter')
    return {'actual_filter':True,'normal_thread':True,'normal_fork':True,'descendant_filter':True}

def namespace_child(root,old_proc):
    # This executes in the actual first child after CLONE_NEWPID, before chroot.
    require(os.getpid()==1,'fixture is not actual fresh PID1')
    try:module.SdkObserverProc(old_proc)
    except RuntimeError as error:require('FRESH_PID_MOUNT' in str(error),'old proc failed for another reason')
    else:raise AssertionError('parent old-PID proc mount was accepted')
    os.close(old_proc)
    mount('proc',root/'observer-proc','proc',2|4|8)
    proc_fd=os.open(root/'observer-proc',os.O_PATH|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
    observer=module.SdkObserverProc(proc_fd)
    view=root/'view';mount(view,view,flags=4096);mount(None,view,flags=4096|32|1|2|4)
    origin=root/'workspace'
    mount(root/'source',origin/'src',flags=4096);mount(None,origin/'src',flags=4096|32|1|2|4|8)
    mount(root/'out',origin/'out',flags=4096);mount(None,origin/'out',flags=4096|32|2|4|8)
    mount(root/'tool-image',origin/'buck2',flags=4096);mount(None,origin/'buck2',flags=4096|32|1|2|4)
    workspace=view/'workspace';mount(origin,workspace,flags=4096|16384)
    mount(None,workspace,flags=4096|32|1|2|4|8)
    mount(root/'tmp',view/'tmp',flags=4096);mount(None,view/'tmp',flags=4096|32|2|4|8)
    os.chroot(view);os.chdir('/workspace');drop_caps()
    # The real proc is inaccessible by path yet reachable by the observer's FD.
    require(b'CapEff:\t0000000000000000' in observer.read('self/status',65536),'actual caps0 absent')
    policy=module.SdkCompilerReadPolicy(module.SDK_COMPILER_INPUT_POLICY,observer)
    require(Path('/workspace/src/input').read_bytes()==b'immutable-input','positive source read')
    require(Path('/workspace/buck2').read_bytes()==b'OS fixture only; not a compiler or qualified SDK','actual nested tool bind omitted')
    try:open('/proc/sys/kernel/random/uuid','rb')
    except OSError as error:require(error.errno==errno.ENOENT,'actual proc not absent')
    else:raise AssertionError('proc UUID visible')
    # Component only: invoke the genuine compiler pre-exec mechanism on this
    # disposable process. No actual exec/outcome/Root receipt is claimed here.
    policy.before_exec()
    try:os.fstat(proc_fd)
    except OSError as error:require(error.errno==errno.EBADF,'observer FD not closed')
    else:raise AssertionError('observer proc FD leaked')
    for path in ('/workspace/job.json','/workspace/src/forbidden'):
        try:open(path,'wb')
        except OSError as error:require(error.errno==errno.EROFS,'readonly parent/source not enforced')
        else:raise AssertionError('readonly view was writable')
    for base in ('/workspace/out','/tmp'):
        p=Path(base)/'actual-output';p.write_bytes(b'private');require(p.read_bytes()==b'private','private output')
    for flags in (0x10000000,0x00020000):
        ctypes.set_errno(0);value=libc.syscall(policy.filter.numbers['clone'],flags|signal.SIGCHLD,0,0,0,0)
        if value==0:os._exit(44)
        if value>0:os.waitpid(value,0)
        require(value==-1 and ctypes.get_errno()==errno.EPERM,'namespace clone escaped filter')
    child=os.fork()
    if child==0:
        policy.filter.probe()
        try:open('/workspace/job.json','rb')
        except OSError as error:os._exit(0 if error.errno==errno.ENOENT else 4)
        os._exit(5)
    actual,status=os.waitpid(child,0);require(actual==child and os.waitstatus_to_exitcode(status)==0,'descendant view/fence')
    print(json.dumps({'actual_fresh_pid_proc_fd':True,'old_proc_refused':True,'observer_caps0':True,'compiler_proc_absent':True,
        'observer_fd_closed':True,'readonly_parents':True,'private_writes':True,'descendant_fence':True}),flush=True)

def namespace_worker(root):
    require(os.getuid()==1500 and os.geteuid()==1500 and os.getgid()==1500,'fixture requires actual nonRoot UID1500')
    old_proc=os.open('/proc',os.O_PATH|os.O_DIRECTORY|os.O_CLOEXEC)
    require(libc.unshare(0x10000000|0x00020000|0x40000000|0x20000000)==0,'actual namespaces unavailable')
    Path('/proc/self/setgroups').write_text('deny')
    Path('/proc/self/uid_map').write_text('0 1500 1\n');Path('/proc/self/gid_map').write_text('0 1500 1\n')
    mount(None,'/',flags=16384|262144)
    parent=os.pidfd_open(os.getpid(),0)
    child=os.fork()
    if child==0:
        require(libc.prctl(1,signal.SIGKILL,0,0,0)==0,'fixture parent death fence')
        watch=select.poll();watch.register(parent,select.POLLIN)
        if watch.poll(0):os._exit(20)
        os.close(parent)
        try:namespace_child(root,old_proc)
        except BaseException:
            traceback.print_exc();os._exit(21)
        os._exit(0)
    os.close(parent);os.close(old_proc)
    actual,status=os.waitpid(child,0);require(actual==child,'fixture actual init wait differs')
    return os.waitstatus_to_exitcode(status)

class ActualKernelComponent(unittest.TestCase):
    def run_worker(self,*args,stdin=subprocess.DEVNULL,expected=0):
        child=subprocess.Popen([sys.executable,'-I',str(Path(__file__).resolve()),*args],
            stdin=stdin,stdout=subprocess.PIPE,stderr=subprocess.PIPE,close_fds=True,start_new_session=True)
        try:out,err=child.communicate(timeout=15)
        except BaseException:
            try:os.killpg(child.pid,signal.SIGKILL)
            except ProcessLookupError:pass
            child.communicate(timeout=3);raise
        self.assertEqual(child.returncode,expected,err.decode(errors='replace'))
        return json.loads(out) if expected==0 else out

    def test_real_seccomp_namespace_denials_and_regular_thread_fork_inheritance(self):
        self.assertEqual(self.run_worker('--filter-worker'),{
            'actual_filter':True,'normal_thread':True,'normal_fork':True,'descendant_filter':True})

    def test_actual_procless_readonly_view_and_observer_only_descriptor(self):
        root=Path(tempfile.mkdtemp(prefix='rudder-sdk-procless-',dir='/workspace')).resolve()
        for name in ('view','view/proc','view/workspace','view/tmp','workspace','workspace/src','workspace/out','source','out','tmp','observer-proc'):
            (root/name).mkdir(mode=0o700)
        (root/'workspace/rules.json').write_text('{}')
        (root/'workspace/buck2').write_bytes(b'')
        (root/'tool-image').write_bytes(b'OS fixture only; not a compiler or qualified SDK')
        (root/'source/input').write_bytes(b'immutable-input')
        try:
            actual=self.run_worker('--namespace-worker',str(root))
            self.assertTrue(all(actual.values()));self.assertEqual(len(actual),8)
            self.assertEqual((root/'out/actual-output').read_bytes(),b'private')
            self.assertEqual((root/'tmp/actual-output').read_bytes(),b'private')
        except BaseException:
            print('Retained original failed fixture '+str(root),file=sys.stderr);raise
        else:shutil.rmtree(root)

    def test_actual_x32_syscall_bit_is_killed_before_number_matching(self):
        if module._sc_platform.machine()=='x86_64':
            self.assertEqual(self.run_worker('--x32-worker',expected=-signal.SIGSYS),b'')
        else:
            self.assertEqual(module._sc_platform.machine(),'aarch64')
            self.assertEqual(module.SDK_COMPILER_INPUT_POLICY['architectures']['aarch64']['audit_arch'],0xc00000b7)

    def test_policy_change_cannot_enable_ambient_proc_or_descriptor_inheritance(self):
        changed=dict(module.SDK_COMPILER_INPUT_POLICY,compiler_proc='ambient-proc')
        with self.assertRaisesRegex(RuntimeError,'FIXED_VIEW_POLICY_DIFFERS'):
            module.SdkCompilerReadPolicy(changed,None)

    def test_real_sealed_request_is_replaced_by_empty_pipe(self):
        raw=os.memfd_create('test-only-observer-data',os.MFD_CLOEXEC|os.MFD_ALLOW_SEALING)
        try:
            os.write(raw,b'{"private_test_data":"never-compiler-stdin"}');fcntl.fcntl(raw,fcntl.F_ADD_SEALS,15)
            read=os.open('/proc/self/fd/'+str(raw),os.O_RDONLY|os.O_CLOEXEC)
            try:self.assertEqual(self.run_worker('--stdin-worker',stdin=read),{'actual_empty_pipe':True})
            finally:os.close(read)
        finally:os.close(raw)

if __name__=='__main__':
    if len(sys.argv)>1 and sys.argv[1]=='--filter-worker':
        print(json.dumps(filter_probe_and_fork()),flush=True)
    elif len(sys.argv)>1 and sys.argv[1]=='--x32-worker':
        require(module._sc_platform.machine()=='x86_64','x32 fixture is x86_64 only')
        resource.setrlimit(resource.RLIMIT_CORE,(0,0))
        require(libc.prctl(38,1,0,0,0)==0,'fixture NNP')
        fence=module._CompilerSeccomp(module.SDK_COMPILER_INPUT_POLICY);fence.install()
        # Harmless unshare(0) if a broken fence accidentally admits this ABI.
        libc.syscall(fence.numbers['denied_syscalls']['unshare']|0x40000000,0)
        raise AssertionError('x32 syscall was not killed')
    elif len(sys.argv)>1 and sys.argv[1]=='--namespace-worker':
        sys.exit(namespace_worker(Path(sys.argv[2])))
    elif len(sys.argv)>1 and sys.argv[1]=='--stdin-worker':
        job=module.consume_sdk_observer_job();require(job=={'private_test_data':'never-compiler-stdin'},'actual request differs')
        require(os.read(0,1)==b'' and module._sc_stat.S_ISFIFO(os.fstat(0).st_mode),'actual EOF pipe absent')
        print(json.dumps({'actual_empty_pipe':True}),flush=True)
    else:unittest.main()
