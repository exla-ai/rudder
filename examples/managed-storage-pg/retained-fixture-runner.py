#!/usr/bin/env python3
"""Root-owned exact Child observer for a disposable component fixture ONLY.
No saved PID attach, detached IO or normal managed runtime retirement claim.
"""
import hashlib,json,os,re,selectors,signal,subprocess,sys,time
from pathlib import Path
node,test,state_file,output=map(Path,sys.argv[1:5])
variant=sys.argv[5] if len(sys.argv)==6 else "managed-storage-v3"
assert variant in ("managed-storage-v3","provider-owner-v1","role-activation-v1")
provider_owner=variant=="provider-owner-v1"
role_activation_only=variant=="role-activation-v1"
assert test.name==("provider-fenced-ownership-pg.test.mjs" if provider_owner else "managed-storage-pg-qualification.mjs")
state=json.loads(state_file.read_text());work=Path(state['work']);out=Path(output)
assert state_file.parent==work and work.name=='rudder-managed-storage-pg-'+state['nonce']
assert out==work/'evidence' and (work/'qualification-nonce').read_text().strip()==state['nonce']
assert state.get('fixtureVariant','managed-storage-v3')==variant
assert state.get('roleActivationOnly',False)==role_activation_only
if role_activation_only:assert state.get('selectedRole') in ('sdk','runtime')
else:assert 'selectedRole' not in state
expected_scope='provider-owner-real-PG-component-only' if provider_owner else 'managed-home-v1-v2-v3-real-PG-component-only'
start=time.clock_gettime(time.CLOCK_BOOTTIME);deadline=start+150
child=None;pidfd=None;selector=None;receipt={};status=None;proof=None;requested=False;kill_requested=False;failed=False
try:
    child=subprocess.Popen(['/usr/local/bin/gosu','postgres',str(node),str(test)],stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
        env={('RUDDER_PROVIDER_OWNER_PG_TEST_STATE' if provider_owner else 'RUDDER_MANAGED_STORAGE_PG_STATE'):str(state_file)},cwd=work)
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
                if proof.get('qualification')!=state['nonce'] or proof.get('scope')!=expected_scope or proof.get('ok') is not True:failed=True
                if role_activation_only and (proof.get('roleActivationOnly') is not True or proof.get('selectedRole')!=state['selectedRole']):failed=True
            except FileNotFoundError:pass
            except (ValueError,json.JSONDecodeError):failed=True
        if not requested and (now>=deadline or not provider_owner and (proof is not None or failed)):
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
    # The finite provider test exits naturally; its proof can appear between
    # the last poll and both pipe EOF. Re-read only this original owned file.
    if provider_owner and proof is None:
        try:
            raw=(work/'node-proof.json').read_bytes()
            if len(raw)>262144:raise ValueError('proof bound')
            proof=json.loads(raw)
            if proof.get('qualification')!=state['nonce'] or proof.get('scope')!=expected_scope or proof.get('ok') is not True:failed=True
        except (FileNotFoundError,ValueError,json.JSONDecodeError):failed=True
    receipt.update(actual_child_wait=True,stdout_eof=True,stderr_eof=True,exit_code=status,forced_kill_requested=kill_requested,
        node_stdout_sha256=hashlib.sha256(contents[child.stdout]).hexdigest(),node_stderr_sha256=hashlib.sha256(contents[child.stderr]).hexdigest(),
        component_only=True,realRootRegistration=False,productionStartup=False,models=False,fixture_variant=variant,fixture_teardown_requested=requested)
    for stream,name in [(child.stdout,'node-stdout.private.log'),(child.stderr,'node-stderr.private.log')]:
        with open(out/name,'xb') as f:os.chmod(f.fileno(),0o600);f.write(contents[stream]);os.fsync(f.fileno())
    # Retain the actual failed proof as well as successful evidence before an
    # assertion can return control to the cluster/VM fixture teardown.
    if proof is not None:
        with open(out/'node-proof.json','xb') as f:os.chmod(f.fileno(),0o600);f.write((json.dumps(proof,indent=2)+'\n').encode());os.fsync(f.fileno())
    if provider_owner:
        receipt['node_test_reporter_one_executed']=all(re.search(rb'(?m)^# '+name+rb' '+value+rb'\r?$',contents[child.stdout]) for name,value in [(b'tests',b'1'),(b'pass',b'1'),(b'fail',b'0'),(b'cancelled',b'0'),(b'skipped',b'0')])
        assert proof and proof.get('ok') is True and proof.get('scope')==expected_scope and proof.get('executedCases')==1 and status==0 and not requested and not failed and not kill_requested and receipt['node_test_reporter_one_executed']
    else:
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
