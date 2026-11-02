"""Platform mechanics only. No Fly execution, command budget or qualification."""
import importlib.machinery,importlib.util,json,os,sys,tempfile
assert len(sys.argv)==1 and sys.version_info >= (3,9)
def load(name,path):
 loader=importlib.machinery.SourceFileLoader(name,path);spec=importlib.util.spec_from_loader(name,loader)
 module=importlib.util.module_from_spec(spec);loader.exec_module(module);return module
# Same original readonly descriptors verified/retained by Node before spawn.
worker=load('fixed_worker','/proc/self/fd/3');project=load('fixed_project','/proc/self/fd/4')
owner=worker.OriginalLogs('00000000-0000-4000-8000-000000000000');worker.HELD.append(owner)
operations=[]
def sample(op,settlement=False):
 # Deliberately NOT an original-holder budget: this is an isolated platform test.
 owner.requests+=1;operations.append(op);assert op in worker.OPERATIONS;return True
owner.sample=sample
original=owner.child
with tempfile.TemporaryDirectory(prefix='rudder-genuine-target-probe-') as directory:
 os.chmod(directory,0o700)
 try:
  capture,out,err=owner.capture(['/usr/bin/python3','-I','-B','-c','print("RUDDER_LOCAL_PLATFORM_ONLY")'],directory)
  assert owner.child is original and worker.HELD[-1] is owner
  assert out==b'RUDDER_LOCAL_PLATFORM_ONLY\n' and err==b'' and capture['exit_code']==0
  assert all(capture[k] is True for k in ['constructor_completed','actual_child_wait','pidfd_held','stdout_eof','stderr_eof','raw_files_fsynced','raw_directory_fsynced'])
  assert not capture['expired'] and not capture['overflow'] and capture['errors']==[]
  project.capture_valid(capture,out,err)
  print(json.dumps(dict(schema=1,kind='ISOLATED_PLATFORM_MECHANICS_DATA',pythonProcFd=True,actualTinyPidfd=True,actualTinyWait=True,bothTinyEof=True,samePreallocatedChild=True,rawFsync=True,originalBudgetTested=False,flyInvoked=False,genuineIngress=False,qualified=False),separators=(',',':')),flush=True)
 except BaseException:
  if owner.spawn_entered and not (owner.actual_child_wait and all(owner.eof.values())):owner.hold_unknown('platform_probe_unknown')
  raise
