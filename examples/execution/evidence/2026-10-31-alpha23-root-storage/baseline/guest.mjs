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
    proof.status='passed';
  }catch(error){proof.failure=/^[A-Z0-9_]+$/.test(error?.code||'')?error.code:'SMOKE_REJECTED';}
  return proof;
}

export async function guestRoot(input, guestSmoke) {
  const fs=await import('node:fs'),crypto=await import('node:crypto'),{spawn}=await import('node:child_process');
  const need=(ok,code)=>{if(!ok)throw Object.assign(new Error(code),{code});};
  const flags={PATH:'/usr/local/bin:/usr/bin:/bin',RUDDER_DISABLE_UPDATE_CHECK:'1',RUDDER_DISABLE_AUTO_UPDATE:'1',RUDDER_SKIP_AUTO_UPDATE:'1',RUDDER_DISABLE_ONBOARD_INSTALL:'1',RUDDER_TELEMETRY:'off'};
  const root='/run/rudder-stock23-'+input.nonce,hash=b=>crypto.createHash('sha256').update(b).digest('hex');
  need(/^[a-f0-9-]{36}$/.test(input.nonce)&&input.packageVersion==='4.0.0-alpha.23','BOOTSTRAP_INPUT');
  need(process.getuid()===0&&JSON.stringify(Object.keys(process.env).sort())===JSON.stringify(Object.keys(flags).sort()),'CLEARED_ROOT_IDENTITY');
  for(const [k,v]of Object.entries(flags))need(process.env[k]===v,'ROOT_FLAGS_CHANGED');
  fs.mkdirSync(root,{mode:0o700});
  const report={formatVersion:1,kind:input.role,...input,qualified:false,modelCalls:false,supervisorLoaded:false,stockImage:true};
  const write=(name,bytes)=>{const fd=fs.openSync(root+'/'+name,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW,0o600);try{fs.writeFileSync(fd,bytes);fs.fsyncSync(fd);}finally{fs.closeSync(fd);}};
  let child,proofSaved=false;
  const saveProof=()=>{if(proofSaved)return;report.utc=new Date().toISOString();const data=JSON.stringify(report);need(Buffer.byteLength(data)<=1048576,'PROOF_BYTE_BOUND');write('proof.pending',data);fs.renameSync(root+'/proof.pending',root+'/proof.json');proofSaved=true;};
  const fuseMs=Math.min(300000,input.fuseAtMs-Date.now());need(fuseMs>0,'GUEST_ALREADY_EXPIRED');
  setTimeout(()=>{if(child&&child.exitCode===null&&child.signalCode===null)child.kill('SIGKILL');if(!proofSaved){report.failure='GUEST_FUSE';saveProof();}process.exit(124);},fuseMs);
  try{
    const admissionEnd=Math.min(input.fuseAtMs,Date.now()+150000);
    while(!fs.existsSync(root+'/admission.json')){need(Date.now()<admissionEnd,'ADMISSION_DEADLINE');await new Promise(r=>setTimeout(r,200));}
    const admitted=fs.openSync(root+'/admission.json',fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);
    try{const m=fs.fstatSync(admitted);need(m.isFile()&&m.uid===0&&m.gid===0&&m.nlink===1&&(m.mode&0o7777)===0o600&&m.size<=8192,'ADMISSION_FILE');need(JSON.stringify(JSON.parse(fs.readFileSync(admitted,'utf8')))===JSON.stringify(input),'ADMISSION_CHANGED');}finally{fs.closeSync(admitted);}
    report.admittedAfterResolvedDigest=true;
    const read=(file,max)=>{need(fs.realpathSync(file)===file,'STOCK_PATH_CHANGED');let dir='';for(const part of file.split('/').filter(Boolean).slice(0,-1)){dir+='/'+part;const m=fs.lstatSync(dir);need(m.isDirectory()&&!m.isSymbolicLink()&&m.uid===0&&m.gid===0&&!(m.mode&0o022),'STOCK_PARENT');}
      const fd=fs.openSync(file,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);try{const m=fs.fstatSync(fd);need(m.isFile()&&m.uid===0&&m.gid===0&&m.nlink===1&&m.size>0&&m.size<=max,'STOCK_FILE');const h=crypto.createHash('sha256'),b=Buffer.alloc(65536);let n=0;for(;;){const q=fs.readSync(fd,b,0,b.length,null);if(!q)break;n+=q;need(n<=m.size&&n<=max,'STOCK_GROWTH');h.update(b.subarray(0,q));}for(const t of [fs.fstatSync(fd),fs.lstatSync(file)])need(t.isFile()&&!t.isSymbolicLink()&&t.dev===m.dev&&t.ino===m.ino&&t.size===n&&t.nlink===1&&t.uid===0&&t.gid===0&&t.mode===m.mode&&t.mtimeMs===m.mtimeMs&&t.ctimeMs===m.ctimeMs,'STOCK_CHANGED');return {path:file,bytes:n,sha256:h.digest('hex')};}finally{fs.closeSync(fd);}};
    const artifacts={node:read('/usr/local/bin/node',536870912),native:read('/opt/rudder/dist/native/rudder-native',536870912),buddy:read('/opt/rudder/dist/native/rudder-jj',536870912),cli:read('/opt/rudder/dist/index.js',4194304),package:read('/opt/rudder/package.json',131072)};
    const info=JSON.parse(fs.readFileSync('/opt/rudder/package.json','utf8'));need(info.version===input.packageVersion,'ROOT_PACKAGE_VERSION');
    report.artifacts=artifacts;report.packageVersion=info.version;report.node={version:process.version,resolvedPath:fs.realpathSync(process.execPath),sha256:artifacts.node.sha256};
    const base='/workspace/stock23-'+input.nonce;fs.mkdirSync(base,{mode:0o700});fs.chownSync(base,1500,1500);
    for(const name of ['home','tmp','rudder','codex','claude','cache','config','data']){fs.mkdirSync(base+'/'+name,{mode:0o700});fs.chownSync(base+'/'+name,1500,1500);}
    for(const name of ['gitconfig','jj-config.toml','npmrc']){const fd=fs.openSync(base+'/'+name,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW,0o600);try{fs.fchownSync(fd,1500,1500);}finally{fs.closeSync(fd);}}
    const env={...flags,HOME:base+'/home',TMPDIR:base+'/tmp',RUDDER_HOME:base+'/rudder',CODEX_HOME:base+'/codex',CLAUDE_CONFIG_DIR:base+'/claude',
      XDG_CACHE_HOME:base+'/cache',XDG_CONFIG_HOME:base+'/config',XDG_DATA_HOME:base+'/data',
      GIT_CONFIG_GLOBAL:base+'/gitconfig',GIT_CONFIG_SYSTEM:base+'/gitconfig',GIT_CONFIG_NOSYSTEM:'1',JJ_CONFIG:base+'/jj-config.toml',
      npm_config_userconfig:base+'/npmrc',npm_config_globalconfig:base+'/npmrc',NODE_OPTIONS:'--max-old-space-size=384',CI:'1',TERM:'dumb',TZ:'UTC'};
    const data={...input,artifacts,flags,envKeys:Object.keys(env).sort()};
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
    try{report.smoke=JSON.parse(out.toString('utf8').trim());}catch{report.failure='UID1500_INVALID_JSON';}
    need(report.child.exitObserved&&report.child.settled&&report.child.code===0&&report.child.signal===null&&report.child.failure===null,'UID1500_CHILD_FAILED');
    need(report.smoke?.status==='passed'&&report.smoke.nonce===input.nonce,'UID1500_SMOKE_REJECTED');
    report.qualified=true;
  }catch(error){report.failure ||= /^[A-Z0-9_]+$/.test(error?.code||'')?error.code:'STOCK_GUEST_REJECTED';}
  saveProof();
  // The bounded fuse remains armed. Only the exact-instance controller owns
  // destruction; an application exit is never substituted for VM cleanup.
  await new Promise(()=>{});
}
