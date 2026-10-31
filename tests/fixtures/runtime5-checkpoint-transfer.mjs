// Opt-in actual checkpoint DATA transfer. Import/default performs no work.
import fs from 'node:fs/promises';
import net from 'node:net';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {fileURLToPath,pathToFileURL} from 'node:url';
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
const need=ok=>{if(!ok)throw new Error('RUNTIME5_TRANSFER_REJECTED');};
const absolute=value=>typeof value==='string'&&path.isAbsolute(value)&&path.resolve(value)===value&&!/[\0\r\n]/.test(value);
const same=(a,b)=>a?.kind===b?.kind&&a?.algorithm===b?.algorithm&&a?.digest===b?.digest&&a?.size_bytes===b?.size_bytes;
async function privateDirectory(directory){
  need(absolute(directory)&&await fs.realpath(directory)===directory);
  const m=await fs.lstat(directory);need(m.isDirectory()&&!m.isSymbolicLink()&&m.uid===1500&&m.gid===1500&&(m.mode&0o7777)===0o700);
}

/** Root's executable wrapper awaits bridge(socketPath); argv stays fabric rpc. */
export async function bridge(socketPath){
  try{
    need(JSON.stringify(process.argv.slice(2))===JSON.stringify(['fabric','rpc']));
    need(process.getuid()===1500&&absolute(socketPath));await privateDirectory(path.dirname(socketPath));
    const named=await fs.lstat(socketPath);need(named.isSocket()&&!named.isSymbolicLink()&&named.uid===1500&&named.gid===1500&&(named.mode&0o7777)===0o600);
    await new Promise((resolve,reject)=>{
      const socket=net.createConnection({path:socketPath,allowHalfOpen:true});
      let inputEnded=false,peerEnded=false,closed=false,flushed=false,failed=false,done=false,settlement;
      const finish=()=>{if(done||!closed||(!failed&&!flushed))return;done=true;clearTimeout(fuse);clearTimeout(settlement);
        process.stdin.unpipe(socket);socket.unpipe(process.stdout);
        if(failed||!inputEnded||!peerEnded)reject(new Error('RUNTIME5_BRIDGE_FAILED'));else resolve();};
      const fail=()=>{if(done)return;failed=true;process.stdin.unpipe(socket);socket.unpipe(process.stdout);
        process.stdin.destroy();socket.destroy();process.stdout.destroy();
        settlement ||= setTimeout(()=>{if(!done){done=true;clearTimeout(fuse);reject(new Error('RUNTIME5_BRIDGE_UNSETTLED'));}},2000);finish();};
      const fuse=setTimeout(fail,120000);
      process.stdin.once('end',()=>{inputEnded=true;});process.stdin.once('error',fail);
      process.stdin.once('close',()=>{if(!inputEnded)fail();});process.stdout.once('error',fail);
      socket.once('error',fail);socket.once('end',()=>{peerEnded=true;if(!inputEnded)return fail();
        process.stdout.write('',()=>{flushed=true;finish();});});
      socket.once('close',error=>{closed=true;if(error||!peerEnded)fail();finish();});
      socket.once('connect',()=>{process.stdin.pipe(socket);socket.pipe(process.stdout,{end:false});});
    });
  }catch{process.exitCode=1;throw new Error('RUNTIME5_BRIDGE_FAILED');}
}

export async function main(inputPath){
  if(inputPath===undefined)return {status:'prepared-no-run'};
  let input,resultFile,local,timer,expired=false,proof={format_version:1,status:'failed',stage:'input',model_calls:false,accepted_history:false};
  const abort=new AbortController();
  try{
    need(process.getuid()===1500&&process.getgid()===1500&&absolute(inputPath));
    const file=await fs.open(inputPath,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);
    try{const m=await file.stat();need(m.isFile()&&m.uid===1500&&m.gid===1500&&m.nlink===1&&(m.mode&0o7777)===0o600&&m.size>0&&m.size<=65536);
      const buffer=Buffer.alloc(65537);let length=0;for(;;){const part=await file.read(buffer,length,buffer.length-length,null);if(!part.bytesRead)break;length+=part.bytesRead;need(length<=65536);}
      const bytes=buffer.subarray(0,length),after=await file.stat();need(bytes.length===m.size&&after.size===m.size&&after.mtimeMs===m.mtimeMs&&after.ctimeMs===m.ctimeMs);input=JSON.parse(bytes.toString('utf8'));}
    finally{await file.close();}
    const keys=['format_version','base_url','auth_headers','scope','publication_id','receipt_json','bridge_path','result_path','local_module_path','remote_module_path'];
    need(input&&JSON.stringify(Object.keys(input).sort())===JSON.stringify(keys.sort())&&input.format_version===1);
    need(input.local_module_path==='/opt/rudder/dist/fabric-local.js'&&input.remote_module_path==='/opt/rudder/dist/fabric-remote.js');
    need(typeof input.receipt_json==='string'&&input.receipt_json.length>0&&Buffer.byteLength(input.receipt_json)<=65536);
    need(/^pcd1-[a-f0-9]{64}$/.test(input.publication_id||''));
    const base=new URL(input.base_url);need(base.protocol==='https:'&&!base.username&&!base.password&&!base.search&&!base.hash&&base.pathname.endsWith('/api/rudder/fabric')&&input.base_url===input.base_url.trim());
    need(input.scope&&Object.keys(input.scope).length===2&&['provider_id','tenant_id'].every(k=>/^[A-Za-z0-9._:-]{1,128}$/.test(input.scope[k]||'')));
    need(input.auth_headers&&typeof input.auth_headers==='object'&&!Array.isArray(input.auth_headers));const headers={};
    for(const [key,value]of Object.entries(input.auth_headers)){const name=key.toLowerCase();need(['authorization','x-rudder-workspace-id'].includes(name)&&!Object.hasOwn(headers,name)&&typeof value==='string'&&value.length>0&&value.length<=8192&&!/[\r\n\0]/.test(value));headers[name]=value;}
    need(headers.authorization&&absolute(input.bridge_path)&&absolute(input.result_path));
    const wrapper=await fs.lstat(input.bridge_path);need(wrapper.isFile()&&!wrapper.isSymbolicLink()&&wrapper.uid===1500&&wrapper.nlink===1&&(wrapper.mode&0o111)!==0&&(wrapper.mode&0o022)===0);
    await privateDirectory(path.dirname(input.bridge_path));await privateDirectory(path.dirname(input.result_path));
    await privateDirectory(process.env.HOME);await privateDirectory(process.env.TMPDIR);
    resultFile=await fs.open(input.result_path,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW,0o600);
    proof.publication_id=input.publication_id;proof.receipt_sha256=hash(Buffer.from(input.receipt_json));
    timer=setTimeout(()=>{expired=true;abort.abort();void local?.close().catch(()=>{});},110000);
    proof.stage='modules';
    for(const modulePath of [input.local_module_path,input.remote_module_path])need(await fs.realpath(modulePath)===modulePath);
    const {FabricLocalClient}=await import(pathToFileURL(input.local_module_path).href);
    const {FabricRemoteClient}=await import(pathToFileURL(input.remote_module_path).href);
    proof.stage='local_rpc';
    need(!expired);local=await FabricLocalClient.open({nativeBinary:input.bridge_path,
      env:{PATH:'/usr/local/bin:/usr/bin:/bin',HOME:process.env.HOME,TMPDIR:process.env.TMPDIR},requestTimeoutMs:30000});
    const inventories=[],methods=[];let reads=0,lost=false,replySettled=false,replyReceipt;
    const source={pinPartialCheckpoint:async receipt=>{need(receipt===input.receipt_json);const value=await local.pinPartialCheckpoint(receipt);need(Buffer.byteLength(JSON.stringify(value))<=524288);inventories.push(value);return value;},
      read:reference=>{reads++;return local.read(reference);},unpin:pin=>local.unpin(pin)};
    const target=new URL(input.base_url+'/roots/'+encodeURIComponent(input.publication_id));
    const fetch=async(url,request)=>{
      need(!expired);const method=request.method||'GET';methods.push(method);
      const response=await globalThis.fetch(url,{...request,signal:AbortSignal.any([abort.signal,...(request.signal?[request.signal]:[])])});
      if(!lost&&method==='POST'&&new URL(url).href===target.href&&response.ok){
        const reader=response.body?.getReader();need(reader);const chunks=[];let count=0;
        try{for(;;){const part=await reader.read();if(part.done)break;count+=part.value.byteLength;
          if(count>65536){await reader.cancel();throw new Error('RUNTIME5_RESPONSE_BOUND');}chunks.push(Buffer.from(part.value));}}
        finally{reader.releaseLock();}
        const bytes=Buffer.concat(chunks),receipt=JSON.parse(bytes.toString('utf8'));
        need(receipt.root_id===input.publication_id&&same(receipt.root,inventories[0]?.publication)&&receipt.durability==='replicated');
        replyReceipt=receipt;replySettled=true;lost=true;
        // The real successful HTTP body reached EOF. Only the caller's reply
        // is discarded; publication identity and producer bytes stay original.
        throw new Error('RUNTIME5_DELIBERATELY_LOST_REPLY');
      }
      return response;
    };
    const remote=new FabricRemoteClient({baseUrl:input.base_url,scope:input.scope,authHeaders:()=>headers,fetch,concurrency:1,requestTimeoutMs:8000});
    proof.stage='first_upload';
    let firstRejected=false;try{await remote.uploadPartialCheckpoint(input.publication_id,input.receipt_json,source);}catch{firstRejected=true;}
    need(firstRejected&&lost&&replySettled&&!expired&&inventories.length===1);
    proof.stage='reconciliation';
    const before=methods.length,readBefore=reads;
    const result=await remote.uploadPartialCheckpoint(input.publication_id,input.receipt_json,source);
    need(!expired&&inventories.length===2&&inventories[0].owned_pin!==inventories[1].owned_pin);
    const strip=({owned_pin,...data})=>data;
    need(JSON.stringify(strip(inventories[0]))===JSON.stringify(strip(inventories[1])));
    const retryMethods=methods.slice(before);need(retryMethods.length>=2&&retryMethods.every(method=>method==='GET')&&reads===readBefore);
    need(result.uploaded_objects===0&&result.uploaded_bytes===0&&!result.cleanup_warning&&result.publication_id===input.publication_id);
    need(['format_version','root_id','durability','published_at_ms','object_count','total_bytes'].every(k=>result.bundle[k]===replyReceipt[k])&&same(result.bundle.root,replyReceipt.root));
    need(same(result.publication,inventories[0].publication)&&same(result.q,inventories[0].q)&&same(result.runnable,inventories[0].runnable));
    Object.assign(proof,{status:'passed',stage:'complete',lost_response_reconciled:true,response_body_settled:true,
      publication:result.publication,runnable:result.runnable,q:result.q,bundle:result.bundle,
      inventory:strip(inventories[0]),retry_http_methods:retryMethods,retry_uploaded_objects:0,retry_uploaded_bytes:0,
      source_reads:reads,original_receipt_unchanged:true,inventories_from_native:true,q_not_rewritten:true});
  }catch{proof.status='failed';proof.failure='RUNTIME5_TRANSFER_REJECTED';}
  finally{
    if(timer)clearTimeout(timer);
    if(local)try{await local.close();proof.local_helper_settled=true;}catch{proof.status='failed';proof.local_helper_settled=false;proof.failure='RUNTIME5_LOCAL_CLOSE_UNCONFIRMED';}
    else proof.local_helper_settled=false;
    if(expired){proof.status='failed';proof.failure='RUNTIME5_TRANSFER_DEADLINE';}
    if(resultFile){try{const bytes=Buffer.from(JSON.stringify(proof)+'\n');need(bytes.length<=1048576);await resultFile.writeFile(bytes);await resultFile.sync();}
      finally{await resultFile.close();}}
  }
  need(resultFile);return proof;
}
if(process.argv[1]&&path.resolve(process.argv[1])===fileURLToPath(import.meta.url)){
  const args=process.argv.slice(2);
  if(args.length){if(args.length===2&&args[0]==='--run')main(args[1]).then(value=>{if(value.status!=='passed')process.exitCode=1;})
    .catch(()=>{console.error('RUNTIME5_TRANSFER_REJECTED');process.exitCode=1;});
    else{console.error('RUNTIME5_TRANSFER_REJECTED');process.exitCode=1;}}
}
