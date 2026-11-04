// Real App + PTYs + attach view + engine journal. Providers implement typed
// deterministic protocols; these tests make no model/auth/network requests.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import readline from 'node:readline';
import test from 'node:test';
import { launch } from 'tui-integration-tests';
import { nativeBin, repoRoot, scratchRepo, removeScratch, waitUntil } from './helpers.mjs';

const fakeClaude = `#!/usr/bin/env node
const fs=require('node:fs'),rl=require('node:readline');
fs.writeFileSync('claude-broker.json',JSON.stringify({identity:process.env.RUDDER_FABRIC_IDENTITY,socket:process.env.RUDDER_FABRIC_BROKER_SOCKET}));
const sid=process.argv[process.argv.indexOf('--session-id')+1]||'fixture-session';
const put=x=>process.stdout.write(JSON.stringify(x)+'\\n');let active,initialized=false;
rl.createInterface({input:process.stdin}).on('line',raw=>{const x=JSON.parse(raw);
 if(x.type==='control_request'&&x.request.subtype==='initialize')put({type:'control_response',response:{subtype:'success',request_id:x.request_id,response:{claude_code_version:'2.1.314',commands:[],models:[],agents:[]}}});
 if(x.type==='user'){
  if(!initialized){initialized=true;put({type:'system',subtype:'init',session_id:sid,claude_code_version:'2.1.314',capabilities:['interrupt_receipt_v1','interrupt_cancel_queued_v1']});}
  active=x;fs.appendFileSync('provider-user-frames.ndjson',JSON.stringify({uuid:x.uuid,origin:x.origin,content:x.message.content})+'\\n');
  put({...x,isReplay:true,session_id:sid});put({type:'assistant',session_id:sid,user_message_uuid:x.uuid,parent_tool_use_id:null,message:{role:'assistant',content:[{type:'text',text:'ENGINE_SDK_READY'}]}});
  if(x.origin?.kind==='peer')setTimeout(()=>{if(active?.uuid===x.uuid){put({type:'result',subtype:'success',is_error:false,session_id:sid,user_message_uuid:x.uuid,terminal_reason:'completed',origin:{kind:'peer',from:x.origin.from}});active=null;}},50);
 }
 if(x.type==='control_request'&&x.request.subtype==='interrupt'){
  fs.appendFileSync('provider-control-frames.ndjson',JSON.stringify(x.request)+'\\n');
  if(x.request.cancel_queued!==true)throw Error('queue cancellation required');
  put({type:'control_response',response:{subtype:'success',request_id:x.request_id,response:{still_queued:[],cancelled:[]}}});
  const old=active;setTimeout(()=>{if(old){put({type:'result',subtype:'error_during_execution',is_error:true,session_id:sid,user_message_uuid:old.uuid,terminal_reason:'aborted_tools',origin:old.origin});active=null;}},15);
 }
});
`;

async function fixture(t) {
  try { await fs.access(nativeBin); } catch { if(process.env.CI)assert.fail('runtime journey requires current native artifact');t.skip('build native binary');return; }
  const dir=await scratchRepo('rudder-live-runtime-'),home=path.join(dir,'.test-home'),bin=path.join(dir,'fake-bin');
  await fs.mkdir(bin);await fs.mkdir(home,{mode:0o700});
  const claude=path.join(bin,'claude');await fs.writeFile(claude,fakeClaude,{mode:0o755});
  const wsModule=path.join(repoRoot,'node_modules/ws/index.js');
  const codex=path.join(bin,'codex');
  await fs.writeFile(codex,`#!/usr/bin/env node
const fs=require('node:fs'),http=require('node:http'),{WebSocketServer}=require(${JSON.stringify(wsModule)});
fs.writeFileSync('codex-broker.json',JSON.stringify({identity:process.env.RUDDER_FABRIC_IDENTITY,socket:process.env.RUDDER_FABRIC_BROKER_SOCKET}));
if(!process.argv.includes('app-server')){console.log('ENGINE_CODEX_READY');setInterval(()=>{},1000);return;}
const socket=process.argv[process.argv.indexOf('--listen')+1].replace('unix://',''),sid='codex-'+process.env.RUDDER_FABRIC_ATTEMPT_ID;
let active='codex-initial-turn';const thread=()=>({id:sid,cwd:process.cwd(),ephemeral:false,source:'cli',status:{type:active?'active':'idle'},turns:active?[{id:active,status:'inProgress'}]:[]});
const server=http.createServer(),wss=new WebSocketServer({server});
wss.on('connection',ws=>ws.on('message',raw=>{const x=JSON.parse(raw);if(!('id'in x))return;let result={};
 if(x.method==='initialize')result={userAgent:'rudder_agent_link/0.166.0'};
 if(x.method==='thread/loaded/list')result={data:[sid],nextCursor:null};
 if(x.method==='thread/read'||x.method==='thread/resume')result={thread:thread()};
 if(x.method==='turn/interrupt'){if(x.params.turnId!==active)throw Error('stale turn');const old=active;active=null;setTimeout(()=>ws.send(JSON.stringify({method:'turn/completed',params:{threadId:sid,turn:{id:old,status:'interrupted'}}})),15);}
 if(x.method==='turn/start'||x.method==='turn/steer'){
  if(!x.params.additionalContext||Object.values(x.params.additionalContext).some(entry=>entry.kind!=='untrusted'||!/^[a-z][a-z0-9_]{0,63}$/.test(entry.name)))throw Error('invalid untrusted context');
  fs.appendFileSync('codex-peer-calls.ndjson',JSON.stringify({method:x.method,params:x.params})+'\\n');
  if(x.method==='turn/steer'&&x.params.expectedTurnId!==active)throw Error('missing expected turn');
  if(x.method==='turn/start'){active='codex-resumed-turn';const turn=active;result={turn:{id:turn,status:'inProgress'}};ws.send(JSON.stringify({method:'turn/started',params:{threadId:sid,turn:{id:turn,status:'inProgress'}}}));setTimeout(()=>{active=null;ws.send(JSON.stringify({method:'turn/completed',params:{threadId:sid,turn:{id:turn,status:'completed'}}}));},100);}else result={turnId:active};
 }
 ws.send(JSON.stringify({jsonrpc:'2.0',id:x.id,result}));
}));server.listen(socket);
`,{mode:0o755});
  const env={...process.env,RUDDER_HOME:home,RUDDER_CLI:path.join(repoRoot,'dist/index.js'),RUDDER_CLAUDE_BIN:claude,RUDDER_CODEX_BIN:codex,RUDDER_CLAUDE_EXECUTION_PROFILE:'sdk',RUDDER_TASK_ROUTING:'local',RUDDER_OFFLINE:'1',RUDDER_TELEMETRY:'0',RUDDER_DISABLE_AUTO_UPDATE:'1',RUDDER_DISABLE_UPDATE_CHECK:'1',RUDDER_SKIP_AUTO_UPDATE:'1',TERM:'xterm-256color'};
  for(const key of ['RUDDER_ENGINE','RUDDER_ATTACH_SOCKET','RUDDER_CLOUD_WORKER','RUDDER_CLOUD_RUNTIME','RUDDER_WORKSPACE_ID','RUDDER_WORKER_TOKEN','RUDDER_SAIL_ID','RUDDER_FABRIC_IDENTITY','RUDDER_FABRIC_ATTEMPT_ID'])delete env[key];
  const children=[];let view;
  t.after(async()=>{if(view)await view.close().catch(()=>{});try{execFileSync(nativeBin,['engine','shutdown','--repo',dir,'--json'],{env,cwd:dir,timeout:8000,stdio:'pipe'});}catch{}for(const child of children){child.stdin?.end();if(child.exitCode===null&&child.signalCode===null)child.kill('SIGTERM');}await removeScratch(dir);});
  const engine=spawn(nativeBin,['engine','--repo',dir],{env,cwd:dir,stdio:['ignore','pipe','pipe']});children.push(engine);let stderr='';engine.stderr.on('data',x=>{stderr=(stderr+x).slice(-16384);});
  await new Promise((resolve,reject)=>{let out='';const timer=setTimeout(()=>reject(Error('engine startup '+stderr)),10000);engine.stdout.on('data',x=>{out+=x;if(out.includes('\n')){clearTimeout(timer);try{assert.equal(JSON.parse(out.split('\n')[0]).running,true);resolve();}catch(e){reject(e);}}});engine.once('exit',code=>{clearTimeout(timer);reject(Error('engine startup exited '+code+' '+stderr));});});
  view=await launch({binary:nativeBin,args:['attach','--dashboard'],env,cwd:dir,cols:140,rows:42});await view.waitForText('Type a task',{timeout:15000});
  const input=async text=>{await view.press('Ctrl+W');await view.press('3');await view.type(text);if(text.startsWith('/model '))await view.press('Escape');await view.press('Enter');};
  async function records(){const rows=[];for(const id of await fs.readdir(path.join(dir,'.rudder/runs')).catch(()=>[])){try{rows.push(JSON.parse(await fs.readFile(path.join(dir,'.rudder/runs',id,'run.json'),'utf8')));}catch{}}return rows;}
  async function identity(run){let file;await waitUntil(async()=>{for(const name of await fs.readdir(path.join(home,'fabric/identities')).catch(()=>[])){try{const candidate=path.join(home,'fabric/identities',name),value=JSON.parse(await fs.readFile(candidate,'utf8'));if(value.run_id===run&&value.token){file=candidate;return true;}}catch{}}return false;},{timeout:10000,label:'run identity'});const own=JSON.parse(await fs.readFile(file,'utf8'));for(const backend of ['claude','codex']){const endpoint=JSON.parse(await fs.readFile(path.join(dir,backend+'-broker.json'),'utf8'));const projected=JSON.parse(await fs.readFile(endpoint.identity,'utf8'));if(projected.run_id===own.run_id){assert(endpoint.socket);assert(!endpoint.identity.includes('journal'));return endpoint;}}assert.fail('missing own projected broker endpoint');}
  function mcp(file){const child=spawn(nativeBin,['fabric','mcp','--identity-file',file.identity,'--broker-socket',file.socket],{env,cwd:dir,stdio:['pipe','pipe','pipe']});children.push(child);child.stderr.resume();let next=0;const pending=new Map();readline.createInterface({input:child.stdout}).on('line',raw=>{const x=JSON.parse(raw);pending.get(x.id)?.(x);pending.delete(x.id);});async function rpc(method,params){const id=++next,promise=new Promise((resolve,reject)=>{const timer=setTimeout(()=>{pending.delete(id);reject(Error('MCP timeout '+method));},10000);pending.set(id,x=>{clearTimeout(timer);resolve(x);});});child.stdin.write(JSON.stringify({jsonrpc:'2.0',id,method,params})+'\n');return promise;}return{async init(){await rpc('initialize',{protocolVersion:'2025-11-25',capabilities:{},clientInfo:{name:'engine-fixture',version:'1'}});child.stdin.write(JSON.stringify({jsonrpc:'2.0',method:'notifications/initialized'})+'\n');},async call(name,args={}){return rpc('tools/call',{name,arguments:args});},async tool(name,args={}){const answer=await rpc('tools/call',{name,arguments:args});assert.equal(answer.result?.isError,false,JSON.stringify(answer));return answer.result.structuredContent;}};}
  return{dir,home,env,engine,view,input,records,identity,mcp};
}

// This preserves real renderer/process ownership and uses typed provider
// observations instead of driving lifecycle through terminal text.
test('one engine owns both managed backends, pins live controls and retains peer attribution', {timeout:90000},async t=>{
 const f=await fixture(t);if(!f)return;
 await f.input('/model claude haiku');await f.input('/main fixture Claude conversation');await f.view.waitForText('ENGINE_SDK_READY',{timeout:15000});
 let claude;await waitUntil(async()=>Boolean(claude=(await f.records()).find(r=>r.backend==='claude'&&r.task==='fixture Claude conversation')),{timeout:10000,label:'Claude run'});
 await f.input('/model codex gpt-6.1-sol low');await f.input('/main fixture Codex conversation');await f.view.waitForText('ENGINE_CODEX_READY',{timeout:15000});
 let codex;await waitUntil(async()=>Boolean(codex=(await f.records()).find(r=>r.backend==='codex'&&r.task==='fixture Codex conversation')),{timeout:10000,label:'Codex run'});
 const cm=f.mcp(await f.identity(claude.id)),xm=f.mcp(await f.identity(codex.id));await cm.init();await xm.init();
 async function working(client,id){let found;await waitUntil(async()=>{found=(await client.tool('agents_find')).agents.find(a=>a.run_id===id);return found?.runtime?.phase==='working'&&found.runtime.capabilities.includes('interrupt');},{timeout:10000,label:'current qualified turn'});return found;}
 const ct=await working(xm,claude.id),xt=await working(cm,codex.id);
 assert.equal(ct.runtime.execution_profile,'claude-sdk');assert.equal(xt.runtime.execution_profile,'codex-app-server');
 const base=(target)=>({target_run:target.run_id,target_epoch:target.run_epoch,scope:'project',expected_thread_id:target.runtime.thread_id,expected_turn_id:target.runtime.active_turn_id,reason:'owned protocol fixture'});
 // Claude replacement and interrupted continuation remain unavailable until
 // tool/child reconciliation exists. An authenticated peer must not bypass it.
 assert(!ct.runtime.capabilities.includes('steer'));
 assert(xt.runtime.capabilities.includes('steer'));
 async function refused(client,name,args){
  const answer=await client.call(name,args);
  assert.equal(answer.result?.isError,true,JSON.stringify(answer));
  assert.equal(answer.result.structuredContent,undefined);
  assert.match(answer.result.content[0].text,/agent broker rejected the request/);
 }
 await refused(xm,'agents_steer',{...base(ct),idempotency_key:'unsupported-replacement',instruction:'must not replace Claude work'});
 // Active append uses the same live Codex turn and preserves peer attribution.
 const instruction='consider attributed peer data without replacing the active turn';
 const steer=await cm.tool('agents_steer',{...base(xt),idempotency_key:'steer',instruction});
 const sr=await cm.tool('agents_await',{command_id:steer.receipt.command_id,timeout_ms:5000});
 assert.equal(sr.receipt.state,'completed');assert.equal(sr.receipt.result.adapter.status,'transport_accepted');
 const calls=(await fs.readFile(path.join(f.dir,'codex-peer-calls.ndjson'),'utf8')).trim().split('\n').map(JSON.parse);
 assert.equal(calls.length,1);assert.equal(calls[0].method,'turn/steer');
 const params=calls[0].params;
 assert.equal(params.threadId,xt.runtime.thread_id);assert.equal(params.expectedTurnId,xt.runtime.active_turn_id);
 const context=Object.values(params.additionalContext);assert.equal(context.length,1);assert.equal(context[0].kind,'untrusted');
 const message=JSON.parse(context[0].value);assert.equal(message.sender_run_id,claude.id);
 const attributed=JSON.parse(message.text);assert.equal(attributed.instruction,instruction);
 assert.equal(attributed.principal,'agent:'+claude.id);assert.equal(attributed.sender_epoch,ct.run_epoch);
 assert(!JSON.stringify(params.input).includes(instruction),'peer text is separate from fixed application routing notice');
 const stillWorking=await working(cm,codex.id);assert.equal(stillWorking.runtime.active_turn_id,xt.runtime.active_turn_id);
 for(const [client,target] of [[xm,ct],[cm,xt]]){
  const interrupt=await client.tool('agents_interrupt',{...base(target),idempotency_key:'interrupt-'+target.run_id});
  const ir=await client.tool('agents_await',{command_id:interrupt.receipt.command_id,timeout_ms:5000});
  assert.equal(ir.receipt.state,'completed');assert.equal(ir.receipt.result.adapter.status,'interrupt_confirmed');
  await waitUntil(async()=>(await f.records()).find(r=>r.id===target.run_id)?.status==='paused',{timeout:10000,label:'typed interruption pauses '+target.run_id});
  const observed=(await client.tool('agents_find')).agents.find(a=>a.run_id===target.run_id);
  assert.equal(observed.runtime.phase,'interrupted');assert.equal(observed.runtime.active_turn_id,null);
  assert.equal(observed.runtime.last_interrupted_turn_id,target.runtime.active_turn_id);
  assert(!observed.runtime.capabilities.includes('resume'));
  await refused(client,'agents_resume',{...base(target),idempotency_key:'unsupported-resume-'+target.run_id,instruction:'must not start a replacement turn'});
 }
 // Check actual provider inputs after all replies, not merely the error flag.
 const frames=(await fs.readFile(path.join(f.dir,'provider-user-frames.ndjson'),'utf8')).trim().split('\n').map(JSON.parse);
 assert.equal(frames.length,1,'Claude replacement/resume wrote no user input');
 const controls=(await fs.readFile(path.join(f.dir,'provider-control-frames.ndjson'),'utf8')).trim().split('\n').map(JSON.parse);
 assert.equal(controls.length,1);assert.equal(controls[0].subtype,'interrupt');assert.equal(controls[0].cancel_queued,true);
 assert.equal(await fs.readFile(path.join(f.dir,'codex-peer-calls.ndjson'),'utf8'),calls.map(x=>JSON.stringify(x)).join('\n')+'\n','resume wrote no Codex turn/start');
 const latest=await cm.tool('agents_find');assert.equal(latest.agents.find(a=>a.run_id===codex.id).runtime.thread_id,xt.runtime.thread_id);
});
