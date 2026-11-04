import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { registerHooks } from 'node:module';
import { EventEmitter, once } from 'node:events';
import { fork } from 'node:child_process';
import { createServer } from 'node:http';
import ts from 'typescript';

const root=fileURLToPath(new URL('../',import.meta.url));
registerHooks({resolve(specifier,context,next) {
  if(specifier==='/app/terminal-recovery.js') return next(pathToFileURL(path.join(root,'cloud/src/private-access/terminal-recovery.ts')).href,context);
  if(specifier.startsWith('.') && specifier.endsWith('.js') && context.parentURL) {
    const url=new URL(specifier.slice(0,-3)+'.ts',context.parentURL);
    if(url.protocol==='file:' && fileURLToPath(url).startsWith(root) && existsSync(url)) return next(url.href,context);
  }
  return next(specifier,context);
},load(url,context,next) {
  if(url.startsWith(pathToFileURL(root).href) && url.endsWith('.ts')) return {format:'module',shortCircuit:true,
    source:ts.transpileModule(readFileSync(fileURLToPath(url),'utf8'),{compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.ES2022}}).outputText};
  return next(url,context);
}});
const {HomePrivatePreviews}=await import('../src/private-access/home.ts');
const {PrivatePreviewDirectory,processStartToken}=await import('../src/private-access/previews.ts');
const {PrivatePreviewRoutes}=await import('../cloud/src/private-access/preview-routes.ts');
const {CanonicalTerminalRelay}=await import('../cloud/src/private-access/terminal-relay.ts');
const {CanonicalTerminalView}=await import('../cloud/app/terminal-client.js');
const pause=(ms)=>new Promise(resolve=>setTimeout(resolve,ms));
async function until(check) {for(let i=0;i<80;i++){if(check())return;await pause(5);}assert.fail('bounded condition did not settle');}

// Linux process discovery and HTTP are real. Tailscale's device/Serve commands
// are a local stand-in, never a claim that the provider or ACL gate passed.
test('registered Home preview reaches authorized exact-reference discovery and expires with process custody',{skip:process.platform!=='linux'},async t=>{
  const task='/workspace/rudder-scratch/n13-private-terminal';await fs.mkdir(task,{recursive:true});
  const dir=await fs.mkdtemp(path.join(task,'preview-'));t.after(async()=>{await fs.rm(dir,{recursive:true,force:true});await fs.rmdir(task).catch(()=>{});});
  const childFile=path.join(dir,'listener.cjs');await fs.writeFile(childFile,"const s=require('node:http').createServer((q,r)=>r.end('preview'));s.listen(0,'127.0.0.1',()=>process.send({port:s.address().port}));");
  const child=fork(childFile,{cwd:dir,stdio:['ignore','ignore','ignore','ipc'],execArgv:[]});
  t.after(async()=>{if(child.exitCode===null){child.kill();await once(child,'exit');}});
  const [{port}]=await once(child,'message');const start=await processStartToken(child.pid);assert.match(start,/^\d+$/);
  const scope={projectId:'engine-project',workspaceId:'run-1',attemptId:'pty-attempt-1',ownerEpoch:7};
  const record={version:1,role:'preview',action:'register',scope,owner:{project_id:scope.projectId,owner_id:'app-owner',epoch:7},workspacePath:dir,rootPid:process.pid,pid:child.pid,processStart:start,protocol:'http'};
  let current=true,config={},served=0,retired=0,revokeOnProbe=false;
  let home;
  const directory=new PrivatePreviewDirectory({timeoutMs:500,processMatches:item=>home.processMatches(item),
    async execute(args){
      if(args[0]==='status')return {code:0,stdout:JSON.stringify({BackendState:'Running',Self:{ID:'device-1',Online:true,DNSName:'rudder.tail-test.ts.net.'}})};
      if(args[0]==='serve' && args[1]==='status')return {code:0,stdout:JSON.stringify(config)};
      if(args.includes('--bg')) {served++;config={TCP:{[port]:{HTTPS:true}},Web:{[`rudder.tail-test.ts.net:${port}`]:{Handlers:{'/':{Proxy:`http://127.0.0.1:${port}/`}}}}};}
      if(args.includes('off')){retired++;config={};}
      return {code:0,stdout:''};
    },
    async probe(url){if(revokeOnProbe)current=false;const target=url.startsWith('https://rudder.tail-test.ts.net:')?`http://127.0.0.1:${port}/`:url;return (await fetch(target,{signal:AbortSignal.timeout(500)})).ok;},
  });
  home=new HomePrivatePreviews('authenticated-home',directory,async(repo,operation,request)=>{
    assert.equal(repo,dir);if(operation==='preview-register'){assert.equal(request.role,undefined);return {receipt:true};}
    return current?[record]:[];
  });
  await home.reconcile([{workspaceId:'cloud-repository',dir}]);
  const rows=home.snapshot('cloud-repository');assert.equal(rows.length,1);assert.equal(served,1);assert.equal(rows[0].pid,child.pid);
  assert.deepEqual(home.snapshot('another-cloud-repository'),[]);
  let allowed=true,custodyChange=false;
  const relay=new PrivatePreviewRoutes({workspace(actor,workspace){assert.equal(actor,'account');assert.equal(workspace,'cloud-repository');if(!allowed)throw new Error('ACL denied');return {homeId:'authenticated-home'};},
    async rpc(homeId,method){assert.equal(homeId,'authenticated-home');assert.equal(method,'previews');if(custodyChange)allowed=false;return home.snapshot('cloud-repository');}});
  const discovered=await relay.list('account','cloud-repository');assert.equal(discovered[0].id,rows[0].id);
  assert.equal((await relay.resolve('account','cloud-repository',rows[0].id)).url,rows[0].url);
  await assert.rejects(relay.resolve('account','cloud-repository',port.toString()),/exact private preview reference/);
  custodyChange=true;await assert.rejects(relay.resolve('account','cloud-repository',rows[0].id),/ACL denied/);allowed=true;custodyChange=false;
  // A listening process in the same cwd is not enough once the native role is gone.
  current=false;await home.reconcile([{workspaceId:'cloud-repository',dir}]);assert.deepEqual(home.snapshot('cloud-repository'),[]);assert.ok(retired>0);
  await assert.rejects(relay.resolve('account','cloud-repository',rows[0].id),/no longer ready/);
  current=true;revokeOnProbe=true;await home.reconcile([{workspaceId:'cloud-repository',dir}]);assert.deepEqual(home.snapshot('cloud-repository'),[],'owner role lost during readiness cannot publish');revokeOnProbe=false;
  current=true;record.processStart='0';await home.reconcile([{workspaceId:'cloud-repository',dir}]);assert.deepEqual(home.snapshot('cloud-repository'),[]);
});

test('Cloud preview ACL and tenant come from the selected authenticated Home',async()=>{
  const row={tenantId:'other-home',projectId:'p',workspaceId:'w',attemptId:'a',id:'a'.repeat(64),deviceId:'d',dnsName:'rudder.tail-test.ts.net',url:'https://rudder.tail-test.ts.net:3000/',port:3000,pid:10,processStart:'1',protocol:'http',ready:true};
  const routes=new PrivatePreviewRoutes({workspace:()=>({homeId:'current-home'}),rpc:async()=>[row]});
  assert.deepEqual(await routes.list('account','cloud-repository'),[]);
  await assert.rejects(routes.resolve('account','cloud-repository',row.id),/no longer ready/);
});

class ServerSocket extends EventEmitter {
  readyState=1;bufferedAmount=0;frames=[];
  send(data,callback){this.frames.push(JSON.parse(data));queueMicrotask(()=>{if(this.readyState===1)this.client.onmessage?.({data});callback();});}
  close(){if(this.readyState!==1)return;this.readyState=3;this.emit('close');this.client.readyState=3;this.client.onclose?.();}
}
function pair(){const server=new ServerSocket();const client={readyState:1,send:data=>server.emit('message',data,false),close:()=>server.close()};server.client=client;return {server,client};}
const canonical=(sequence=1,epoch=7)=>({version:1,type:'terminal-screen',generation:'pty-attempt-1',sequence,
 owner:{project_id:'engine-project',owner_id:'current-owner',epoch},text:`full screen ${sequence}`,
 screen:{format:'vt100-ansi-v1',columns:80,rows:24,screen:`\x1bcfull screen ${sequence}`,scrollback:['retained history']}});

test('negotiated current-run canonical browser paints release only current screen slots and reconnect resets',async t=>{
  let current=canonical(),allowed=true;const paints=[],sockets=[],selection={actor:'account',workspaceId:'cloud-repository',runId:'run-1'};
  const relay=new CanonicalTerminalRelay({authorize:async()=>allowed,read:async()=>current,pollMs:5});t.after(()=>relay.close());
  const element={textContent:'',scrollTop:4,scrollLeft:2};let capabilities=0,fallback=0;
  const view=new CanonicalTerminalView(element,{workspaceId:selection.workspaceId,runId:selection.runId,origin:'https://rudder.test',afterPaint:fn=>paints.push(fn),
    onCapability:mode=>{assert.equal(mode,'read-only');capabilities++;},onUnavailable:()=>fallback++,socket(url){
      assert.equal(new URL(url).searchParams.get('capability'),'canonical-screen-text-v1');
      const {server,client}=pair();sockets.push(server);queueMicrotask(()=>relay.attach(selection,'viewer',server).catch(()=>server.close()));return client;
    }});t.after(()=>view.stop());view.start();
  await until(()=>paints.length===1);assert.equal(element.textContent,'full screen 1');assert.equal(capabilities,1);
  current=canonical(2);await pause(25);assert.equal(sockets[0].frames.filter(f=>f.type==='terminal-screen').length,1,'unpainted frame holds its viewer slot');
  paints.shift()();await until(()=>paints.length===1 && element.textContent==='full screen 2');
  assert.equal(element.scrollTop,4);assert.equal(element.scrollLeft,2);paints.shift()();
  const oldGeneration=sockets[0].frames[0].generation;
  view.stop();view.start();await until(()=>capabilities===2 && paints.length===1);
  assert.notEqual(sockets[1].frames[0].generation,oldGeneration);assert.equal(element.textContent,'full screen 2');
  paints.shift()();allowed=false;await until(()=>sockets[1].readyState===3);assert.equal(fallback,0);
  // No input or predictive write is available from this read-only capability.
  assert.equal(view.recovery.input(1,new Uint8Array([65]),'unsafe'),false);
});

test('owner epoch replacement closes the negotiated current-run view',async t=>{
  const {server}=pair();let frame=canonical();
  const relay=new CanonicalTerminalRelay({authorize:async()=>true,read:async()=>frame,pollMs:5});t.after(()=>relay.close());
  await relay.attach({actor:'account',workspaceId:'repo',runId:'run'},'viewer',server);
  frame=canonical(2,8);await until(()=>server.readyState===3);assert.equal(relay.activeViews,0);
});

const {runPrivatePreviewCommand}=await import('../src/private-access/cli.ts');
function cliFixture() {
  const row={tenantId:'home',projectId:'project',workspaceId:'run',attemptId:'attempt',id:'a'.repeat(64),deviceId:'device',dnsName:'rudder.tail-test.ts.net',url:'https://rudder.tail-test.ts.net:3000/',port:3000,pid:10,processStart:'42',protocol:'http',ready:true};
  const events=[];let resolves=0;let changed=false,revoked=false;
  const deps={project:{repo:'owner/repo'},client:{async request(route){events.push(route);
    if(route==='/api/rudder/home')return {projects:[{workspaceId:'cloud-repository',repo:'owner/repo'},{workspaceId:'other',repo:'owner/another'}]};
    if(route.startsWith('/api/app/previews?'))return {version:1,previews:[row]};
    resolves++;if(revoked)throw new Error('Project ACL revoked');
    return {version:1,preview:changed && resolves>1?{...row,deviceId:'replacement-device'}:row};
  }},async localStatus(){events.push('local-status');return {BackendState:'Running',Self:{Online:true,DNSName:'laptop.tail-test.ts.net',ID:'laptop'},Peer:{device:{Online:true,DNSName:row.dnsName,ID:row.deviceId}}};},
    async reachable(url){events.push('probe');return true;},open(url){events.push(`open:${url}`);},write(text){events.push(`print:${text}`);}};
  return {row,deps,events,change:()=>{changed=true;},revoke:()=>{revoked=true;}};
}
test('production Cloud preview CLI opens only an exact re-resolved id after local same-device readiness',async()=>{
  const {row,deps,events}=cliFixture();await runPrivatePreviewCommand([row.id],{},deps);
  assert.equal(events.filter(e=>e.startsWith('/api/app/previews/open')).length,2);
  assert.ok(events.indexOf('probe')<events.findLastIndex(e=>e.startsWith('/api/app/previews/open')));
  assert.ok(events.findLastIndex(e=>e==='local-status')<events.findIndex(e=>e.startsWith('open:')));
  assert.equal(events.find(e=>e.startsWith('open:')),`open:${row.url}`);
  await assert.rejects(runPrivatePreviewCommand(['3000'],{},deps),/Usage:/);
});
test('Cloud preview CLI refuses changed device, revoked ACL, and unreachable route without opening',async()=>{
  for(const failure of ['device','acl','route']) {
    const {row,deps,events,change,revoke}=cliFixture();
    deps.reachable=async()=>{if(failure==='device')change();if(failure==='acl')revoke();return failure!=='route';};
    await assert.rejects(runPrivatePreviewCommand([row.id],{},deps),/binding changed|ACL revoked|not reachable/);
    assert.equal(events.some(e=>e.startsWith('open:')),false);
  }
});
test('Cloud preview CLI lists fresh references without opening a guessed URL',async()=>{
  const {row,deps,events}=cliFixture();await runPrivatePreviewCommand([],{},deps);
  assert.ok(events.includes(`print:${row.id} :3000 ${row.url}`));
  assert.equal(events.some(e=>e.startsWith('open:')),false);
  assert.equal(events.includes('local-status'),false);
});


test('authenticated preview HTTP routes pass only explicit registration requests to the selected Home',async t=>{
  const calls=[];
  const routes=new PrivatePreviewRoutes({workspace(actor,workspace){assert.equal(actor,'account');assert.equal(workspace,'cloud-repository');return {homeId:'authenticated-home'};},
    async rpc(home,method,params){calls.push({home,method,params});return method==='previewRegister'?{registration:'receipted'}:[];}});
  const server=createServer((req,res)=>{void routes.handle(req,res,new URL(req.url,'http://localhost'),'account');});
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));t.after(()=>new Promise(resolve=>server.close(resolve)));
  const base=`http://127.0.0.1:${server.address().port}`;
  const request={version:1,requestId:'f28bd6c1-c9ec-4eeb-9b12-23ba18c0939e',action:'register',scope:{projectId:'p',workspaceId:'w',attemptId:'a',ownerEpoch:7},pid:10,processStart:'42',protocol:'http'};
  let response=await fetch(base+'/api/app/previews/register?workspace=cloud-repository',{method:'POST',body:JSON.stringify(request)});
  assert.equal(response.status,200);assert.equal((await response.json()).preview.registration,'receipted');
  assert.deepEqual(calls[0],{home:'authenticated-home',method:'previewRegister',params:{workspaceId:'cloud-repository',request}});
  response=await fetch(base+'/api/app/previews?workspace=cloud-repository');assert.equal(response.status,200);assert.deepEqual((await response.json()).previews,[]);
  response=await fetch(base+'/api/app/previews/open?workspace=cloud-repository&id=3000');assert.equal(response.status,400);
  response=await fetch(base+'/api/app/previews/register?workspace=cloud-repository',{method:'POST',body:'x'.repeat(4097)});assert.equal(response.status,413);
  response=await fetch(base+'/api/app/previews?workspace=cloud-repository&tenant=forged');assert.equal(response.status,400);
});
