// Fixture-only subset of the sealed actual-Codex synthetic provider.
// Import is pure. No outbound HTTP client, credentials, model or subprocess.
import http from 'node:http';
import {createHash} from 'node:crypto';
import {createInterface} from 'node:readline';
import {pathToFileURL} from 'node:url';
import assert from 'node:assert/strict';

export const FIXTURE_KEY='synthetic-loopback-fixture-not-a-credential';
export const sha=value=>createHash('sha256').update(value).digest('hex');
export const completed=(id,output=[])=>({type:'response.completed',response:{id,object:'response',status:'completed',output,usage:{input_tokens:0,output_tokens:0,total_tokens:0}}});
export function toolFor(body){const all=[];for(const tool of body.tools||[]){
  if(tool.type==='namespace')for(const child of tool.tools||[])all.push({...child,namespace:tool.name});else all.push(tool);
}return all.find(t=>t.type==='function'&&['exec_command','shell_command'].includes(t.name));}
const message=(id,text)=>({type:'message',id,role:'assistant',status:'completed',content:[{type:'output_text',text,annotations:[]}]});
const event=(response,value)=>response.write('event: '+value.type+'\ndata: '+JSON.stringify(value)+'\n\n');

export async function serve(nonce){
  assert(/^[a-f0-9-]{36}$/.test(nonce));let output=0,stopping=false,closed=false;
  const emit=value=>{const text=JSON.stringify(value)+'\n';output+=Buffer.byteLength(text);assert(output<=262144,'OUTPUT_BOUND');process.stdout.write(text);};
  const held=new Map(),receipts=[];let failure=null;
  const server=http.createServer(async(req,res)=>{try{
    assert(req.method==='POST'&&req.url==='/v1/responses'&&req.socket.remoteAddress==='127.0.0.1');
    assert.equal(req.headers.authorization,'Bearer '+FIXTURE_KEY);
    let bytes=0,chunks=[];for await(const chunk of req){bytes+=chunk.length;assert(bytes<=1048576);chunks.push(chunk);}
    const raw=Buffer.concat(chunks),body=JSON.parse(raw),index=receipts.length;assert(index<6);
    assert(body.model==='fixture-model'&&body.stream===true&&Array.isArray(body.input));
    const sawSteer=JSON.stringify(body.input).includes('RUDDER_NATIVE_FIXTURE_STEER_'+nonce);
    const receipt={index,bytes,sha256:sha(raw),remoteAddress:req.socket.remoteAddress,sawSteer};receipts.push(receipt);emit({kind:'request',...receipt});
    res.writeHead(200,{'content-type':'text/event-stream','cache-control':'no-cache'});
    event(res,{type:'response.created',response:{id:'fixture-'+index,object:'response',status:'in_progress',output:[]}});
    if(index===0){
      const tool=toolFor(body);assert(tool,'NO_REAL_COMMAND_TOOL');
      const args=tool.name==='exec_command'?{cmd:"printf 'actual-codex-tool-edit\\n' > fixture.txt",yield_time_ms:1000,max_output_tokens:1024}
        :{command:"printf 'actual-codex-tool-edit\\n' > fixture.txt",timeout_ms:3000};
      const call={type:'function_call',id:'fc_fixture',call_id:'call_fixture',name:tool.name,arguments:JSON.stringify(args),...(tool.namespace?{namespace:tool.namespace}:{})};
      event(res,{type:'response.output_item.done',output_index:0,item:call});event(res,completed('fixture-0',[call]));res.end();
    }else if(index===1){
      assert(body.input.some(i=>i.type==='function_call_output'&&i.call_id==='call_fixture'));
      const item=message('fixture-edited','Synthetic provider acknowledges the actual tool edit.');
      event(res,{type:'response.output_item.done',output_index:0,item});event(res,completed('fixture-1',[item]));res.end();
    }else{
      if(index===3)assert(sawSteer,'NATIVE_PEER_CONTEXT_NOT_IN_PROVIDER_INPUT');
      let fixtureEnded=false;held.set(index,{res,finish:()=>{fixtureEnded=true;const item=message('fixture-held-'+index,'Continue after native steering.');
        event(res,{type:'response.output_item.done',output_index:0,item});event(res,completed('fixture-'+index,[item]));res.end();}});
      res.once('close',()=>emit({kind:'responseClosed',index,fixtureEnded}));emit({kind:'held',index});
    }
  }catch{failure='SYNTHETIC_PROVIDER_PROTOCOL_REJECTED';emit({kind:'failure',code:failure});res.destroy();}});
  server.on('connect',(_req,socket)=>{failure='EXTERNAL_PROXY_ATTEMPT_REJECTED';emit({kind:'failure',code:failure});socket.destroy();});
  await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
  const stop=async()=>{if(stopping)return;stopping=true;for(const entry of held.values())entry.res.destroy();server.closeAllConnections();
    await new Promise(resolve=>server.close(resolve));closed=true;emit({kind:'stopped',requests:receipts.length,failure});};
  const timer=setTimeout(()=>{failure='PROVIDER_DEADLINE';stop().catch(()=>{});process.exitCode=1;},70000);
  emit({kind:'ready',baseUrl:'http://127.0.0.1:'+server.address().port,node:process.version});
  const input=createInterface({input:process.stdin,crlfDelay:Infinity});
  try{for await(const line of input){assert(Buffer.byteLength(line)<=4096);const control=JSON.parse(line);
    if(control.op==='finishHeld'){const entry=held.get(control.index);assert(entry);entry.finish();}
    else if(control.op==='stop'){await stop();break;}else throw Error('UNKNOWN_CONTROL');
  }}catch{failure='PROVIDER_CONTROL_REJECTED';process.exitCode=1;}finally{
    input.close();await stop();clearTimeout(timer);assert(closed);
    await new Promise(resolve=>process.stdout.write('',resolve));
  }
}
if(process.argv[1]&&import.meta.url===pathToFileURL(process.argv[1]).href){
  assert(process.argv[2]==='--serve');serve(process.argv[3]).catch(()=>{process.stderr.write('SYNTHETIC_PROVIDER_FAILED\n');process.exitCode=1;});
}
