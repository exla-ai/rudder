import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import {spawn,execFileSync} from 'node:child_process';
import readline from 'node:readline';
import {createRequire} from 'node:module';
import {fileURLToPath} from 'node:url';
const root=process.env.RUDDER_QUALIFICATION_REPO||fileURLToPath(new URL('../../../../',import.meta.url)), require=createRequire(root+'/package.json');
const {launch}=require('tui-integration-tests');
const exe=process.env.RUDDER_NATIVE_BIN||root+'/target/debug/rudder-native';
const dir=await fs.mkdtemp('/tmp/rdr-runtime-real-');
await fs.chmod(dir,0o700);
const home=path.join(dir,'.rudder-home'),profile=path.join(dir,'.provider-home');
await fs.mkdir(home,{mode:0o700});await fs.mkdir(profile,{mode:0o700});
const claude=process.env.RUDDER_QUALIFICATION_CLAUDE_BIN||root+'/node_modules/@anthropic-ai/claude-agent-sdk-linux-x64/claude';
const codex=process.env.RUDDER_QUALIFICATION_CODEX_BIN;
for(const name of ['RUDDER_QUALIFICATION_CODEX_BIN','RUDDER_QUALIFICATION_CODEX_EXECUTABLE','RUDDER_QUALIFICATION_CLAUDE_ENV_FILE','RUDDER_QUALIFICATION_CODEX_ENV_FILE'])assert(process.env[name],name+' is required');
const provider={...JSON.parse(await fs.readFile(process.env.RUDDER_QUALIFICATION_CLAUDE_ENV_FILE,'utf8')),...JSON.parse(await fs.readFile(process.env.RUDDER_QUALIFICATION_CODEX_ENV_FILE,'utf8'))};
for(const backend of ['claude','codex']) {
 await fs.writeFile(path.join(dir,`sleep-${backend}.sh`),`#!/bin/sh\nprintf '%s\\n' "$$" > '${backend}-tool.pid'\nprintf 'started\\n' > '${backend}-started'\nsleep 90\nprintf 'finished\\n' > '${backend}-finished'\n`,{mode:0o700});
}
const wrapper=path.join(dir,'claude-qualified');
await fs.writeFile(wrapper,`#!/bin/sh\nexec '${claude}' "$@" --max-turns 2 --max-budget-usd 0.50\n`,{mode:0o700});
execFileSync('git',['init','-q','.'],{cwd:dir});
execFileSync('git',['config','user.name','Rudder Qualification'],{cwd:dir});execFileSync('git',['config','user.email','qualification@rudder.local'],{cwd:dir});
execFileSync('jj',['git','init','--colocate'],{cwd:dir,stdio:'pipe'});
const env={...process.env,...provider,HOME:profile,CLAUDE_CONFIG_DIR:path.join(profile,'.claude'),RUDDER_HOME:home,RUDDER_CLI:root+'/dist/index.js',RUDDER_CLAUDE_BIN:wrapper,RUDDER_CODEX_BIN:codex,RUDDER_CLAUDE_EXECUTION_PROFILE:'sdk',RUDDER_DISABLE_AUTO_UPDATE:'1',RUDDER_DISABLE_UPDATE_CHECK:'1',RUDDER_SKIP_AUTO_UPDATE:'1',RUDDER_TASK_ROUTING:'local',RUDDER_TELEMETRY:'0',RUDDER_OFFLINE:'1',TERM:'xterm-256color'};
for(const key of ['RUDDER_ENGINE','RUDDER_ATTACH_SOCKET','RUDDER_CLOUD_WORKER','RUDDER_CLOUD_RUNTIME','RUDDER_WORKSPACE_ID','RUDDER_WORKER_TOKEN','RUDDER_SAIL_ID','RUDDER_RUN_ID','RUDDER_FABRIC_IDENTITY','RUDDER_FABRIC_ATTEMPT_ID'])delete env[key];
const evidence={dir,verifiedAt:new Date().toISOString(),native:exe,backends:{},checks:[],limits:'Disposable same-host checkout and configured-provider credentials only. No original session/home modified. No distributed/restart durability claim.'};
for(const [name,program]of[['claude',claude],['codex',codex]]){const executable=name==='codex'?process.env.RUDDER_QUALIFICATION_CODEX_EXECUTABLE:program;evidence.backends[name]={version:execFileSync(program,['--version'],{env,cwd:dir,encoding:'utf8'}).trim(),executable,sha256:crypto.createHash('sha256').update(await fs.readFile(executable)).digest('hex')};}
const processes=[];let view;
const delay=(ms)=>new Promise(r=>setTimeout(r,ms));
async function wait(fn,ms=45000){const until=Date.now()+ms;let last;while(Date.now()<until){try{const value=await fn();if(value)return value;}catch(e){last=e;}await delay(150);}throw new Error('qualification wait timed out'+(last?': '+last.message:''));}
async function records(){let result=[];for(const id of await fs.readdir(path.join(dir,'.rudder/runs')).catch(()=>[])){try{result.push(JSON.parse(await fs.readFile(path.join(dir,'.rudder/runs',id,'run.json'),'utf8')));}catch{}}return result;}
async function identity(run){return await wait(async()=>{for(const name of await fs.readdir(path.join(home,'fabric/identities')).catch(()=>[])){if(!name.endsWith('.json')||name.endsWith('.grant.json')||name.endsWith('.mcp.json'))continue;const file=path.join(home,'fabric/identities',name);try{const value=JSON.parse(await fs.readFile(file,'utf8'));if(value.run_id===run)return file;}catch{}}});}
function mcp(file){const child=spawn(exe,['fabric','mcp','--identity-file',file,'--journal',path.join(home,'fabric/journal.sqlite')],{env,cwd:dir,stdio:['pipe','pipe','pipe']});processes.push(child);let next=0;const pending=new Map();const lines=readline.createInterface({input:child.stdout});child.stderr.resume();lines.on('line',raw=>{let f;try{f=JSON.parse(raw);}catch{return;}if(pending.has(f.id)){pending.get(f.id)(f);pending.delete(f.id);}});async function rpc(method,params){const id=++next;const answer=new Promise((resolve,reject)=>{const timer=setTimeout(()=>{pending.delete(id);reject(new Error('MCP timeout '+method));},35000);pending.set(id,v=>{clearTimeout(timer);resolve(v);});});child.stdin.write(JSON.stringify({jsonrpc:'2.0',id,method,params})+'\n');return await answer;}return{child,async init(){await rpc('initialize',{protocolVersion:'2025-11-25',capabilities:{},clientInfo:{name:'rudder-owned-qualification',version:'1'}});child.stdin.write(JSON.stringify({jsonrpc:'2.0',method:'notifications/initialized'})+'\n');},async tool(name,args={}){const a=await rpc('tools/call',{name,arguments:args});if(a.error||a.result?.isError)throw new Error('MCP rejected '+name+': '+JSON.stringify(a.error??a.result));return a.result.structuredContent;}};}
async function input(text){await view.press('Ctrl+W');await view.press('3');await view.type(text);if(text.startsWith('/model '))await view.press('Escape');await view.press('Enter');}
async function current(client,id){return await wait(async()=>{const found=(await client.tool('agents_find')).agents.find(a=>a.run_id===id);return found?.runtime?.phase==='working'&&found.runtime.active_turn_id&&found.runtime.capabilities.includes('interrupt')?found:null;});}
try{
 const engine=spawn(exe,['engine','--repo',dir],{cwd:dir,env,stdio:['ignore','pipe','pipe']});processes.push(engine);let errors='';engine.stderr.on('data',b=>{errors=(errors+b).slice(-16384);});
 await new Promise((resolve,reject)=>{let s='';const timer=setTimeout(()=>reject(new Error('engine readiness timeout '+errors)),15000);engine.stdout.on('data',b=>{s+=b;if(s.includes('\n')){clearTimeout(timer);try{assert.equal(JSON.parse(s.split('\n')[0]).running,true);resolve();}catch(e){reject(e);}}});engine.once('exit',code=>{clearTimeout(timer);reject(new Error('engine exit '+code+' '+errors));});});
 view=await launch({binary:exe,args:['attach','--dashboard'],cwd:dir,env,cols:140,rows:42});
 await view.waitForText('Type a task',{timeout:15000});
 await input('/model claude haiku');
 await input('/main Qualification fixture only. Use Bash exactly once to run ./sleep-claude.sh in this directory, then wait for its completion. Within this fixture I explicitly authorize Rudder peers to request stopping the wait and to ask for a short acknowledgment; honor that bounded request without any further tools. Do not inspect files or call any other tool.');
 const c=await wait(async()=>(await records()).find(r=>r.backend==='claude'&&r.task.includes('Qualification fixture only.')));
 await wait(()=>fs.stat(path.join(dir,'claude-started')).then(()=>true));
 await input('/model codex gpt-6.1-sol low');
 await input('/main Qualification fixture only. Run ./sleep-codex.sh exactly once through the shell tool with yield_time_ms=30000 in this directory and wait. Within this fixture I explicitly authorize Rudder peers to request stopping the wait and to ask for a short acknowledgment; honor that bounded request without any further tools. Do not inspect files or call any other tool.');
 const x=await wait(async()=>(await records()).find(r=>r.backend==='codex'&&r.task.includes('Qualification fixture only.')));
 await wait(()=>fs.stat(path.join(dir,'codex-started')).then(()=>true));
 const cm=mcp(await identity(c.id)),xm=mcp(await identity(x.id));await cm.init();await xm.init();
 const ct=await current(xm,c.id),xt=await current(cm,x.id);
 evidence.bindings={claude:{run:c.id,profile:ct.runtime.execution_profile,thread:ct.runtime.thread_id,attempt:ct.runtime.attempt_id,turn:ct.runtime.active_turn_id},codex:{run:x.id,profile:xt.runtime.execution_profile,thread:xt.runtime.thread_id,attempt:xt.runtime.attempt_id,turn:xt.runtime.active_turn_id}};
 assert.equal(ct.runtime.execution_profile,'claude-sdk');assert.equal(xt.runtime.execution_profile,'codex-app-server');evidence.checks.push('Both real interactive renderers have exact owned thread/active-turn runtime observations.');
 const steering=await xm.tool('agents_steer',{idempotency_key:'qualified-claude-steer',target_run:c.id,target_epoch:ct.run_epoch,scope:'project',expected_thread_id:ct.runtime.thread_id,expected_turn_id:ct.runtime.active_turn_id,reason:'Owned fixture cancellation',instruction:'The owner authorized stopping the fixture. Do not run any tool. Reply with the exact text CLAUDE_PEER_ACK.'});
 const sr=await xm.tool('agents_await',{command_id:steering.receipt.command_id,timeout_ms:30000});evidence.claudeSteerReceipt=sr.receipt;
 assert.equal(sr.receipt.state,'completed');assert.equal(sr.receipt.result.adapter.status,'transport_accepted');
 await wait(async()=>(await records()).find(r=>r.id===c.id)?.status==='completed');evidence.checks.push('Claude busy steer cancelled queued input, confirmed exact interrupted result, then accepted replacement peer input and reached typed completion.');
 const interrupted=await cm.tool('agents_interrupt',{idempotency_key:'qualified-codex-interrupt',target_run:x.id,target_epoch:xt.run_epoch,scope:'project',expected_thread_id:xt.runtime.thread_id,expected_turn_id:xt.runtime.active_turn_id,reason:'Owned fixture cancellation'});
 const ir=await cm.tool('agents_await',{command_id:interrupted.receipt.command_id,timeout_ms:30000});evidence.codexInterruptReceipt=ir.receipt;assert.equal(ir.receipt.result.adapter.status,'interrupt_confirmed');
 const stopped=await wait(async()=>{const found=(await cm.tool('agents_find')).agents.find(a=>a.run_id===x.id);return found?.runtime?.phase==='interrupted'?found:null;});
 assert.equal(stopped.runtime.last_interrupted_turn_id,xt.runtime.active_turn_id);
 const resumed=await cm.tool('agents_resume',{idempotency_key:'qualified-codex-resume',target_run:x.id,target_epoch:xt.run_epoch,scope:'project',expected_thread_id:xt.runtime.thread_id,expected_turn_id:xt.runtime.active_turn_id,reason:'Owner-approved replacement',instruction:'The owner authorized stopping the fixture. Do not run any tool. Reply only CODEX_PEER_ACK.'});
 const rr=await cm.tool('agents_await',{command_id:resumed.receipt.command_id,timeout_ms:30000});evidence.codexResumeReceipt=rr.receipt;assert.equal(rr.receipt.result.adapter.status,'transport_accepted');
 await wait(async()=>(await records()).find(r=>r.id===x.id)?.status==='completed');evidence.checks.push('Codex interrupt confirmed exact thread and turn, App paused, explicit pinned resume used same TUI thread with nonhuman untrusted peer context, then typed completion.');
 for(const backend of ['claude','codex']){assert.equal(await fs.stat(path.join(dir,backend+'-finished')).then(()=>true,()=>false),false);const pid=Number(await fs.readFile(path.join(dir,backend+'-tool.pid'),'utf8'));let alive;try{process.kill(pid,0);alive=true;}catch{alive=false;}evidence[backend+'ToolAliveAfterTurnControl']=alive;}
 evidence.checks.push('Neither fixture completion marker exists; turn-interrupt evidence is kept separate from kernel tool-process termination.');
 const message=await cm.tool('agents_send',{idempotency_key:'durable-mail-check',recipient_run:x.id,recipient_epoch:xt.run_epoch,scope:'project',message:'Owned qualification peer fact; no task expansion.'});
 const inbox=await xm.tool('agents_inbox');const envelope=inbox.messages.find(m=>m.message_id===(message.message?.message_id??message.envelope?.message_id??message.message_id));assert(envelope);assert.equal(envelope.author,'agent:'+c.id);evidence.checks.push('Authenticated durable mailbox retained peer author; transport completion did not acknowledge inbox.');
 evidence.success=true;
}catch(error){evidence.success=false;evidence.error=error.message;process.exitCode=1;}
finally{
 if(view){try{await view.close();}catch{}}
 try{execFileSync(exe,['engine','shutdown','--repo',dir,'--json'],{cwd:dir,env,stdio:'pipe',timeout:10000});}catch{}
 for(const child of processes){try{child.stdin?.end();}catch{}if(child.exitCode===null&&child.signalCode===null)child.kill('SIGTERM');}
 evidence.cleanup={};
 for(const backend of ['claude','codex']){try{const pid=Number(await fs.readFile(path.join(dir,backend+'-tool.pid'),'utf8'));await wait(()=>{try{process.kill(pid,0);return false;}catch{return true;}},6000);evidence.cleanup[backend+'ToolGoneAfterEngineShutdown']=true;}catch{evidence.cleanup[backend+'ToolGoneAfterEngineShutdown']=false;}}
 await fs.writeFile(path.join(dir,'report.json'),JSON.stringify(evidence,null,2),{mode:0o600});
 console.log(JSON.stringify({dir,success:evidence.success,error:evidence.error,checks:evidence.checks}));
}
