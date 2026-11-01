#!/usr/bin/env python3
"""Root-owned exact Child observer for a disposable component fixture ONLY.
No saved PID attach, detached IO or normal managed runtime retirement claim.
"""
import hashlib,json,os,selectors,signal,subprocess,sys,time
from pathlib import Path
node,test,state_file,output=map(Path,sys.argv[1:])
state=json.loads(state_file.read_text());work=Path(state['work']);out=Path(output)
assert state_file.parent==work and work.name=='rudder-managed-storage-pg-'+state['nonce']
assert out==work/'evidence' and (work/'qualification-nonce').read_text().strip()==state['nonce']
start=time.clock_gettime(time.CLOCK_BOOTTIME);deadline=start+150
child=None;pidfd=None;selector=None;receipt={};status=None;proof=None;requested=False;kill_requested=False;failed=False
try:
    child=subprocess.Popen(['/usr/local/bin/gosu','postgres',str(node),str(test)],stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
        env={'RUDDER_MANAGED_STORAGE_PG_STATE':str(state_file)},cwd=work)
    # Retain Popen BEFORE any later pidfd/pipe/proof work can fail.
    pidfd=os.pidfd_open(child.pid,0);selector=selectors.DefaultSelector();contents={child.stdout:bytearray(),child.stderr:bytearray()}
    for stream in contents:os.set_blocking(stream.fileno(),False);selector.register(stream,selectors.EVENT_READ)
    while selector.get_map() or child.poll() is None:
        now=time.clock_gettime(time.CLOCK_BOOTTIME)
        if proof is None:
            try:
                raw=(work/'node-proof.json').read_bytes()
                if len(raw)>262144:raise ValueError('proof bound')
                proof=json.loads(raw)
                if proof.get('qualification')!=state['nonce'] or proof.get('scope')!='managed-home-v1-v2-v3-real-PG-component-only' or proof.get('ok') is not True:failed=True
            except FileNotFoundError:pass
            except (ValueError,json.JSONDecodeError):failed=True
        if not requested and (proof is not None or failed or now>=deadline):
            requested=True;failed|=now>=deadline;signal.pidfd_send_signal(pidfd,signal.SIGTERM)
            receipt['fixture_teardown_requested']=True;receipt['normal_runtime_retirement']=False;receipt['teardown_at_boottime']=now
        if requested and not kill_requested and child.poll() is None and now>=receipt['teardown_at_boottime']+5:
            kill_requested=True;signal.pidfd_send_signal(pidfd,signal.SIGKILL)
        for event,_ in selector.select(0.05):
            b=os.read(event.fileobj.fileno(),4096)
            if not b:selector.unregister(event.fileobj);continue
            if len(contents[event.fileobj])+len(b)>1048576:failed=True
            else:contents[event.fileobj].extend(b)
    status=child.wait()
    receipt.update(actual_child_wait=True,stdout_eof=True,stderr_eof=True,exit_code=status,forced_kill_requested=kill_requested,
        node_stdout_sha256=hashlib.sha256(contents[child.stdout]).hexdigest(),node_stderr_sha256=hashlib.sha256(contents[child.stderr]).hexdigest(),
        component_only=True,realRootRegistration=False,productionStartup=False,models=False)
    for stream,name in [(child.stdout,'node-stdout.private.log'),(child.stderr,'node-stderr.private.log')]:
        with open(out/name,'xb') as f:os.chmod(f.fileno(),0o600);f.write(contents[stream]);os.fsync(f.fileno())
    # Retain the actual failed proof as well as successful evidence before an
    # assertion can return control to the cluster/VM fixture teardown.
    if proof is not None:
        with open(out/'node-proof.json','xb') as f:os.chmod(f.fileno(),0o600);f.write((json.dumps(proof,indent=2)+'\n').encode());os.fsync(f.fileno())
    assert proof and proof.get('ok') is True and requested and status==-signal.SIGTERM and not failed and not kill_requested
finally:
    # An uninterruptible child can keep this exact owner charged. No external
    # timeout or signal request is treated as actual process/pipe settlement.
    if child is not None:
        if child.poll() is None:child.kill()
        child.wait()
        for stream in [child.stdout,child.stderr]:
            if stream is not None:stream.close()
    if selector is not None:selector.close()
    if pidfd is not None:os.close(pidfd)
    with open(out/'node-lifecycle.json','x') as f:os.chmod(f.fileno(),0o600);json.dump(receipt,f,indent=2);f.write('\n')
