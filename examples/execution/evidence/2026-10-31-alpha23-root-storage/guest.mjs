// Builtin-only functions embedded in Machine init; no image/file copying.
// Importing this module runs no smoke, filesystem or provider operation.
export async function guestSmoke(input) {
  const fs=await import('node:fs'), crypto=await import('node:crypto'), {spawn}=await import('node:child_process');
  const need=(ok,code)=>{if(!ok)throw Object.assign(new Error(code),{code});};
  const hash=b=>crypto.createHash('sha256').update(b).digest('hex');
  const encoded=b=>({bytes:b.length,sha256:hash(b),base64:b.toString('base64')});
  const proof={formatVersion:1,kind:'stock23-uid1500-smoke',nonce:input.nonce,status:'failed',
    modelCalls:false,supervisorLoaded:false,environmentCleared:true,commands:[]};
  const read=(file,max)=>{
    need(fs.realpathSync(file)===file,'STOCK_PATH_CHANGED');
    let parent='';for(const part of file.split('/').filter(Boolean).slice(0,-1)){
      parent+='/'+part;const m=fs.lstatSync(parent);
      need(m.isDirectory()&&!m.isSymbolicLink()&&m.uid===0&&m.gid===0&&!(m.mode&0o022),'STOCK_PARENT_WRITABLE');
    }
    const fd=fs.openSync(file,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);
    try{const first=fs.fstatSync(fd);need(first.isFile()&&first.uid===0&&first.gid===0&&first.nlink===1&&first.size>0&&first.size<=max,'STOCK_FILE_INVALID');
      const digest=crypto.createHash('sha256'),chunk=Buffer.alloc(65536);let count=0;
      for(;;){const n=fs.readSync(fd,chunk,0,chunk.length,null);if(!n)break;count+=n;need(count<=first.size&&count<=max,'STOCK_FILE_GREW');digest.update(chunk.subarray(0,n));}
      for(const last of [fs.fstatSync(fd),fs.lstatSync(file)])need(last.isFile()&&!last.isSymbolicLink()&&last.dev===first.dev&&last.ino===first.ino&&last.nlink===1&&last.uid===0&&last.gid===0&&last.mode===first.mode&&last.size===count&&last.mtimeMs===first.mtimeMs&&last.ctimeMs===first.ctimeMs,'STOCK_FILE_CHANGED');
      return {path:file,bytes:count,sha256:digest.digest('hex')};
    }finally{fs.closeSync(fd);}
  };
  const run=(label,argv)=>new Promise(resolve=>{
    const child=spawn(argv[0],argv.slice(1),{cwd:process.env.HOME,env:{...process.env},stdio:['ignore','pipe','pipe']});
    let out=Buffer.alloc(0),err=Buffer.alloc(0),status,signal,exitObserved=false,failure,done=false,settle;
    const finish=settled=>{if(done)return;done=true;clearTimeout(timer);clearTimeout(settle);
      if(!settled){child.stdout.destroy();child.stderr.destroy();child.unref();}
      resolve({label,argv,child:{code:status??null,signal:signal??null,exitObserved,settled,failure:failure||null},stdout:encoded(out),stderr:encoded(err)});};
    const stop=why=>{failure ||= why;if(!exitObserved)child.kill('SIGKILL');settle ||= setTimeout(()=>finish(false),2000);};
    const timer=setTimeout(()=>stop('COMMAND_DEADLINE'),12000);
    child.stdout.on('data',b=>{if(done)return;const keep=Math.max(0,16384-out.length);out=Buffer.concat([out,b.subarray(0,keep)]);if(b.length>keep)stop('COMMAND_STDOUT_BOUND');});
    child.stderr.on('data',b=>{if(done)return;const keep=Math.max(0,8192-err.length);err=Buffer.concat([err,b.subarray(0,keep)]);if(b.length>keep)stop('COMMAND_STDERR_BOUND');});
    child.on('error',()=>{failure ||= 'COMMAND_SPAWN';});child.on('exit',(code,sig)=>{status=code;signal=sig;exitObserved=true;});
    child.on('close',()=>finish(true));
  });
  try{
    need(process.getuid()===1500&&process.getgid()===1500,'UID1500_REQUIRED');
    const fields={};for(const line of fs.readFileSync('/proc/self/status','utf8').split('\n')){const i=line.indexOf(':');if(i>=0)fields[line.slice(0,i)]=line.slice(i+1).trim();}
    need(fields.Uid?.split(/\s+/).every(x=>x==='1500')&&fields.Gid?.split(/\s+/).every(x=>x==='1500')&&fields.Groups===''&&fields.NoNewPrivs==='1','IDENTITY_OR_NNP');
    for(const k of ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])need(/^0+$/.test(fields[k]||''),'CAPABILITIES_REMAIN');
    need(JSON.stringify(Object.keys(process.env).sort())===JSON.stringify(input.envKeys),'UNCLEARED_CHILD_ENV');
    for(const [k,v]of Object.entries(input.flags))need(process.env[k]===v,'SMOKE_FLAGS_CHANGED');
    for(const key of ['HOME','TMPDIR']){const m=fs.lstatSync(process.env[key]);need(m.isDirectory()&&!m.isSymbolicLink()&&m.uid===1500&&m.gid===1500&&(m.mode&0o7777)===0o700,'PRIVATE_UID1500_DIRECTORIES');}
    proof.identity={uid:1500,gid:1500,groups:[],noNewPrivs:1,capabilities:Object.fromEntries(['CapEff','CapPrm','CapInh','CapAmb','CapBnd'].map(k=>[k,fields[k]]))};
    const ro=input.readOnlyFixture;
    need(ro?.mountPoint==='/workspace/seed-ro-'+input.nonce+'/objects'&&ro.file===ro.mountPoint+'/fixture', 'RO_FIXTURE_INPUT');
    const mountText=fs.readFileSync('/proc/self/mountinfo','utf8');need(Buffer.byteLength(mountText)<=262144,'MOUNTINFO_BOUND');
    const matches=mountText.trim().split('\n').map(line=>line.split(' - ')).filter(parts=>parts.length===2&&parts[0].split(' ')[4]===ro.mountPoint);
    need(matches.length===1,'UID1500_RO_MOUNT_ABSENT');
    const left=matches[0][0].split(' '),right=matches[0][1].split(' '),options=left[5].split(','),superOptions=right[2].split(',');
    need(right[0]==='tmpfs'&&right[1]==='rudder-seed-ro-'+input.nonce&&['ro','nosuid','nodev','noexec'].every(flag=>options.includes(flag))&&superOptions.includes('ro'),'UID1500_RO_FLAGS');
    const mountNamespace=fs.readlinkSync('/proc/self/ns/mnt');need(mountNamespace===ro.mountNamespace&&/^mnt:\[[0-9]+\]$/.test(mountNamespace),'UID1500_MOUNT_NAMESPACE');
    const before=fs.lstatSync(ro.file);need(before.isFile()&&!before.isSymbolicLink()&&before.uid===1500&&before.gid===1500&&(before.mode&0o7777)===0o600&&before.nlink===1,'UID1500_FIXTURE_MODE');
    const bytes=fs.readFileSync(ro.file);need(bytes.length===ro.bytes&&hash(bytes)===ro.sha256,'UID1500_FIXTURE_BYTES');
    let writeError=null;try{fs.appendFileSync(ro.file,'forbidden-uid1500-write');}catch(error){writeError=error.code;}
    need(writeError==='EROFS'&&hash(fs.readFileSync(ro.file))===ro.sha256,'UID1500_WRITE_NOT_EROFS');
    const stat=fs.statfsSync(ro.mountPoint),capacityBytes=stat.bsize*stat.blocks;
    need(stat.type===0x01021994&&Number.isSafeInteger(capacityBytes)&&capacityBytes>0&&capacityBytes<=16777216,'UID1500_TMPFS_CAP');
    proof.readOnly={formatVersion:1,scope:'root-storage-capability-only',mountNamespace,mountId:Number(left[0]),
      mountPoint:ro.mountPoint,filesystem:right[0],source:right[1],options,superOptions,capacityBytes,
      fixture:{file:ro.file,uid:before.uid,gid:before.gid,mode:before.mode&0o7777,bytes:bytes.length,sha256:hash(bytes)},
      readPassed:true,writeAttempted:true,writeError,unchangedAfterWrite:true};
    const artifacts={};for(const [key,record]of Object.entries(input.artifacts)){
      artifacts[key]=read(record.path,536870912);need(artifacts[key].sha256===record.sha256&&artifacts[key].bytes===record.bytes,'STOCK_ARTIFACT_MISMATCH');
    }
    proof.artifacts=artifacts;
    proof.node={version:process.version,resolvedPath:fs.realpathSync(process.execPath),sha256:artifacts.node.sha256};
    need(/^v22\./.test(process.version)&&proof.node.resolvedPath==='/usr/local/bin/node','NODE_IDENTITY');
    const packageInfo=JSON.parse(fs.readFileSync('/opt/rudder/package.json','utf8'));
    need(packageInfo.version===input.packageVersion&&packageInfo.bin?.rudder==='dist/index.js','PACKAGE_VERSION');
    proof.packageVersion=packageInfo.version;
    const jobs=[['cli-version',['/usr/local/bin/node','/opt/rudder/dist/index.js','--version']],
      ['native-smoke',['/opt/rudder/dist/native/rudder-native','--smoke']],
      ['native-execution',['/opt/rudder/dist/native/rudder-native','fabric','execution','capabilities']],
      ['buddy-version',['/opt/rudder/dist/native/rudder-jj','--version']],
      ['buddy-capabilities',['/opt/rudder/dist/native/rudder-jj','rudder-capabilities']],
      ['git-version',['/usr/local/bin/git','--version']],['jj-version',['/usr/local/bin/jj','--version']]];
    for(const [label,argv]of jobs){const result=await run(label,argv);proof.commands.push(result);
      need(result.child.exitObserved&&result.child.settled&&result.child.code===0&&result.child.signal===null&&result.child.failure===null,'SMOKE_COMMAND_FAILED');
      const text=Buffer.from(result.stdout.base64,'base64').toString('utf8').trim();
      if(label==='cli-version')need(text===input.packageVersion,'CLI_VERSION');
      else if(label==='native-smoke')need(text==='rudder-native smoke ok','NATIVE_SMOKE');
      else if(label==='native-execution'){const c=JSON.parse(text);need(c.format_version===1&&c.platform==='linux'&&c.network_isolated===false,'EXECUTION_CAPABILITIES');}
      else if(label==='buddy-version')need(text.startsWith('rudder-jj 0.40.0-rudder-bridge-v1'),'BUDDY_VERSION');
      else if(label==='buddy-capabilities'){const c=JSON.parse(text);need(c.format_version===1&&c.jj_version==='0.40.0'&&c.vfs_default===false&&c.local_development===false&&c.automatic_jj_replacement===false,'BUDDY_CAPABILITIES');}
      else if(label==='git-version')need(/^git version 2\.56\.0(?:\s|$)/.test(text),'GIT_VERSION');
      else if(label==='jj-version')need(/^jj 0\.40\.0(?:-[0-9a-f]{40})?$/.test(text),'JJ_VERSION');
    }
    need(fs.readlinkSync('/proc/self/ns/mnt')===mountNamespace,'UID1500_MOUNT_NAMESPACE_CHANGED');
    proof.status='passed';
  }catch(error){proof.failure=/^[A-Z0-9_]+$/.test(error?.code||'')?error.code:'SMOKE_REJECTED';}
  return proof;
}

export async function guestRoot(input, guestSmoke) {
  const fs=await import('node:fs'),crypto=await import('node:crypto'),{spawn}=await import('node:child_process');
  const need=(ok,code)=>{if(!ok)throw Object.assign(new Error(code),{code});};
  const flags={PATH:'/usr/local/bin:/usr/bin:/bin',RUDDER_DISABLE_UPDATE_CHECK:'1',RUDDER_DISABLE_AUTO_UPDATE:'1',RUDDER_SKIP_AUTO_UPDATE:'1',RUDDER_DISABLE_ONBOARD_INSTALL:'1',RUDDER_TELEMETRY:'off'};
  const root='/run/rudder-seed-ro-'+input.nonce,base='/workspace/seed-ro-'+input.nonce,mountPoint=base+'/objects';
  const hash=b=>crypto.createHash('sha256').update(b).digest('hex');
  need(/^[a-f0-9-]{36}$/.test(input.nonce)&&input.packageVersion==='4.0.0-alpha.23','BOOTSTRAP_INPUT');
  need(process.getuid()===0&&JSON.stringify(Object.keys(process.env).sort())===JSON.stringify(Object.keys(flags).sort()),'CLEARED_ROOT_IDENTITY');
  for(const [k,v]of Object.entries(flags))need(process.env[k]===v,'ROOT_FLAGS_CHANGED');
  fs.mkdirSync(root,{mode:0o700});
  const report={formatVersion:1,kind:input.role,...input,qualified:false,scope:'root-storage-capability-only',
    productionSeedProducer:false,managedBackend:false,preparationReuse:false,modelCalls:false,supervisorLoaded:false,stockImage:true};
  const storage=report.storageRoot={formatVersion:1,scope:'root-storage-capability-only',nonce:input.nonce,status:'failed',
    mountPoint,maxBytes:16777216,capSysAdminBit:21,attempted:false,actions:[],cleanup:{attempted:false,verifiedAbsent:false}};
  const write=(name,bytes)=>{const fd=fs.openSync(root+'/'+name,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW,0o600);try{fs.writeFileSync(fd,bytes);fs.fsyncSync(fd);}finally{fs.closeSync(fd);}};
  let child,proofSaved=false,uidChildSettled=false,rootCommandUnsettled=false,success=false;
  const saveProof=()=>{if(proofSaved)return;report.utc=new Date().toISOString();const data=JSON.stringify(report);need(Buffer.byteLength(data)<=1048576,'PROOF_BYTE_BOUND');write('proof.pending',data);fs.renameSync(root+'/proof.pending',root+'/proof.json');proofSaved=true;};
  const mountInfo=()=>{
    const text=fs.readFileSync('/proc/self/mountinfo','utf8');need(Buffer.byteLength(text)<=262144,'MOUNTINFO_BOUND');
    const matches=text.trim().split('\n').map(line=>line.split(' - ')).filter(parts=>parts.length===2&&parts[0].split(' ')[4]===mountPoint);
    need(matches.length<=1,'MOUNT_ID_AMBIGUOUS');if(!matches.length)return null;
    const a=matches[0][0].split(' '),b=matches[0][1].split(' ');
    need(Number.isSafeInteger(Number(a[0]))&&Number(a[0])>0,'MOUNT_ID_INVALID');
    return {mountId:Number(a[0]),mountPoint:a[4],filesystem:b[0],source:b[1],options:a[5].split(','),superOptions:b[2].split(',')};
  };
  const run=async(label,program,args)=>{
    const action={label,argv:[program,...args]};storage.actions.push(action);
    action.child=await new Promise(resolve=>{
      const processChild=spawn(program,args,{cwd:root,env:{PATH:flags.PATH},shell:false,stdio:['ignore','pipe','pipe']});
      let out=Buffer.alloc(0),err=Buffer.alloc(0),code,signal,exitObserved=false,failure,done=false,settle;
      const finish=settled=>{if(done)return;done=true;clearTimeout(timer);clearTimeout(settle);
        if(!settled){rootCommandUnsettled=true;processChild.stdout.destroy();processChild.stderr.destroy();processChild.unref();}
        resolve({code:code??null,signal:signal??null,exitObserved,settled,failure:failure||null,
          stdoutBytes:out.length,stderrBytes:err.length,stdoutSha256:hash(out),stderrSha256:hash(err)});};
      const stop=why=>{failure ||= why;if(!exitObserved)processChild.kill('SIGKILL');settle ||= setTimeout(()=>finish(false),2000);};
      const timer=setTimeout(()=>stop('ROOT_COMMAND_DEADLINE'),5000);
      processChild.stdout.on('data',b=>{if(done)return;const keep=Math.max(0,8192-out.length);out=Buffer.concat([out,b.subarray(0,keep)]);if(b.length>keep)stop('ROOT_STDOUT_BOUND');});
      processChild.stderr.on('data',b=>{if(done)return;const keep=Math.max(0,8192-err.length);err=Buffer.concat([err,b.subarray(0,keep)]);if(b.length>keep)stop('ROOT_STDERR_BOUND');});
      processChild.on('error',()=>{failure ||= 'ROOT_COMMAND_SPAWN';});
      processChild.on('exit',(value,sig)=>{code=value;signal=sig;exitObserved=true;});processChild.on('close',()=>finish(true));
    });
    need(action.child.exitObserved&&action.child.settled&&action.child.code===0&&action.child.signal===null&&action.child.failure===null,'ROOT_MOUNT_COMMAND_FAILED');
    return action;
  };
  const fuseMs=Math.min(300000,input.fuseAtMs-Date.now());need(fuseMs>0,'GUEST_ALREADY_EXPIRED');
  setTimeout(()=>{if(child&&child.exitCode===null&&child.signalCode===null)child.kill('SIGKILL');if(!proofSaved){report.failure='GUEST_FUSE';storage.cleanup.failure='GUEST_FUSE_NO_SETTLEMENT';saveProof();}process.exit(124);},fuseMs);
  try{
    const admissionEnd=Math.min(input.fuseAtMs,Date.now()+150000);
    while(!fs.existsSync(root+'/admission.json')){need(Date.now()<admissionEnd,'ADMISSION_DEADLINE');await new Promise(r=>setTimeout(r,200));}
    const admitted=fs.openSync(root+'/admission.json',fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);
    try{const m=fs.fstatSync(admitted);need(m.isFile()&&m.uid===0&&m.gid===0&&m.nlink===1&&(m.mode&0o7777)===0o600&&m.size<=8192,'ADMISSION_FILE');need(JSON.stringify(JSON.parse(fs.readFileSync(admitted,'utf8')))===JSON.stringify(input),'ADMISSION_CHANGED');}finally{fs.closeSync(admitted);}
    report.admittedAfterResolvedDigest=true;
    const fields={};for(const line of fs.readFileSync('/proc/self/status','utf8').split('\n')){const i=line.indexOf(':');if(i>=0)fields[line.slice(0,i)]=line.slice(i+1).trim();}
    const caps=Object.fromEntries(['CapEff','CapPrm','CapInh','CapAmb','CapBnd'].map(k=>[k,fields[k]]));
    need(Object.values(caps).every(value=>/^[a-f0-9]{16}$/.test(value||'')),'ROOT_CAPABILITY_RECORD');
    storage.identity={uid:process.getuid(),gid:process.getgid(),uids:fields.Uid,gids:fields.Gid,noNewPrivs:fields.NoNewPrivs,capabilities:caps};
    storage.mountNamespace=fs.readlinkSync('/proc/self/ns/mnt');need(/^mnt:\[[0-9]+\]$/.test(storage.mountNamespace),'ROOT_MOUNT_NAMESPACE');
    storage.capSysAdminEffective=(BigInt('0x'+caps.CapEff)&(1n<<21n))!==0n;
    need(storage.capSysAdminEffective,'ROOT_CAP_SYS_ADMIN_MISSING');
    const read=(file,max)=>{need(fs.realpathSync(file)===file,'STOCK_PATH_CHANGED');let dir='';for(const part of file.split('/').filter(Boolean).slice(0,-1)){dir+='/'+part;const m=fs.lstatSync(dir);need(m.isDirectory()&&!m.isSymbolicLink()&&m.uid===0&&m.gid===0&&!(m.mode&0o022),'STOCK_PARENT');}
      const fd=fs.openSync(file,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);try{const m=fs.fstatSync(fd);need(m.isFile()&&m.uid===0&&m.gid===0&&m.nlink===1&&m.size>0&&m.size<=max,'STOCK_FILE');const h=crypto.createHash('sha256'),b=Buffer.alloc(65536);let n=0;for(;;){const q=fs.readSync(fd,b,0,b.length,null);if(!q)break;n+=q;need(n<=m.size&&n<=max,'STOCK_GROWTH');h.update(b.subarray(0,q));}for(const t of [fs.fstatSync(fd),fs.lstatSync(file)])need(t.isFile()&&!t.isSymbolicLink()&&t.dev===m.dev&&t.ino===m.ino&&t.size===n&&t.nlink===1&&t.uid===0&&t.gid===0&&t.mode===m.mode&&t.mtimeMs===m.mtimeMs&&t.ctimeMs===m.ctimeMs,'STOCK_CHANGED');return {path:file,bytes:n,sha256:h.digest('hex')};}finally{fs.closeSync(fd);}};
    const artifacts={node:read('/usr/local/bin/node',536870912),native:read('/opt/rudder/dist/native/rudder-native',536870912),buddy:read('/opt/rudder/dist/native/rudder-jj',536870912),cli:read('/opt/rudder/dist/index.js',4194304),package:read('/opt/rudder/package.json',131072)};
    const info=JSON.parse(fs.readFileSync('/opt/rudder/package.json','utf8'));need(info.version===input.packageVersion,'ROOT_PACKAGE_VERSION');
    report.artifacts=artifacts;report.packageVersion=info.version;report.node={version:process.version,resolvedPath:fs.realpathSync(process.execPath),sha256:artifacts.node.sha256};

    storage.tools={mount:read('/usr/bin/mount',16777216),umount:read('/usr/bin/umount',16777216)};
    need(fs.realpathSync('/workspace')==='/workspace'&&fs.lstatSync('/workspace').isDirectory(),'WORKSPACE_SHAPE');
    fs.mkdirSync(base,{mode:0o700});fs.chownSync(base,1500,1500);
    fs.mkdirSync(mountPoint,{mode:0o700});
    need(mountInfo()===null,'PREEXISTING_SCRATCH_MOUNT');storage.initiallyAbsent=true;storage.attempted=true;
    await run('mount-tmpfs','/usr/bin/mount',['-t','tmpfs','-o','size=16777216,nosuid,nodev,noexec,mode=0700,uid=1500,gid=1500','rudder-seed-ro-'+input.nonce,mountPoint]);
    storage.beforeReadonly=mountInfo();need(storage.beforeReadonly?.filesystem==='tmpfs'&&storage.beforeReadonly.source==='rudder-seed-ro-'+input.nonce&&storage.beforeReadonly.options.includes('rw'),'TMPFS_NOT_RW');
    const file=mountPoint+'/fixture',bytes=Buffer.from('rudder-seed-ro-fixture-v1\n'+input.nonce+'\n');
    const fixture=fs.openSync(file,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW,0o600);
    try{fs.writeFileSync(fixture,bytes);fs.fchownSync(fixture,1500,1500);fs.fsyncSync(fixture);}finally{fs.closeSync(fixture);}
    const metadata=fs.lstatSync(file);need(metadata.isFile()&&metadata.uid===1500&&metadata.gid===1500&&(metadata.mode&0o7777)===0o600&&metadata.nlink===1,'FIXTURE_RW_MODE');
    storage.fixture={file,uid:metadata.uid,gid:metadata.gid,mode:metadata.mode&0o7777,bytes:bytes.length,sha256:hash(bytes),createdBeforeReadonly:true};
    await run('remount-ro','/usr/bin/mount',['-o','remount,ro,nosuid,nodev,noexec,size=16777216','--',mountPoint]);
    storage.readonly=mountInfo();need(storage.readonly&&['ro','nosuid','nodev','noexec'].every(flag=>storage.readonly.options.includes(flag))&&storage.readonly.superOptions.includes('ro'),'ROOT_RO_FLAGS');
    need(storage.readonly.mountId===storage.beforeReadonly.mountId,'REMOUNT_ID_CHANGED');
    const stat=fs.statfsSync(mountPoint),capacityBytes=stat.bsize*stat.blocks;
    need(stat.type===0x01021994&&Number.isSafeInteger(capacityBytes)&&capacityBytes>0&&capacityBytes<=16777216,'ROOT_TMPFS_CAP');storage.capacityBytes=capacityBytes;
    let rootWriteError=null;try{fs.appendFileSync(file,'forbidden-root-write');}catch(error){rootWriteError=error.code;}
    storage.rootWrite={uid:process.getuid(),attempted:true,error:rootWriteError};need(rootWriteError==='EROFS','ROOT_WRITE_NOT_EROFS');
    need(hash(fs.readFileSync(file))===storage.fixture.sha256,'ROOT_FIXTURE_CHANGED');storage.rootWrite.unchangedAfterWrite=true;
    for(const name of ['home','tmp','rudder','codex','claude','cache','config','data']){fs.mkdirSync(base+'/'+name,{mode:0o700});fs.chownSync(base+'/'+name,1500,1500);}
    for(const name of ['gitconfig','jj-config.toml','npmrc']){const fd=fs.openSync(base+'/'+name,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW,0o600);try{fs.fchownSync(fd,1500,1500);}finally{fs.closeSync(fd);}}
    const env={...flags,HOME:base+'/home',TMPDIR:base+'/tmp',RUDDER_HOME:base+'/rudder',CODEX_HOME:base+'/codex',CLAUDE_CONFIG_DIR:base+'/claude',
      XDG_CACHE_HOME:base+'/cache',XDG_CONFIG_HOME:base+'/config',XDG_DATA_HOME:base+'/data',
      GIT_CONFIG_GLOBAL:base+'/gitconfig',GIT_CONFIG_SYSTEM:base+'/gitconfig',GIT_CONFIG_NOSYSTEM:'1',JJ_CONFIG:base+'/jj-config.toml',
      npm_config_userconfig:base+'/npmrc',npm_config_globalconfig:base+'/npmrc',NODE_OPTIONS:'--max-old-space-size=384',CI:'1',TERM:'dumb',TZ:'UTC'};
    const data={...input,artifacts,flags,envKeys:Object.keys(env).sort(),readOnlyFixture:{...storage.fixture,mountPoint,mountNamespace:storage.mountNamespace}};
    const source=`(${guestSmoke.toString()})(${JSON.stringify(data)}).then(p=>{console.log(JSON.stringify(p));if(p.status!=='passed')process.exitCode=1;}).catch(()=>{console.error('UID1500_SMOKE_FAILED');process.exitCode=1;});`;
    const args=['--reuid=1500','--regid=1500','--clear-groups','--inh-caps=-all','--ambient-caps=-all','--bounding-set=-all','--no-new-privs','--','/usr/bin/env','-i',...Object.entries(env).map(([k,v])=>k+'='+v),'/usr/local/bin/node','--input-type=module','-e',source];
    let out=Buffer.alloc(0),err=Buffer.alloc(0);
    write('child.stdout.private',out);write('child.stderr.private',err);
    report.child=await new Promise(resolve=>{
      child=spawn('/usr/bin/setpriv',args,{env:{PATH:flags.PATH},stdio:['ignore','pipe','pipe']});
      let code,signal,exitObserved=false,failure,done=false,settle;
      const stop=why=>{failure ||= why;if(!exitObserved)child.kill('SIGKILL');settle ||= setTimeout(()=>finish(false),2000);};
      const timer=setTimeout(()=>stop('UID1500_CHILD_DEADLINE'),120000);
      const finish=settled=>{if(done)return;done=true;clearTimeout(timer);clearTimeout(settle);if(!settled){child.stdout.destroy();child.stderr.destroy();child.unref();}resolve({code:code??null,signal:signal??null,exitObserved,settled,failure:failure||null,stdoutBytes:out.length,stderrBytes:err.length,stdoutSha256:hash(out),stderrSha256:hash(err)});};
      child.stdout.on('data',b=>{if(done)return;const keep=Math.max(0,524288-out.length),part=b.subarray(0,keep);out=Buffer.concat([out,part]);fs.appendFileSync(root+'/child.stdout.private',part);if(b.length>keep)stop('UID1500_STDOUT_BOUND');});
      child.stderr.on('data',b=>{if(done)return;const keep=Math.max(0,8192-err.length),part=b.subarray(0,keep);err=Buffer.concat([err,part]);fs.appendFileSync(root+'/child.stderr.private',part);if(b.length>keep)stop('UID1500_STDERR_BOUND');});
      child.on('error',()=>{failure ||= 'UID1500_SPAWN';});child.on('exit',(value,sig)=>{code=value;signal=sig;exitObserved=true;});child.on('close',()=>finish(true));
    });
    uidChildSettled=report.child.exitObserved===true&&report.child.settled===true;
    try{report.smoke=JSON.parse(out.toString('utf8').trim());}catch{report.failure='UID1500_INVALID_JSON';}
    need(report.child.exitObserved&&report.child.settled&&report.child.code===0&&report.child.signal===null&&report.child.failure===null,'UID1500_CHILD_FAILED');
    need(report.smoke?.status==='passed'&&report.smoke.nonce===input.nonce,'UID1500_SMOKE_REJECTED');

    need(report.smoke.readOnly?.mountId===storage.readonly.mountId,'CHILD_MOUNT_ID_CHANGED');success=true;
  }catch(error){report.failure ||= /^[A-Z0-9_]+$/.test(error?.code||'')?error.code:'RO_GUEST_REJECTED';}
  finally{
    const cleanup=storage.cleanup;cleanup.childAbsentOrSettled=!child||uidChildSettled;
    cleanup.rootCommandsSettled=!rootCommandUnsettled;
    try{
      need(cleanup.childAbsentOrSettled&&cleanup.rootCommandsSettled,'UNSETTLED_CHILD_RETAINS_MOUNT');
      const current=mountInfo();cleanup.presentBefore=!!current;
      if(current){need(storage.attempted&&current.filesystem==='tmpfs'&&current.source==='rudder-seed-ro-'+input.nonce,'CLEANUP_MOUNT_OWNERSHIP');
        if(storage.beforeReadonly)need(current.mountId===storage.beforeReadonly.mountId,'CLEANUP_MOUNT_ID_CHANGED');
        cleanup.attempted=true;await run('umount','/usr/bin/umount',['--',mountPoint]);}
      cleanup.verifiedAbsent=mountInfo()===null;need(cleanup.verifiedAbsent,'MOUNT_STILL_PRESENT');
      cleanup.mountNamespace=fs.readlinkSync('/proc/self/ns/mnt');
      if(storage.mountNamespace)need(cleanup.mountNamespace===storage.mountNamespace,'ROOT_MOUNT_NAMESPACE_CHANGED');
    }catch(error){cleanup.failure=/^[A-Z0-9_]+$/.test(error?.code||'')?error.code:'MOUNT_CLEANUP_REJECTED';}
    const ro=report.smoke?.readOnly;
    const readOnlyPassed=uidChildSettled&&storage.rootWrite?.error==='EROFS'&&ro?.readPassed===true
      &&ro.writeAttempted===true&&ro.writeError==='EROFS'&&ro.unchangedAfterWrite===true
      &&ro.mountId===storage.readonly?.mountId&&ro.mountNamespace===storage.mountNamespace;
    storage.status=readOnlyPassed&&cleanup.attempted&&cleanup.verifiedAbsent&&!cleanup.failure?'passed':'failed';
    report.qualified=success&&storage.status==='passed';
  }
  saveProof();
  // Only the exact-instance controller destroys this disposable VM. A root
  // mount capability result never claims a production seed owner or backend.
  await new Promise(()=>{});
}
