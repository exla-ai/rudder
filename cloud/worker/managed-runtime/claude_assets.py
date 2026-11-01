#!/usr/bin/env python3
"""Pinned signed Claude candidate DATA packaging. Never executes the backend.
No gpg, keyserver, default keyring, installer, profile, credential or model call.
"""
from pathlib import Path
import argparse, base64, hashlib, importlib.util, json, os, selectors, shutil, stat, subprocess, time, urllib.request, uuid

_spec=importlib.util.spec_from_file_location('rudder_image_assets',Path(__file__).with_name('assets.py'))
assets=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(assets)
VERSION='2.1.316'
SHA='fd13c70000fec3f2116dfbcbb6121210b4a29328bf0e46210a59dd594a67d824'
SIZE=328262000
FP='31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE'
URL='https://downloads.claude.ai/claude-code-releases/2.1.316/linux-x64/claude'
GPGV='/usr/bin/gpgv'

def pinned(path,expected,limit,end):
    assets.check_time(end);before=path.lstat()
    assets.need(stat.S_ISREG(before.st_mode) and before.st_nlink==1 and before.st_size<=limit,'CLAUDE_INPUT_FILE')
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC|os.O_NONBLOCK)
    try:
        assets.need(assets.file_stamp(os.fstat(fd))==assets.file_stamp(before),'CLAUDE_INPUT_OPEN_CHANGED')
        chunks=[];total=0
        while True:
            assets.check_time(end);b=os.read(fd,assets.CHUNK)
            if not b:break
            total+=len(b);assets.need(total<=limit,'CLAUDE_INPUT_BOUND');chunks.append(b)
        raw=b''.join(chunks)
        assets.need(total==before.st_size and assets.file_stamp(os.fstat(fd))==assets.file_stamp(path.lstat())==assets.file_stamp(before)
                    and assets.sha(raw)==expected,'CLAUDE_INPUT_PIN')
        return raw
    finally:os.close(fd)

def selected(manifest,spec):
    assets.need(manifest['version']==VERSION,'CLAUDE_MANIFEST_VERSION')
    p=manifest['platforms']['linux-x64']
    assets.need(p=={'binary':'claude','checksum':SHA,'size':SIZE} and spec['version']==VERSION and spec['platform']=='linux-x64'
                and spec['url']==URL and spec['bytes']==SIZE and spec['sha256']==SHA and spec['primary_fingerprint']==FP,'CLAUDE_PINNED_PLATFORM')
    return p

def status_valid(raw):
    # Trust only actual verifier success plus exact machine-readable primary
    # and signing fingerprints. A GOODSIG human UID line alone is insufficient.
    lines=raw.decode('ascii').splitlines();valid=[]
    for line in lines:
        assets.need(line.startswith('[GNUPG:] '),'CLAUDE_VERIFIER_STATUS')
        fields=line.split();tag=fields[1]
        assets.need(tag not in {'BADSIG','ERRSIG','NO_PUBKEY','NODATA','FAILURE','ERROR','EXPSIG','EXPKEYSIG','REVKEYSIG'},'CLAUDE_VERIFIER_REJECTED')
        if tag=='VALIDSIG':valid.append(fields[2:])
    assets.need(len(valid)==1 and len(valid[0])==10 and valid[0][0]==FP and valid[0][-1]==FP
                and valid[0][6]=='1' and valid[0][7]=='10','CLAUDE_VALID_SIGNATURE_REQUIRED')

def verify_signature(manifest,signature,keyring,scratch,end):
    assets.directory(scratch);work=scratch/('.claude-verify-'+uuid.uuid4().hex);work.mkdir(mode=0o700)
    for name,data in [('manifest.json',manifest),('manifest.json.sig',signature),('release-key.gpg',keyring)]:assets.write_new(work/name,data,0o400)
    home=work/'home';home.mkdir(mode=0o700);assets.fsync_dir(work)
    verifier=Path(GPGV);assets.directory(verifier.parent);before=verifier.lstat()
    assets.need(stat.S_ISREG(before.st_mode) and before.st_uid==before.st_gid==0 and before.st_nlink==1 and not before.st_mode&0o022,'CLAUDE_VERIFIER_IMAGE')
    fd=os.open(verifier,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW);process=None;pidfd=None;pipe_owner=None
    try:
        assets.need(assets.file_stamp(os.fstat(fd))==assets.file_stamp(before),'CLAUDE_VERIFIER_OPEN_CHANGED')
        assets.check_time(end)
        process=subprocess.Popen([GPGV,'--homedir',str(home),'--keyring',str(work/'release-key.gpg'),'--status-fd','1','--',str(work/'manifest.json.sig'),str(work/'manifest.json')],
            executable='/proc/self/fd/'+str(fd),stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,env={},cwd=work,pass_fds=(fd,))
        # Popen is held BEFORE pidfd/pipe work; finally never abandons its child.
        pidfd=os.pidfd_open(process.pid,0);pipe_owner=selectors.DefaultSelector()
        output={process.stdout:bytearray(),process.stderr:bytearray()}
        for stream in output:os.set_blocking(stream.fileno(),False);pipe_owner.register(stream,selectors.EVENT_READ)
        cancelled=False;failed=False
        while pipe_owner.get_map() or process.poll() is None:
            if not cancelled and time.clock_gettime(time.CLOCK_BOOTTIME)>=end:cancelled=True;process.kill()
            for event,_ in pipe_owner.select(0.02):
                chunk=os.read(event.fileobj.fileno(),4096)
                if not chunk:pipe_owner.unregister(event.fileobj);continue
                if len(output[event.fileobj])+len(chunk)>8192:
                    failed=True
                    if not cancelled:cancelled=True;process.kill()
                elif not failed:output[event.fileobj].extend(chunk)
        code=process.wait()  # actual wait and BOTH pipe EOFs above, not kill ACK
        assets.need(not cancelled and not failed and code==0,'CLAUDE_VERIFIER_FAILED')
        status_valid(bytes(output[process.stdout]));assets.check_time(end)
        assets.need(assets.file_stamp(os.fstat(fd))==assets.file_stamp(verifier.lstat())==assets.file_stamp(before),'CLAUDE_VERIFIER_CHANGED')
        verifier_hash,_=assets.file_hash(verifier,4*1024*1024,end)
        return {'signatureVerified':True,'primaryFingerprint':FP,'verifierSha256':verifier_hash,'actualVerifierExit':code,
                'stdoutSha256':assets.sha(output[process.stdout]),'stderrSha256':assets.sha(output[process.stderr]),'actualWaitAndBothEof':True}
    finally:
        # No detached verifier even on partial pidfd/selector initialization.
        # An uninterruptible kernel wait can keep this build charged; an external
        # timeout is not a cleanup receipt and this helper cannot attest one.
        if process is not None:
            if process.poll() is None:process.kill()
            process.wait()
            for stream in [process.stdout,process.stderr]:
                if stream is not None:stream.close()
        if pipe_owner is not None:pipe_owner.close()
        if pidfd is not None:os.close(pidfd)
        os.close(fd)

def install_raw(raw,output,end,bounds=lambda:None):
    assets.need(not output.exists() and not output.is_symlink(),'CLAUDE_OUTPUT_EXISTS')
    pending=output.parent/('.claude.pending-'+uuid.uuid4().hex)
    fd=os.open(pending,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,0o600);total=0;digest=hashlib.sha256();prefix=b''
    try:
        while True:
            assets.check_time(end);bounds();b=raw.read(assets.CHUNK)
            if not b:break
            total+=len(b);assets.need(total<=SIZE,'CLAUDE_BINARY_BOUND');digest.update(b)
            if len(prefix)<4:prefix=(prefix+b)[:4]
            view=memoryview(b)
            while view:
                n=os.write(fd,view);assets.need(n>0,'CLAUDE_WRITE_UNCERTAIN');view=view[n:]
        assets.need(total==SIZE and digest.hexdigest()==SHA and prefix==b'\x7fELF','CLAUDE_BINARY_IDENTITY')
        os.fchmod(fd,0o555);os.fsync(fd)
    finally:os.close(fd)
    assets.check_time(end);bounds();assets.rename_new(pending,output);assets.fsync_dir(output.parent)

def main():
    p=argparse.ArgumentParser();p.add_argument('--policy',required=True);p.add_argument('--metadata-root',required=True);p.add_argument('--scratch-root',required=True)
    p.add_argument('--output',required=True);p.add_argument('--receipt',required=True);p.add_argument('--binary');p.add_argument('--free-floor-bytes',type=int,default=512*1024*1024)
    args=p.parse_args();assets.need(os.geteuid()==os.getegid()==0,'CLAUDE_BUILD_REQUIRES_ROOT')
    end=assets.original_end();policy=assets.policy_at(args.policy);spec=policy['claude'];assets.need(policy.get('claude_packaged') is True and policy.get('claude_supported') is False,'CLAUDE_CANDIDATE_ONLY')
    root=assets.directory(args.metadata_root);output=Path(args.output);assets.directory(output.parent);assets.directory(Path(args.receipt).parent)
    manifest=pinned(root/'manifest.json',spec['manifest_sha256'],65536,end);signature=pinned(root/'manifest.json.sig',spec['signature_sha256'],65536,end)
    pinned(root/'release-key.asc',spec['public_key_ascii_sha256'],65536,end)
    encoded=pinned(root/'release-keyring.b64',spec['public_keyring_base64_sha256'],65536,end);keyring=base64.b64decode(encoded.strip(),validate=True)
    assets.need(assets.sha(keyring)==spec['public_keyring_sha256'],'CLAUDE_PUBLIC_KEYRING_PIN')
    pinned(root/'acquisition.json',spec['metadata_sha256'],65536,end);selected(json.loads(manifest),spec)
    verification=verify_signature(manifest,signature,keyring,Path(args.scratch_root),end)
    assets.need(args.free_floor_bytes>=0 and shutil.disk_usage(output.parent).free>args.free_floor_bytes+SIZE+16*1024*1024,'CLAUDE_ACQUISITION_FREE_SPACE')
    bounds=lambda:assets.need(shutil.disk_usage(output.parent).free>args.free_floor_bytes+16*1024*1024,'CLAUDE_ACQUISITION_DISK_FLOOR')
    if args.binary:
        path=Path(args.binary);before=path.lstat();assets.need(stat.S_ISREG(before.st_mode) and before.st_nlink==1 and before.st_size==SIZE,'CLAUDE_REUSED_ARTIFACT_FILE')
        fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC|os.O_NONBLOCK)
        with os.fdopen(fd,'rb') as raw:
            assets.need(assets.file_stamp(os.fstat(raw.fileno()))==assets.file_stamp(before),'CLAUDE_REUSED_OPEN_CHANGED')
            install_raw(raw,output,end,bounds)
            assets.need(assets.file_stamp(os.fstat(raw.fileno()))==assets.file_stamp(path.lstat())==assets.file_stamp(before),'CLAUDE_REUSED_ARTIFACT_CHANGED')
    else:
        opener=urllib.request.build_opener(urllib.request.ProxyHandler({}),assets.NoRedirect())
        with opener.open(URL,timeout=20) as raw:
            assets.need(raw.status==200 and raw.geturl()==URL,'CLAUDE_BINARY_RESPONSE');install_raw(raw,output,end,bounds)
    receipt={'format_version':1,'artifact':'claude','version':VERSION,'platform':'linux-x64','sha256':SHA,'size_bytes':SIZE,'source_policy_sha256':policy['_raw_sha256'],
        'manifest_sha256':spec['manifest_sha256'],'signature_sha256':spec['signature_sha256'],'public_keyring_sha256':spec['public_keyring_sha256'],'verification':verification,
        'reused_supplied_binary':bool(args.binary),'backendExecuted':False,'paidModelCalls':0,'claude_packaged':True,'claude_supported':False,'productionStartup':False,'spec3':False,'models':False}
    assets.write_new(Path(args.receipt),assets.canonical(receipt),0o444);assets.fsync_dir(Path(args.receipt).parent)

if __name__=='__main__':
    try:main()
    except Exception:raise SystemExit('RUDDER_CLAUDE_CANDIDATE_REJECTED')
