// Bounded flyctl runner adapted from the reviewed source5 command helper.
// It emits no provider stdout/stderr. The controller retains safe summaries.
import {spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
const FLY='/workspace/home/.fly/bin/flyctl';
export async function command(args, {timeout,stdoutLimit=1048576}) {
  return new Promise(resolve=>{
    const child=spawn(FLY,args,{stdio:['ignore','pipe','pipe']});
    let out=Buffer.alloc(0),err=Buffer.alloc(0),outBytes=0,errBytes=0,status,signal,failure,ended=false,settle;
    const outHash=createHash('sha256'),errHash=createHash('sha256');
    const stop=code=>{
      if(ended)return; failure ||= code;
      // ChildProcess.kill does not signal a process group after its leader was
      // observed exited. Unsettled inherited pipes never count as success.
      if(status===undefined&&signal===undefined)child.kill('SIGKILL');
      settle ||= setTimeout(()=>finish(false),2000);
    };
    const timer=setTimeout(()=>stop('CLI_DEADLINE'),timeout);
    child.stdout.on('data',bytes=>{if(ended)return;outHash.update(bytes);outBytes+=bytes.length;
      out=Buffer.concat([out,bytes.subarray(0,Math.max(0,stdoutLimit-out.length))]);if(outBytes>stdoutLimit)stop('CLI_STDOUT_BOUND');});
    child.stderr.on('data',bytes=>{if(ended)return;errHash.update(bytes);errBytes+=bytes.length;
      err=Buffer.concat([err,bytes.subarray(0,Math.max(0,8192-err.length))]);if(errBytes>8192)stop('CLI_STDERR_BOUND');});
    child.on('error',()=>{failure ||= 'CLI_SPAWN';});
    child.on('exit',(code,sig)=>{status=code;signal=sig;});
    const finish=settled=>{
      if(ended)return;ended=true;clearTimeout(timer);clearTimeout(settle);
      if(!settled){child.stdout.destroy();child.stderr.destroy();child.unref();}
      resolve({ok:settled&&!failure&&status===0&&!signal,stdout:out.toString('utf8'),
        diagnostics:{settled,status:status??null,signal:signal??null,failure:failure||null,
          stdoutBytes:outBytes,stderrBytes:errBytes,stdoutSha256:outHash.digest('hex'),stderrSha256:errHash.digest('hex')}});
    };
    child.on('close',()=>finish(true));
  });
}
