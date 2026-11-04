// Synthetic loopback ONLY. No client, credentials, model, subprocess or retry.
import http from 'node:http';
import {createHash,randomUUID} from 'node:crypto';
import readline from 'node:readline';
import assert from 'node:assert/strict';
const [nonce,project]=process.argv.slice(2);
assert.match(nonce,/^[a-f0-9-]{36}$/);assert(project.startsWith('/'));
let count=0,primary=0,output=0,stopping=false;
const emit=value=>{const line=JSON.stringify(value)+'\n';output+=Buffer.byteLength(line);assert(output<=262144);process.stdout.write(line);};
const held=new Set(),sockets=new Set();
const server=http.createServer(async(req,res)=>{let phase='request-headers';try{
 assert.equal(req.socket.remoteAddress,'127.0.0.1');assert.equal(req.method,'POST');
 assert.equal(req.headers['x-api-key'],'sk-ant-api03-loopback-fixture-no-billing');
 const route=new URL(req.url,'http://127.0.0.1').pathname;assert(['/v1/messages','/v1/messages/count_tokens'].includes(route));
 phase='bounded-request-body';let length=0,chunks=[];for await(const b of req){length+=b.length;assert(length<=2*1024*1024);chunks.push(b);}
 const raw=Buffer.concat(chunks),body=JSON.parse(raw);assert(count++<32);
 const text=JSON.stringify(body.messages??[]),main=text.includes('RUDDER_ACTUAL_CLAUDE_NATIVE'),peer=text.includes('RUDDER_CLAUDE_NATIVE_PEER_'+nonce);
 if(main&&route==='/v1/messages')primary++;const primaryIndex=primary;
 emit({kind:'request',route,index:count,primary:main&&route==='/v1/messages'?primary:null,bytes:length,sha256:createHash('sha256').update(raw).digest('hex'),sawPeer:peer,remoteAddress:req.socket.remoteAddress});
 if(route==='/v1/messages/count_tokens'){res.writeHead(200,{'content-type':'application/json'});res.end('{"input_tokens":100}');return;}
 if(main&&primary>1){held.add(res);res.once('close',()=>{held.delete(res);emit({kind:'responseClosed',primary:primaryIndex,sawPeer:peer,fixtureEnded:false});});emit({kind:'held',primary:primaryIndex,sawPeer:peer});return;}
 phase='actual-Write-tool';const tool=(body.tools??[]).find(t=>t.name==='Write');assert(!main||tool,'actual Write tool unavailable');
 const block=main?{type:'tool_use',id:'toolu_rudder_native_fixture',name:tool.name,input:{}}:{type:'text',text:''};
 const event=(type,data)=>res.write('event: '+type+'\ndata: '+JSON.stringify({type,...data})+'\n\n');
 res.writeHead(200,{'content-type':'text/event-stream','cache-control':'no-cache'});
 event('message_start',{message:{id:'msg_'+randomUUID().replaceAll('-',''),type:'message',role:'assistant',model:body.model,content:[],stop_reason:null,stop_sequence:null,usage:{input_tokens:100,output_tokens:1}}});
 event('content_block_start',{index:0,content_block:block});
 event('content_block_delta',{index:0,delta:main?{type:'input_json_delta',partial_json:JSON.stringify({file_path:project+'/fixture.txt',content:'actual-claude-native-tool-edit\n'})}:{type:'text_delta',text:'loopback auxiliary response'}});
 event('content_block_stop',{index:0});event('message_delta',{delta:{stop_reason:main?'tool_use':'end_turn',stop_sequence:null},usage:{output_tokens:30}});event('message_stop',{});res.end();
 }catch{emit({kind:'failure',code:'SYNTHETIC_PROVIDER_PROTOCOL_REJECTED',phase});res.destroy();}});
server.on('connection',s=>{sockets.add(s);s.once('close',()=>sockets.delete(s));});
server.on('connect',(_req,s)=>{emit({kind:'failure',code:'EXTERNAL_PROXY_ATTEMPT'});s.destroy();});
const stop=async()=>{if(stopping)return;stopping=true;for(const r of held)r.destroy();for(const s of sockets)s.destroy();await new Promise(resolve=>server.close(resolve));emit({kind:'stopped',requests:count});};
await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
emit({kind:'ready',baseUrl:'http://127.0.0.1:'+server.address().port,node:process.version});
const timer=setTimeout(()=>{emit({kind:'failure',code:'PROVIDER_ORIGINAL_DEADLINE'});void stop();process.exitCode=1;},90000);
const lines=readline.createInterface({input:process.stdin,crlfDelay:Infinity});
try{for await(const line of lines){assert(Buffer.byteLength(line)<=4096);assert.deepEqual(JSON.parse(line),{op:'stop'});break;}}
finally{lines.close();await stop();clearTimeout(timer);await new Promise(resolve=>process.stdout.write('',resolve));}
