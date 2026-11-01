// Qualification relay only. Imports the EXACT production protocol module.
// Rust owns the actual CLI Child and all pipes; no fabricated CLI lifecycle.
import http from 'node:http';
import fs from 'node:fs/promises';
import readline from 'node:readline';
import {createRequire} from 'node:module';
import {pathToFileURL} from 'node:url';
import assert from 'node:assert/strict';
const [modulePath,socketPath,run,attempt,project]=process.argv.slice(2);
const {ClaudeSdkProtocol,SdkControlError,SDK_PROFILE}=await import(pathToFileURL(modulePath).href);
const {WebSocket,WebSocketServer}=createRequire(pathToFileURL(modulePath))('ws');
let bytes=0,closing=false,peer=null,inFlight=0;
const emit=value=>{const line=JSON.stringify(value)+'\n';bytes+=Buffer.byteLength(line);assert(bytes<=1024*1024);process.stdout.write(line);};
const publish=value=>{if(peer?.readyState===WebSocket.OPEN){assert(peer.bufferedAmount<=1024*1024);peer.send(JSON.stringify(value));}};
const protocol=new ClaudeSdkProtocol(run,attempt,project,frame=>emit({kind:'cli_input',frame}),publish,message=>emit({kind:'notice',message}),10000);
const server=http.createServer((_q,r)=>{r.writeHead(404);r.end();});
const ws=new WebSocketServer({noServer:true,maxPayload:1024*1024,perMessageDeflate:false});
server.on('upgrade',(request,socket,head)=>{if(closing||peer||request.headers.origin){socket.destroy();return;}ws.handleUpgrade(request,socket,head,p=>ws.emit('connection',p));});
ws.on('connection',p=>{peer=p;let initialized=false;p.on('error',()=>{});p.on('close',()=>{if(peer===p)peer=null;});
 p.on('message',raw=>{void(async()=>{let m;try{
  assert(raw.length<=1024*1024);m=JSON.parse(raw);assert(typeof m.method==='string');
  if(m.method==='initialized'&&m.id===undefined)return;
  assert(inFlight<4);inFlight++;
  if(m.method==='initialize'){assert(!initialized);initialized=true;publish({id:m.id,result:{profile:SDK_PROFILE,format_version:1,run_id:run,attempt_id:attempt,steer_mode:'interrupt_then_replace'}});return;}
  assert(initialized);const t=protocol.thread(),params=m.params??{};
  if(m.method==='thread/loaded/list'){publish({id:m.id,result:{data:t?[t.id]:[],rudder_uncertain:protocol.snapshot().uncertain}});return;}
  assert(t&&params.threadId===t.id);
  if(['thread/read','thread/resume'].includes(m.method)){publish({id:m.id,result:{thread:t}});return;}
  if(m.method==='thread/turns/list'){publish({id:m.id,result:{data:t.turns,nextCursor:null}});return;}
  assert(['turn/start','turn/steer','turn/interrupt'].includes(m.method));
  assert(params._rudder&&params._rudder.operation);
  const kind=params._rudder.operation.kind;
  assert(m.method==='turn/interrupt'?kind==='interrupt':m.method==='turn/steer'?kind==='steer':['deliver','resume'].includes(kind));
  assert(m.method!=='turn/interrupt'||params.turnId===params._rudder.expected_turn_id);assert(m.method!=='turn/steer'||params.expectedTurnId===params._rudder.expected_turn_id);
  const result=await protocol.execute(params._rudder,String(params.threadId));publish({id:m.id,result});
 }catch(error){const kind=error instanceof SdkControlError?error.kind:'uncertain';publish(kind==='uncertain'?{id:m?.id,result:{rudderUncertain:'fixture relay operation failed'}}:{id:m?.id,error:{code:kind==='unsupported'?-32011:-32010,message:'original protocol control refused'}});emit({kind:'rpcFailure',code:kind});}
 finally{inFlight=Math.max(0,inFlight-1);}})();});});
await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(socketPath,resolve);});await fs.chmod(socketPath,0o600);emit({kind:'ready'});
const lines=readline.createInterface({input:process.stdin,crlfDelay:Infinity});
const timer=setTimeout(()=>{emit({kind:'failure',code:'BRIDGE_ORIGINAL_DEADLINE'});process.exitCode=1;lines.close();},90000);
const initialized=protocol.initialize().then(()=>protocol.humanInput('RUDDER_ACTUAL_CLAUDE_NATIVE Write fixture.txt using Write, then await further instructions.')).catch(()=>{emit({kind:'failure',code:'ORIGINAL_PROTOCOL_INITIALIZATION'});});
try{for await(const line of lines){assert(Buffer.byteLength(line)<=2*1024*1024);const value=JSON.parse(line);
 if(value.op==='stop'){assert.deepEqual(value,{op:'stop'});break;}
 assert(value.op==='cli_frame'&&value.frame&&Object.keys(value).length===2);protocol.ingest(value.frame);emit({kind:'snapshot',snapshot:protocol.snapshot()});
 }}finally{closing=true;protocol.close();peer?.terminate();ws.close();await new Promise(resolve=>server.close(resolve));lines.close();clearTimeout(timer);await initialized;await new Promise(resolve=>process.stdout.write('',resolve));}
