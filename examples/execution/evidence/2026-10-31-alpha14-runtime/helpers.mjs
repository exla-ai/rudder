// PRIVATE copied reviewed helpers; source SHA is recorded in preparation.json.
import fs from "node:fs/promises";
import assert from "node:assert/strict";
import {spawn} from "node:child_process";
import {createHash} from "node:crypto";
const FLY="/workspace/home/.fly/bin/flyctl",MAX_STDERR=8192;
export function parseProcStatus(text) {
  const wanted = ['Uid','Gid','Groups','NoNewPrivs','CapEff','CapPrm','CapInh','CapAmb','CapBnd'];
  const fields = new Map();
  for (const match of text.matchAll(/^([A-Za-z]+):[ \t]*([^\r\n]*)$/gm)) {
    if (wanted.includes(match[1])) fields.set(match[1],match[2].trim());
  }
  return Object.fromEntries(wanted.map(key => [key,fields.get(key)]));
}

export function heldNamespaceWitness(fs, assert, kind) {
  assert(['user','net','mnt'].includes(kind));
  const fd = fs.openSync('/proc/self/ns/' + kind,fs.constants.O_RDONLY);
  try {
    const before = fs.fstatSync(fd,{bigint:true});
    assert.equal(fs.statfsSync('/proc/self/fd/' + fd).type,0x6e736673);
    const after = fs.fstatSync(fd,{bigint:true});
    assert.equal(after.dev,before.dev); assert.equal(after.ino,before.ino);
    return {fd,device:before.dev.toString(),inode:before.ino.toString()};
  } catch(error) {fs.closeSync(fd); throw error;}
}

export function privateErrorDetail(error) {
  const source = String(error?.stack || error);
  const compact = source.replace(/data:[^\s,]+;base64,[A-Za-z0-9+/=]+/g,'data:<inline-preload>');
  return {raw:Buffer.from(source),formatted:Buffer.from(compact),shortened:compact !== source};
}

export function gateSource(input) {
  return `const fs=require('node:fs'),crypto=require('node:crypto');const target=${JSON.stringify(input.gatePath)},tmp=target+'.pending-'+crypto.randomUUID();
const fd=fs.openSync(tmp,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK,0o600);
try{fs.writeFileSync(fd,${JSON.stringify(JSON.stringify(input.gate))});fs.fchownSync(fd,1500,1500);fs.fsyncSync(fd);}finally{fs.closeSync(fd);}
try{fs.linkSync(tmp,target);}finally{fs.unlinkSync(tmp);}const directory=fs.openSync('/workspace',fs.constants.O_RDONLY|fs.constants.O_DIRECTORY);try{fs.fsyncSync(directory);}finally{fs.closeSync(directory);}
console.log(JSON.stringify({nonce:${JSON.stringify(input.nonce)},published:true}));`;
}



export function parseOwnedExecJson(stdout, step) {
  const fail = (code, guestExitCode) => {
    step.ok = false; step.failure = code; step.validationFailure = code;
    if (guestExitCode !== undefined) step.guestExitCode = guestExitCode;
    throw Object.assign(new Error('owned exec rejected; inspect its private pipe files'), {code});
  };
  for (const match of stdout.matchAll(/^Exit code:[ \t]*(-?[0-9]{1,10})[ \t]*\r?$/gm)) {
    const code = Number(match[1]); if (code !== 0) fail('OWNED_GUEST_EXIT_NONZERO', code);
  }
  let value;
  try {value = JSON.parse(stdout.trim());} catch {fail('OWNED_EXEC_INVALID_JSON');}
  if (value?.status === 'failed') fail('OWNED_GUEST_REPORTED_FAILURE');
  step.jsonValidated = true; return value;
}



export async function preserveOwnedExecOutput(stdout, stderr, stdoutPath, stderrPath, write = fs.writeFile) {
  await write(stdoutPath, stdout, {mode: 0o600, flag: 'wx'});
  await write(stderrPath, stderr, {mode: 0o600, flag: 'wx'});
  return {privateStdoutFile: stdoutPath, privateStderrFile: stderrPath,
    retainedStdoutBytes: stdout.length, retainedStderrBytes: stderr.length};
}



export function proofReadSource(input) {
  return `const fs=require('node:fs'),crypto=require('node:crypto');const p=${JSON.stringify(input.proofPath)};let fd;
try{fd=fs.openSync(p,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);const m=fs.fstatSync(fd);if(!m.isFile()||m.nlink!==1||m.uid!==1500||m.gid!==1500||(m.mode&0o7777)!==0o600||m.size>1048576)throw Error('unsafe-proof');
const b=Buffer.alloc(1048577);let n=0;for(;;){const q=fs.readSync(fd,b,n,b.length-n,null);if(!q)break;n+=q;if(n>1048576)throw Error('oversize-proof');}const end=fs.fstatSync(fd),named=fs.lstatSync(p);for(const x of [end,named])if(!x.isFile()||x.isSymbolicLink()||x.dev!==m.dev||x.ino!==m.ino||x.uid!==1500||x.gid!==1500||x.mode!==m.mode||x.nlink!==1||x.size!==n)throw Error('changed-proof');console.log(JSON.stringify({status:'ready',sha256:crypto.createHash('sha256').update(b.subarray(0,n)).digest('hex'),proof:JSON.parse(b.subarray(0,n))}));}
catch(e){console.log(JSON.stringify({status:e.code==='ENOENT'?'pending':'rejected'}));}finally{if(fd!==undefined)fs.closeSync(fd);}`;
}

export async function command(args, {timeout, stdoutLimit, diagnosticPath, ownedStdoutPath}) {
  return new Promise(resolve => {
    const child = spawn(FLY, args, {stdio: ['ignore', 'pipe', 'pipe']});
    let status, signal, failure, ended = false, settleTimer, stdout = Buffer.alloc(0), stderr = Buffer.alloc(0), stdoutBytes = 0, stderrBytes = 0;
    const stdoutHash = createHash('sha256'), stderrHash = createHash('sha256');
    const stop = code => {
      if (ended) return;
      failure ||= code; child.kill('SIGKILL');
      settleTimer ||= setTimeout(() => {failure ||= 'CLI_SETTLEMENT_DEADLINE'; finish(false);}, 2000);
    };
    const timer = setTimeout(() => stop('DEADLINE'), timeout);
    child.stdout.on('data', bytes => {
      if (ended) return;
      stdoutHash.update(bytes); stdoutBytes += bytes.length;
      const keep = bytes.subarray(0, Math.max(0, stdoutLimit - stdout.length)); stdout = Buffer.concat([stdout, keep]);
      if (stdoutBytes > stdoutLimit) stop('STDOUT_BOUND');
    });
    child.stderr.on('data', bytes => {
      if (ended) return;
      stderrHash.update(bytes); stderrBytes += bytes.length;
      stderr = Buffer.concat([stderr, bytes.subarray(0, Math.max(0, MAX_STDERR - stderr.length))]);
      if (stderrBytes > MAX_STDERR) stop('STDERR_BOUND');
    });
    child.on('error', () => {failure ||= 'SPAWN';});
    child.on('exit', (code, sig) => {status = code; signal = sig;});
    const finish = async settled => {
      if (ended) return;
      ended = true; clearTimeout(timer); if (settleTimer) clearTimeout(settleTimer);
      if (!settled) {child.stdout.destroy(); child.stderr.destroy(); child.unref();}
      const diagnostics = {status, signal, failure: failure || null, settled, stdoutBytes, stderrBytes,
        stdoutSha256: stdoutHash.digest('hex'), stderrSha256: stderrHash.digest('hex'),
        retainedStdoutBytes: stdout.length, retainedStderrBytes: stderr.length};
      try {
        if (ownedStdoutPath) Object.assign(diagnostics,
          await preserveOwnedExecOutput(stdout, stderr, ownedStdoutPath, diagnosticPath));
        else if (failure || status !== 0) {
          await fs.writeFile(diagnosticPath, stderr, {mode: 0o600, flag: 'wx'});
          diagnostics.privateStderrFile = diagnosticPath;
        }
        resolve({ok: settled && !failure && status === 0 && !signal, stdout: stdout.toString('utf8'), diagnostics});
      } catch {resolve({ok: false, stdout: '', diagnostics: {...diagnostics, failure: 'SAVE_PRIVATE_DIAGNOSTICS'}});}
    };
    child.on('close', () => finish(true));
  });
}
